//! Retiring an inline projection the code no longer registers takes its registry row and
//! its applied marks (funkode-io/replay#338).
//!
//! The consumer's shape: a maintenance guard was an inline projection, then moved into
//! command middleware, and the old registration had nowhere to go. Every test asserts what
//! an operator can see — the registry, the marks, what a replica of the previous release
//! can still do — never the statements the retirement ran.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use sqlx::{postgres::PgPoolOptions, PgPool};

use replay_macros::define_aggregate;
use replay_persistence::{
    EventStore, InlineProjection, PersistedEvent, PostgresEventStore, RetiredProjection,
};

use crate::common;
use common::postgres_image::{postgres_database, TestDatabase};

define_aggregate! {
    Gauge {
        namespace: "gauge",
        state: {
            reading: i64,
        },
        commands: {
            Read { value: i64 },
        },
        events: {
            Measured { value: i64 },
        }
    }
}

impl replay::EventStream for Gauge {
    type Event = GaugeEvent;

    fn stream_type() -> String {
        "Gauge".to_string()
    }

    fn apply(&mut self, event: Self::Event) {
        match event {
            GaugeEvent::Measured { value } => self.reading = value,
        }
    }
}

/// A projection with no view of its own, counting what the store asks of it.
struct Guard {
    name: &'static str,
    version: i32,
    inits: Arc<AtomicUsize>,
}

impl Guard {
    fn new(name: &'static str, version: i32) -> Self {
        Self {
            name,
            version,
            inits: Arc::default(),
        }
    }
}

impl InlineProjection for Guard {
    type Exec = sqlx::PgConnection;
    type Event = GaugeEvent;

    fn name(&self) -> &str {
        self.name
    }

    fn version(&self) -> i32 {
        self.version
    }

    async fn init(&mut self, _conn: &mut Self::Exec) -> Result<(), replay::Error> {
        self.inits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn reset(&mut self, _conn: &mut Self::Exec) -> Result<(), replay::Error> {
        Ok(())
    }

    async fn handle(
        &mut self,
        _conn: &mut Self::Exec,
        _events: &[PersistedEvent<Self::Event>],
    ) -> Result<(), replay::Error> {
        Ok(())
    }
}

async fn start_pool() -> (PgPool, TestDatabase) {
    let container = postgres_database().await;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&container.url())
        .await
        .expect("Failed to connect to Postgres");
    common::migrations::MIGRATOR
        .run(&pool)
        .await
        .expect("Failed to run migrations");
    (pool, container)
}

/// A store of the previous release: it still registers the guard.
async fn previous_release(pool: &PgPool, guard: Guard) -> PostgresEventStore {
    PostgresEventStore::builder(pool.clone())
        .projection_catch_up_interval(Duration::ZERO)
        .register(guard)
        .build()
        .await
        .expect("build must succeed")
}

/// A store of the new release: the guard is middleware now, nothing is registered.
async fn new_release(pool: &PgPool) -> PostgresEventStore {
    PostgresEventStore::builder(pool.clone())
        .projection_catch_up_interval(Duration::ZERO)
        .stream_lock_wait(Duration::from_secs(2))
        .build()
        .await
        .expect("build must succeed")
}

/// Append one measurement to `gauge`.
async fn measure(store: &PostgresEventStore, gauge: &str, value: i64) {
    store
        .store_events_stream::<Gauge, _, _, _>(
            &GaugeUrn::new(gauge).unwrap(),
            "Gauge".to_string(),
            replay::Metadata::default(),
            futures::stream::iter([Ok(GaugeEvent::Measured { value })]),
            None,
            |_: &PersistedEvent<GaugeEvent>| {},
            replay_persistence::OpenGate,
        )
        .await
        .expect("append must succeed");
}

/// The registry, by name.
async fn registry(pool: &PgPool) -> Vec<(String, i32)> {
    sqlx::query_as("SELECT name, version FROM projections ORDER BY name")
        .fetch_all(pool)
        .await
        .expect("reading the registry must succeed")
}

/// How many applied marks `name` has, at any version.
async fn marks(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM projection_applied_transactions WHERE projection = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("counting the marks must succeed")
}

/// The guard's row and marks go together, and a second retirement finds nothing: the call
/// can run at every startup.
#[tokio::test]
async fn retiring_takes_the_registry_row_and_its_marks_and_is_idempotent_postgres_test() {
    let (pool, _container) = start_pool().await;

    let old = previous_release(&pool, Guard::new("maintenance_guard", 1)).await;
    measure(&old, "boiler", 7).await;
    measure(&old, "boiler", 9).await;
    drop(old);
    assert_eq!(
        registry(&pool).await,
        vec![("maintenance_guard".to_string(), 1)]
    );
    assert!(
        marks(&pool, "maintenance_guard").await > 0,
        "the appends left marks"
    );

    let new = new_release(&pool).await;
    let retired = new
        .retire_projection("maintenance_guard")
        .await
        .expect("retiring must succeed");
    assert_eq!(retired.version, Some(1));
    assert!(retired.was_registered());
    assert!(retired.applied_marks > 0, "{retired:?}");
    assert_eq!(registry(&pool).await, vec![]);
    assert_eq!(marks(&pool, "maintenance_guard").await, 0);

    let again = new
        .retire_projection("maintenance_guard")
        .await
        .expect("retiring again must succeed");
    assert_eq!(
        again,
        RetiredProjection {
            version: None,
            applied_marks: 0
        }
    );
    assert!(!again.was_registered());
}

