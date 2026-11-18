//! GC pause measurements (Enterprise Wave 0, Stage 2): stop-the-world
//! `Heap::collect` latency vs live-set size. A pause IS the `collect`
//! call (single-threaded STW; M:N rendezvous adds coordination around
//! the same stop). Each case builds a synthetic heap with a controlled
//! live set plus garbage, runs 21 collections, and reports
//! median/p99. The p99 bound pins the published pause budget
//! (`docs/gc.md`); the 10x headroom over observed absorbs CI machine
//! variance the way the 5x bench flag does.
use toto_runtime::heap::{GcRef, Heap};
use toto_runtime::value::RtVal;

fn build_heap(live_objs: usize, garbage_objs: usize) -> (Heap, Vec<GcRef>) {
    let mut heap = Heap::new();
    let mut roots = Vec::new();
    // Live set: linked arrays (cross-references exercise the marker)
    // plus strings and small structs for payload mix.
    let mut prev: Option<GcRef> = None;
    for i in 0..live_objs {
        let elems: Vec<RtVal> = (0..8).map(|j| RtVal::Int(i as i64 * 31 + j)).collect();
        let arr = heap.alloc_array(elems);
        if let Some(p) = prev {
            let _link = heap.alloc_array(vec![RtVal::Heap(p), RtVal::Heap(arr)]);
            if i % 4 == 0 {
                roots.push(arr);
            }
        } else {
            roots.push(arr);
        }
        prev = Some(arr);
        if i % 3 == 0 {
            let s = heap.alloc_str("live payload string for gc pause sizing");
            roots.push(s);
        }
    }
    for g in 0..garbage_objs {
        if g % 2 == 0 {
            heap.alloc_str("garbage string that must die in the next collect");
        } else {
            heap.alloc_array(vec![RtVal::Int(g as i64), RtVal::Int(-(g as i64))]);
        }
    }
    (heap, roots)
}

fn p99(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() * 99 / 100]
}

fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// The published pause budget binds the production configuration
/// (release): p99 STW pause <= 10 ms at <= 100k live objects. The same
/// test runs in the debug gate with a 30 ms guard bound (debug
/// codegen is ~2.5x slower here; both numbers measured, see
/// `docs/gc.md`). A debug-only failure is a measurement-environment
/// signal, not a budget violation — release decides.
fn budget_us() -> u128 {
    if cfg!(debug_assertions) {
        30_000
    } else {
        10_000
    }
}

fn measure(label: &str, live_objs: usize, garbage_objs: usize) {
    let budget_us = budget_us();
    // Warm up (allocator + page faults out of the timed region).
    let (mut heap, roots) = build_heap(live_objs, garbage_objs);
    heap.collect(&roots);
    let mut samples = Vec::with_capacity(21);
    for _ in 0..21 {
        // Replenish garbage so every timed collect does real sweep work.
        for g in 0..garbage_objs {
            if g % 2 == 0 {
                heap.alloc_str("garbage string that must die in the next collect");
            } else {
                heap.alloc_array(vec![RtVal::Int(g as i64)]);
            }
        }
        let start = std::time::Instant::now();
        let report = heap.collect(&roots);
        samples.push(start.elapsed().as_micros());
        assert!(report.live > 0, "live set must survive");
    }
    let med = median(samples.clone());
    let p = p99(samples);
    println!("gc_pause[{label}]: live~{live_objs} med={med}us p99={p}us budget={budget_us}us");
    assert!(
        p <= budget_us,
        "p99 pause {p}us exceeds budget {budget_us}us at live~{live_objs}"
    );
}

#[test]
fn gc_pause_budget_1k() {
    measure("1k", 1_000, 1_000);
}

#[test]
fn gc_pause_budget_10k() {
    measure("10k", 10_000, 10_000);
}

#[test]
fn gc_pause_budget_100k() {
    measure("100k", 100_000, 20_000);
}
