//! GC unit tests (spike practice): exact reclaimed/live counts,
//! stale-reference handling, zero values, and the publish-then-collect
//! discipline behind every safepoint.

use std::collections::HashMap;

use toto_compiler::hir::Ty;
use toto_runtime::heap::Heap;
use toto_runtime::roots::ShadowStack;
use toto_runtime::value::{RtVal, heap_refs_of, zero_value};

#[test]
fn unreachable_objects_reclaimed_with_exact_counts() {
    let mut heap = Heap::new();
    let live = heap.alloc_str("keep");
    let _dead1 = heap.alloc_str("drop me");
    let _dead2 = heap.alloc_array(vec![RtVal::Int(1)]);
    let report = heap.collect(&[live]);
    assert_eq!(report.live, 1);
    assert_eq!(report.collected, 2);
    assert!(heap.get(live).is_some());
    let stats = heap.stats();
    assert_eq!(stats.objects_live, 1);
    assert_eq!(stats.allocated_total, 3);
    assert_eq!(stats.reclaimed_total, 2);
    assert_eq!(stats.collections, 1);
}

#[test]
fn nested_objects_trace_through_containers() {
    let mut heap = Heap::new();
    let inner = heap.alloc_array(vec![RtVal::Int(7)]);
    let outer = heap.alloc_array(vec![RtVal::Heap(inner)]);
    let alone = heap.alloc_str("unreachable");
    let _ = alone;
    let report = heap.collect(&[outer]);
    assert_eq!(report.live, 2);
    assert_eq!(report.collected, 1);
    // The inner array survived through the outer one.
    match heap.get(outer) {
        Some(toto_runtime::heap::Payload::Array(elems)) => {
            assert!(matches!(elems[0], RtVal::Heap(_)));
        }
        other => panic!("outer not an array: {other:?}"),
    }
}

#[test]
fn stale_refs_resolve_to_none() {
    let mut heap = Heap::new();
    let r = heap.alloc_str("temporary");
    assert!(heap.get(r).is_some());
    let report = heap.collect(&[]);
    assert_eq!(report.collected, 1);
    // Same index, old generation: stale.
    assert!(heap.get(r).is_none());
    assert!(heap.desc(r).is_none());
}

#[test]
fn shadow_publish_then_collect_keeps_published() {
    let mut heap = Heap::new();
    let mut shadow = ShadowStack::new();
    let kept = heap.alloc_str("suspended task local");
    let _dropped = heap.alloc_str("garbage");
    shadow.publish(vec![kept]);
    assert_eq!(shadow.len(), 2 - 1);
    let report = heap.collect(&shadow.iter().collect::<Vec<_>>());
    assert_eq!(report.live, 1);
    assert_eq!(report.collected, 1);
    shadow.clear();
    assert!(shadow.is_empty());
}

#[test]
fn heap_refs_cover_ok_wrappers() {
    let mut heap = Heap::new();
    let inner = heap.alloc_str("in ok");
    let v = RtVal::Ok(Box::new(RtVal::Heap(inner)));
    let mut out = Vec::new();
    heap_refs_of(&v, &mut out);
    assert_eq!(out, vec![inner]);
}

#[test]
fn zero_values_match_spec_table() {
    let mut heap = Heap::new();
    let structs = HashMap::new();
    let mut z = |ty: &Ty| zero_value(ty, &mut heap, &structs);
    assert!(matches!(z(&Ty::Int), Some(RtVal::Int(0))));
    assert!(matches!(z(&Ty::Float), Some(RtVal::Float(f)) if f == 0.0));
    assert!(matches!(z(&Ty::Bool), Some(RtVal::Bool(false))));
    assert!(matches!(z(&Ty::Unit), Some(RtVal::Unit)));
    assert!(matches!(z(&Ty::Range), Some(RtVal::Range(0, 0))));
    // Composite zeros allocate.
    assert!(matches!(z(&Ty::String), Some(RtVal::Heap(_))));
    assert!(matches!(
        z(&Ty::Array(Box::new(Ty::Int))),
        Some(RtVal::Heap(_))
    ));
    assert!(matches!(
        z(&Ty::Option(Box::new(Ty::Int))),
        Some(RtVal::None)
    ));
    assert!(matches!(
        z(&Ty::Result(Box::new(Ty::Int), Box::new(Ty::String))),
        Some(RtVal::Ok(_))
    ));
    // No zero value.
    assert!(z(&Ty::Chan(Box::new(Ty::Int))).is_none());
    assert!(z(&Ty::Fn(vec![], Box::new(Ty::Unit))).is_none());
}

