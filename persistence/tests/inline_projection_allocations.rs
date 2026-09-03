//! Allocation guard for the erased inline-projection bridge (issue #148).
//!
//! The bridge that adapts an [`InlineProjection`] to the store's registry used to
//! deep-copy the whole append batch once per registered projection (`event.data.clone()`
//! plus a whole-`PersistedEvent` clone). With `P` projections registered that made an
//! append of size `B` cost roughly `(1 + P) × B` bytes.
//!
//! This test pins the fixed behaviour: routing an append batch through `P` projections
//! must cost about the same as routing it through none. It uses a counting global
//! allocator, so it fails loudly if a per-projection clone is ever reintroduced.
//!
//! The measurement isolates the *payload* copy: the appended event carries a large
//! `blob` field that the projections' event type does not declare, so a borrow-based
//! deserialization allocates nothing for it while a `Value` clone must copy all of it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use replay::{Metadata, WithId};
use replay_macros::Event;
use replay_persistence::{EventStore, InMemoryEventStore, InlineProjection, PersistedEvent};
use serde::{Deserialize, Serialize};
use urn::{Urn, UrnBuilder};

// ── counting allocator ───────────────────────────────────────────────────────

thread_local! {
    /// Whether the current thread is inside a measured region.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Bytes requested on the current thread while counting was enabled.
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

/// Forwards to the system allocator, tallying requested bytes on the measuring thread.
///
/// Both thread-locals are const-initialized `Cell`s with no destructor, so reading them
/// from inside `alloc` cannot allocate and cannot recurse.
struct CountingAllocator;

fn count(bytes: usize) {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATED.try_with(|total| total.set(total.get() + bytes));
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Run `body` with allocation counting enabled, returning the bytes it requested.
fn measure_allocated<T>(body: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATED.set(0);
    COUNTING.set(true);
    let out = body();
    COUNTING.set(false);
    (out, ALLOCATED.get())
}

// ── fixtures ─────────────────────────────────────────────────────────────────

/// The appended (write-side) event. `blob` is the bulky payload whose copies we count.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Event)]
enum LedgerEvent {
    Recorded { seq: u64, blob: String },
}

/// The projections' event type: the same variant *without* the bulky field.
///
/// Serde ignores the unknown `blob` field, so deserializing this from the persisted
/// JSON allocates only the small `seq`. Any copy of the payload therefore shows up in
/// the allocation count as bridge overhead rather than as the projection's own cost.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Event)]
enum SlimLedgerEvent {
    Recorded { seq: u64 },
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
struct LedgerUrn(Urn);

impl From<LedgerUrn> for Urn {
    fn from(urn: LedgerUrn) -> Self {
        urn.0
    }
}

impl TryFrom<Urn> for LedgerUrn {
    type Error = String;

    fn try_from(urn: Urn) -> Result<Self, Self::Error> {
        Ok(LedgerUrn(urn))
    }
}

struct LedgerStream {
    id: LedgerUrn,
}

impl WithId for LedgerStream {
    type StreamId = LedgerUrn;

    fn with_id(id: Self::StreamId) -> Self {
        LedgerStream { id }
    }

    fn get_id(&self) -> &Self::StreamId {
        &self.id
    }
}

impl replay::EventStream for LedgerStream {
    type Event = LedgerEvent;

    fn stream_type() -> String {
        "Ledger".to_string()
    }

    fn apply(&mut self, _event: Self::Event) {}
}

/// Counts the events it is handed, so the test proves the batch really was routed.
struct SeqCounter {
    handled: Arc<AtomicUsize>,
}

impl InlineProjection for SeqCounter {
    type Exec = ();
    type Event = SlimLedgerEvent;

    fn name(&self) -> &str {
        "seq_counter"
    }

    fn version(&self) -> i32 {
        1
    }

    async fn init(&mut self, _conn: &mut Self::Exec) -> Result<(), replay::Error> {
        Ok(())
    }

    async fn handle(
        &mut self,
        _conn: &mut Self::Exec,
        events: &[PersistedEvent<Self::Event>],
    ) -> Result<(), replay::Error> {
        self.handled.fetch_add(events.len(), Ordering::SeqCst);
        Ok(())
    }
}

const BLOB_LEN: usize = 64 * 1024;
const BATCH: usize = 4;
const PROJECTIONS: usize = 8;

fn batch() -> Vec<LedgerEvent> {
    (0..BATCH)
        .map(|seq| LedgerEvent::Recorded {
            seq: seq as u64,
            blob: "x".repeat(BLOB_LEN),
        })
        .collect()
}

/// Append `events` to a store carrying `projections` registered projections, returning
/// the bytes allocated during the append and the total number of events handled.
fn append_measuring_allocations(projections: usize) -> (usize, usize) {
    let handled = Arc::new(AtomicUsize::new(0));

    let mut store = InMemoryEventStore::new();
    for _ in 0..projections {
        store = store.register_projection(SeqCounter {
            handled: handled.clone(),
        });
    }

    let stream_id = LedgerUrn(
        UrnBuilder::new("ledger", &format!("alloc-{projections}"))
            .build()
            .unwrap(),
    );
    let events = batch();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    let (result, allocated) = measure_allocated(|| {
        runtime.block_on(store.store_events::<LedgerStream>(
            &stream_id,
            "Ledger".to_string(),
            Metadata::default(),
            &events,
            None,
        ))
    });
    result.expect("append must succeed");

    (allocated, handled.load(Ordering::SeqCst))
}

/// Routing an append batch through `P` projections must not copy the payload `P` times.
///
/// Before the borrow-deserialize fix each projection cloned the event's JSON `data`, so
/// the cost grew by ~one payload per projection: `(1 + P) × B`. Now the per-projection
/// cost is independent of the payload size, so the measured overhead over a
/// no-projection append stays a small fraction of a single payload.
#[test]
fn routing_through_many_projections_allocates_about_one_payload() {
    let payload = BLOB_LEN * BATCH;

    let (baseline, none_handled) = append_measuring_allocations(0);
    let (with_projections, handled) = append_measuring_allocations(PROJECTIONS);

    assert_eq!(
        none_handled, 0,
        "no projections registered, nothing handled"
    );
    assert_eq!(
        handled,
        BATCH * PROJECTIONS,
        "every projection must receive the whole batch"
    );

    let overhead = with_projections.saturating_sub(baseline);
    let budget = payload / 10;

    assert!(
        overhead < budget,
        "routing {BATCH} events ({payload} B of payload) through {PROJECTIONS} projections \
         allocated {overhead} B over the {baseline} B baseline, which exceeds the \
         {budget} B budget — the bridge is copying the payload per projection again"
    );

    // The whole append, projections included, costs about the store's own handling of
    // the batch plus a bounded routing overhead — not one extra payload per projection.
    assert!(
        with_projections < baseline + payload,
        "append with {PROJECTIONS} projections allocated {with_projections} B against a \
         {baseline} B baseline for {payload} B of payload — expected ~1 payload of \
         routing cost at most, not (1 + {PROJECTIONS})"
    );
}
