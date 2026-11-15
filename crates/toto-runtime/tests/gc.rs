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