// Phase 6, Stage 2a: `is_live` safety property — NO under-retention.
// Everything reachable at the last collection resolves; the query
// is exact relative to the last collection's root set.
#[test]
fn is_live_never_under_retains() {
    let mut heap = Heap::new();
    let inner = heap.alloc_closure(3, vec![RtVal::Int(1)]);
    let outer = heap.alloc_array(vec![RtVal::Heap(inner)]);
    let report = heap.collect(&[outer]);
    assert_eq!((report.live, report.collected), (2, 0));
    assert!(heap.is_live(outer), "root must resolve");
    assert!(heap.is_live(inner), "reached closure must resolve");
    // Unreferenced after the fact: still present until swept.
    let report = heap.collect(&[]);
    assert_eq!((report.live, report.collected), (0, 2));
    assert!(!heap.is_live(outer));
    assert!(!heap.is_live(inner));
}

// Bounded over-retention: unreachable-but-unswept objects resolve
// (retaining at most until the next forced collection — the stated
// bound), and the next collection releases them.
#[test]
fn is_live_over_retains_at_most_one_collection() {
    let mut heap = Heap::new();
    let c = heap.alloc_closure(7, vec![]);
    // No collection yet: present, so live by definition.
    assert!(heap.is_live(c));
    let report = heap.collect(&[]);
    assert_eq!((report.live, report.collected), (0, 1));
    assert!(!heap.is_live(c), "swept object must not resolve");
}

// Stale generations never resolve, even when the slot is reused.
#[test]
fn is_live_rejects_stale_generations_on_reuse() {
    let mut heap = Heap::new();
    let old = heap.alloc_str("temporary");
    heap.collect(&[]);
    assert!(!heap.is_live(old));
    let new = heap.alloc_str("resident");
    assert_eq!(new.index(), old.index(), "slot reuse is LIFO");
    assert_eq!(new.generation(), old.generation().wrapping_add(1).max(1));
    assert!(heap.is_live(new));
    assert!(!heap.is_live(old), "old generation stays dead");
}

// Allocation watermark semantics: objects born after the last
// collection are live-by-presence; the next collection with
// narrower roots sweeps exactly the unreached.
#[test]
fn is_live_covers_post_collection_allocations() {
    let mut heap = Heap::new();
    let a = heap.alloc_str("kept");
    heap.collect(&[a]);
    let b = heap.alloc_closure(1, vec![RtVal::Heap(a)]);
    // B is unmarked (born after the mark) but present: live.
    assert!(heap.is_live(a));
    assert!(heap.is_live(b));
    let report = heap.collect(&[a]);
    assert_eq!((report.live, report.collected), (1, 1));
    assert!(heap.is_live(a));
    assert!(!heap.is_live(b), "unreached post-GC alloc swept");
}

// `closure_refs` enumerates resident closures with their bodies;
// swept closures vanish from the scan.
#[test]
fn closure_refs_lists_resident_closures_only() {
    let mut heap = Heap::new();
    let c1 = heap.alloc_closure(11, vec![]);
    let _s = heap.alloc_str("not a closure");
    let c2 = heap.alloc_closure(22, vec![RtVal::Int(9)]);
    let mut found: Vec<(u32, u32)> = heap
        .closure_refs()
        .iter()
        .map(|(r, b)| (r.index(), *b))
        .collect();
    found.sort();
    assert_eq!(found.len(), 2);
    assert!(found.contains(&(c1.index(), 11)));
    assert!(found.contains(&(c2.index(), 22)));
    heap.collect(&[]);
    assert!(heap.closure_refs().is_empty());
    assert!(!heap.is_live(c1) && !heap.is_live(c2));
}