/// A replica of the previous release keeps working after the retirement: it appends, its
/// catch-up does nothing, and only a restart registers the guard again — which the next
/// retirement takes back.
#[tokio::test]
async fn a_replica_of_the_previous_release_is_unharmed_and_a_restart_is_retired_again_postgres_test(
) {
    let (pool, _container) = start_pool().await;

    let old = previous_release(&pool, Guard::new("maintenance_guard", 1)).await;
    measure(&old, "boiler", 1).await;

    let new = new_release(&pool).await;
    let retired = new
        .retire_projection("maintenance_guard")
        .await
        .expect("retiring must succeed");
    assert_eq!(retired.version, Some(1));

    // The old replica is still serving: it appends, and marks the append for the guard it
    // registers, with no row to belong to.
    measure(&old, "boiler", 2).await;
    assert_eq!(
        registry(&pool).await,
        vec![],
        "an append writes no registry row"
    );
    assert!(marks(&pool, "maintenance_guard").await > 0);
    // Its catch-up finds no row and does nothing.
    let caught_up = old
        .catch_up_projections()
        .await
        .expect("the old replica's catch-up must not fail");
    assert_eq!(caught_up, 0);
    assert_eq!(registry(&pool).await, vec![]);
    drop(old);

    // A restart of the old release is a first registration again.
    let restarted_guard = Guard::new("maintenance_guard", 1);
    let inits = restarted_guard.inits.clone();
    let restarted = previous_release(&pool, restarted_guard).await;
    assert_eq!(inits.load(Ordering::SeqCst), 1, "a restart runs init again");
    assert_eq!(
        registry(&pool).await,
        vec![("maintenance_guard".to_string(), 1)]
    );
    drop(restarted);

    // Once every process runs the new release, the same call takes the row back. The
    // restart's own build sealed the history, so the marks the replica left are already
    // below its horizon and gone; what is left is the row.
    let retired = new
        .retire_projection("maintenance_guard")
        .await
        .expect("retiring must succeed");
    assert_eq!(retired.version, Some(1));
    assert_eq!(registry(&pool).await, vec![]);
    assert_eq!(marks(&pool, "maintenance_guard").await, 0);
}

/// A store cannot retire what it registers: the registration must be dropped first.
#[tokio::test]
async fn a_store_cannot_retire_a_projection_it_registers_postgres_test() {
    let (pool, _container) = start_pool().await;

    let store = previous_release(&pool, Guard::new("maintenance_guard", 1)).await;
    let error = store
        .retire_projection("maintenance_guard")
        .await
        .expect_err("retiring a registered projection must be refused");
    assert_eq!(error.kind(), replay::ErrorKind::InvalidInput, "{error}");
    assert_eq!(
        registry(&pool).await,
        vec![("maintenance_guard".to_string(), 1)]
    );
}

/// A retirement waits for a build or a catch-up of the projection in another process, and
/// the wait is bounded: one that runs out is `Unavailable`, and changes nothing.
#[tokio::test]
async fn retiring_waits_for_the_projection_lock_and_gives_up_when_the_wait_runs_out_postgres_test()
{
    let (pool, _container) = start_pool().await;

    let old = previous_release(&pool, Guard::new("maintenance_guard", 1)).await;
    drop(old);

    // Another session holds the guard's lock, as a build or a catch-up of it would.
    let mut holder = pool.acquire().await.expect("a connection");
    sqlx::query("SELECT pg_advisory_lock(x'52504A43'::int, hashtext($1))")
        .bind("maintenance_guard")
        .execute(&mut *holder)
        .await
        .expect("taking the projection lock must succeed");

    let new = new_release(&pool).await;
    let refused = new
        .retire_projection("maintenance_guard")
        .await
        .expect_err("a retirement whose wait runs out must be refused");
    assert_eq!(refused.kind(), replay::ErrorKind::Unavailable, "{refused}");
    assert_eq!(
        registry(&pool).await,
        vec![("maintenance_guard".to_string(), 1)]
    );

    // Released, the retirement goes through.
    sqlx::query("SELECT pg_advisory_unlock_all()")
        .execute(&mut *holder)
        .await
        .expect("releasing must succeed");
    let retired = new
        .retire_projection("maintenance_guard")
        .await
        .expect("retiring must succeed once the lock is free");
    assert_eq!(retired.version, Some(1));
}
