//! Retiring an inline projection the application no longer registers
//! (funkode-io/replay#338).
//!
//! The registry keeps a row per projection name for as long as anything wrote it, and no
//! build ever removes one: a registration the code dropped stays in `projections`, with
//! its applied marks, and reads as a live component to whoever lists the registry. Each
//! step here is one statement over one name, run in the caller's transaction; what comes
//! back is a version and a count, never the rows.

use sqlx::PgConnection;

use super::advisory_keys::{PROJECTION_LOCK_CLASS, REGISTRY_LOCK_CLASS};

/// What [`PostgresEventStore::retire_projection`](crate::PostgresEventStore::retire_projection)
/// found for the name, and removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetiredProjection {
    /// The version the registry recorded for the projection, or `None` when it held no row
    /// for the name: a second call, or a name that was never registered.
    pub version: Option<i32>,
    /// Applied marks deleted with it: the transactions recorded as having applied the
    /// projection, at any version.
    pub applied_marks: u64,
}

impl RetiredProjection {
    /// Whether the registry held a row for the name: `false` means there was nothing to
    /// retire, and the call changed at most the marks.
    pub fn was_registered(&self) -> bool {
        self.version.is_some()
    }
}

/// Take, for the current transaction, the registry lock exclusively and then `name`'s
/// projection lock, waiting for each.
///
/// The order every other taker uses — a `build` takes the registry shared and then its
/// projections, a deletion of streams the registry exclusively and then every projection
/// — so a retirement queues behind a build or a catch-up of this projection in any
/// process, rather than deadlocking with it, and a build that starts later waits for the
/// retirement to commit and then finds no row: a first registration.
pub(crate) async fn lock(conn: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, 0)")
        .bind(REGISTRY_LOCK_CLASS)
        .execute(&mut *conn)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(PROJECTION_LOCK_CLASS)
        .bind(name)
        .execute(conn)
        .await?;
    Ok(())
}

/// Delete every applied mark of `name`, at any version, and return how many there were.
///
/// Marks have no row to belong to once the registry row is gone, and nothing would ever
/// prune them: the horizon that deletes marks moves only when the projection catches up,
/// and a projection nobody registers never does.
pub(crate) async fn delete_marks(conn: &mut PgConnection, name: &str) -> Result<u64, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM projection_applied_transactions WHERE projection = $1")
        .bind(name)
        .execute(conn)
        .await?;
    Ok(deleted.rows_affected())
}

/// Delete `name`'s registry row and return the version it recorded, or `None` when there
/// was none.
pub(crate) async fn delete_row(
    conn: &mut PgConnection,
    name: &str,
) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar("DELETE FROM projections WHERE name = $1 RETURNING version")
        .bind(name)
        .fetch_optional(conn)
        .await
}
