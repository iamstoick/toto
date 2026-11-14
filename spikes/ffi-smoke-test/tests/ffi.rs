//! FFI smoke tests (Section 4 criteria 1-7).
//!
//! Deterministic: fixed inputs, asserted values and exact GC counts.
//! One test uses a real panic (caught at the boundary); the process
//! must stay alive — a panic escaping through C would abort it.

use ffi_smoke_test::{
    ERR_DIV_ZERO, FfiError, HandleTable, add, bad_bytes, drain_queue, emit_all, fallible_double,
    gc_collect, greet, read_first_byte, rust_div,
};
use spike_1_gc_scheduler::heap::Heap;
use spike_1_gc_scheduler::roots::ShadowStack;

// --------------------------------- 1. plain C call

#[test]
fn c_add_returns_correct_value() {
    assert_eq!(add(20, 22), 42);
    assert_eq!(add(-5, 5), 0);
    assert_eq!(add(i32::MAX, 0), i32::MAX);
}

// --------------------------------- 2. string boundary

#[test]
fn string_round_trip_validates_and_rejects_invalid_utf8() {
    assert_eq!(greet("hello").as_deref(), Ok("hello"));
    assert_eq!(greet("héllo🍲 wörld").as_deref(), Ok("héllo🍲 wörld"));
    // Invalid UTF-8 from C is a clean error, not a panic or UB.
    assert_eq!(bad_bytes(), Err(FfiError::InvalidUtf8));
}

// --------------------------------- 3. rooting across the call

#[test]
fn rooted_object_survives_c_call_missed_root_dies() {
    let mut heap = Heap::new();
    let mut shadow = ShadowStack::new();

    // Rooted: the borrow behind the raw pointer is backed by a root
    // for the whole call; C reads through it; the object survives.
    let hello = heap.alloc_str("hello");
    shadow.push(hello);
    let bytes = heap.get_str(hello).expect("live");
    assert_eq!(read_first_byte(bytes.as_bytes()), b'h');
    let rep = gc_collect(&mut heap, &[&shadow]);
    assert_eq!(rep.live_after, 1, "{rep:?}");
    assert!(heap.get(hello).is_some());
    // `bytes` is last used by the C call above: the borrow ends
    // there (NLL), so the collection cannot observe it.

    // Missed-root control: same call shape, but the object is never
    // published. The call itself happens while the object is still
    // alive (no UB); the forced GC afterwards must reclaim it.
    let world = heap.alloc_str("world");
    let bytes = heap.get(world).expect("alive before any GC");
    let first = read_first_byte(match bytes {
        (_, spike_1_gc_scheduler::heap::Payload::Text(t)) => t.as_bytes(),
        _ => unreachable!(),
    });
    assert_eq!(first, b'w');
    // Borrow ends at the last use above; the collection below is free
    // to reclaim the unpublished object (it must not be touched after).
    let rep = gc_collect(&mut heap, &[&shadow]);
    assert_eq!(rep.live_after, 1, "only the rooted hello: {rep:?}");
    assert_eq!(rep.reclaimed_this_run, 1, "unpublished world died: {rep:?}");
    assert!(heap.get(world).is_none(), "stale handle resolves to None");
}

// --------------------------------- 4. handle rule

#[test]
fn handle_resolves_after_gc_and_fails_cleanly_after_release() {
    let mut heap = Heap::new();
    let mut table = HandleTable::new();
    let cell = heap.alloc_int(42);
    let h = table.retain(cell);

    // Roots for the collection: the table's root set (the holder's
    // shadow would join here in a real runtime).
    let rep = gc_collect(&mut heap, &[table.root_set()]);
    assert_eq!(rep.live_after, 1, "{rep:?}");
    let back = table.resolve(&heap, h).expect("held handle resolves");
    assert_eq!(heap.get_int(back), Some(42));

    assert!(table.release(h));
    assert!(!table.release(h), "double release reports false");
    let rep = gc_collect(&mut heap, &[table.root_set()]);
    assert_eq!(rep.live_after, 0, "{rep:?}");
    assert_eq!(rep.reclaimed_this_run, 1, "{rep:?}");
    assert_eq!(table.resolve(&heap, h), Err(FfiError::DeadHandle(h)));
    assert_eq!(table.resolve(&heap, 999), Err(FfiError::DeadHandle(999)));
}

// --------------------------------- 5. panic boundary

#[test]
fn panic_at_boundary_becomes_error_code() {
    // Success path first: unaffected.
    assert_eq!(rust_div(10, 2), 5);
    // A real Rust panic behind the extern "C" boundary is caught and
    // converted; the process stays alive (a panic escaping through C
    // would abort the test harness).
    assert_eq!(rust_div(10, 0), ERR_DIV_ZERO);
    assert_eq!(rust_div(10, 2), 5, "boundary still usable afterwards");
}

// --------------------------------- 6. callback shape

#[test]
fn static_callback_enqueues_without_reentry() {
    drain_queue(); // start clean (static queue, parallel tests)
    emit_all();
    let got = drain_queue();
    assert_eq!(
        got,
        vec![(1, "start".to_owned()), (2, "stop".to_owned())],
        "exactly the C-emitted messages, in order"
    );
    // The callback performs only infallible queue ops (copy + push);
    // it never touches scheduler/GC state — enforced by code
    // inspection of `on_event` (no other reachable state).
}

// --------------------------------- 7. error slot

#[test]
fn error_slot_converts_and_success_path_unaffected() {
    assert_eq!(fallible_double(5), Ok(10));
    assert_eq!(
        fallible_double(-1),
        Err(FfiError::CError {
            code: -1,
            message: "negative input".to_owned()
        })
    );
    // Success path clears/ignores the slot: no stale error leaks.
    assert_eq!(fallible_double(7), Ok(14));
}
