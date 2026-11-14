//! Spike 1b acceptance tests (Section 5 criteria 1-4, 6-7).
//!
//! Deterministic: round-robin scheduler, no randomness, no timing
//! assertions. Criterion 5 (stack evidence) and switch cost are
//! measured by `demo1b` and reported, not asserted, except for loose
//! structural bounds here.

use std::cell::Cell;
use std::rc::Rc;

use spike_1_gc_scheduler::heap::GcRef;
use spike_1b_stack_switching::{ChannelId, DEFAULT_STACK_BYTES, Scheduler, TaskRt, stack_probe};

fn ranges_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    // Ranges are (base=top, limit=bottom); overlap if neither is
    // fully above the other.
    !(a.1 >= a.0 || b.1 >= b.0 || a.1 >= b.0 || b.1 >= a.0)
}

// --------------------------------- 1. separate stacks, join, no loss/dup

fn stack_task_a(rt: TaskRt, probe: Rc<Cell<usize>>) {
    probe.set(stack_probe());
    for i in 0..2 {
        rt.log(format!("a tick {i}"));
        rt.yield_now();
    }
    rt.log("a done");
}

fn stack_task_b(rt: TaskRt, probe: Rc<Cell<usize>>) {
    probe.set(stack_probe());
    for i in 0..2 {
        rt.log(format!("b tick {i}"));
        rt.yield_now();
    }
    rt.log("b done");
}

#[test]
fn tasks_run_on_distinct_native_stacks_and_join() {
    let mut s = Scheduler::new();
    let pa = Rc::new(Cell::new(0usize));
    let pb = Rc::new(Cell::new(0usize));
    let ha = s.spawn("a", DEFAULT_STACK_BYTES, {
        let pa = Rc::clone(&pa);
        move |rt| stack_task_a(rt, pa)
    });
    let hb = s.spawn("b", DEFAULT_STACK_BYTES, {
        let pb = Rc::clone(&pb);
        move |rt| stack_task_b(rt, pb)
    });

    let ra = s.stack_range(ha);
    let rb = s.stack_range(hb);
    // Distinct, non-overlapping native stacks.
    assert_ne!(ra.0, rb.0, "separate stack tops");
    assert!(!ranges_overlap(ra, rb), "ranges {ra:x?} {rb:x?} overlap");

    s.join(ha);
    assert!(s.is_done(ha));
    s.join(hb);
    s.join(ha);

    // Each task's probe lies inside its own stack range: execution
    // really happened on that stack.
    for (h, p) in [(ha, pa.get()), (hb, pb.get())] {
        let (base, limit) = s.stack_range(h);
        assert!(p < base && p >= limit, "probe 0x{p:x} outside range");
    }

    let log = s.event_log().join("\n");
    for line in [
        "a tick 0", "b tick 0", "a tick 1", "b tick 1", "a done", "b done",
    ] {
        assert!(log.contains(line), "missing {line} in:\n{log}");
    }
    // High-water measured after completion: positive, bounded.
    for h in [ha, hb] {
        let hw = s.high_water(h).expect("measured at finish");
        assert!(hw > 0 && hw < s.stack_usable(h), "hw={hw}");
    }
}

// --------------------------------- 2. nested yield keeps locals intact

fn l4(rt: &TaskRt, round: i64) -> i64 {
    let sentinel = 1000 + round;
    let buf = [round; 4];
    rt.log(format!("round {round} probe=0x{:x}", stack_probe()));
    rt.yield_now(); // suspend 4 frames deep
    assert_eq!(sentinel, 1000 + round, "l4 local intact");
    assert!(buf.iter().all(|&x| x == round), "l4 array intact");
    sentinel + round
}

fn l3(rt: &TaskRt, round: i64) -> i64 {
    let sentinel = 2000 + round;
    let r = l4(rt, round);
    assert_eq!(r, 1000 + 2 * round, "l4 return intact");
    assert_eq!(sentinel, 2000 + round, "l3 local intact");
    r + sentinel
}

fn l2(rt: &TaskRt, round: i64) -> i64 {
    let sentinel = 3000 + round;
    let r = l3(rt, round);
    assert_eq!(sentinel, 3000 + round, "l2 local intact");
    r + sentinel
}

fn nest_task(rt: TaskRt) {
    for round in 0..3 {
        let r = l2(&rt, round);
        assert_eq!(r, 6000 + 4 * round, "full chain intact");
        rt.log(format!("round {round} ok"));
        rt.yield_now();
    }
    rt.log("nest done");
}

fn churn_task(rt: TaskRt) {
    for _ in 0..4 {
        for i in 0..10 {
            rt.alloc_int(i);
        }
        let rep = rt.gc_collect_for_test();
        rt.log(format!("churn reclaimed={}", rep.reclaimed_this_run));
        rt.yield_now();
    }
    rt.log("churn done");
}

#[test]
fn yield_from_nested_calls_preserves_locals_and_returns() {
    let mut s = Scheduler::new();
    let n = s.spawn("nest", DEFAULT_STACK_BYTES, nest_task);
    let c = s.spawn("churn", DEFAULT_STACK_BYTES, churn_task);
    s.join(n);
    s.join(c);
    let log = s.event_log().join("\n");
    for round in 0..3 {
        assert!(log.contains(&format!("round {round} ok")), "{log}");
    }
    assert!(log.contains("nest done"), "{log}");
    let (base, limit) = s.stack_range(n);
    assert!(base > limit, "sane range");
}

// --------------------------------- 3. roots across real switches

fn cons(rt: &TaskRt, head_val: i64, tail: GcRef) -> GcRef {
    rt.with_frame(|rt| {
        let h = rt.alloc_rooted_int(head_val);
        rt.alloc_pair(h, tail)
    })
}

fn build_list(rt: &TaskRt) -> GcRef {
    rt.with_frame(|rt| {
        let nil = rt.alloc_rooted_int(0);
        let mut list = nil;
        for v in [30, 20, 10] {
            let cell = cons(rt, v, list);
            rt.root(cell);
            list = cell;
        }
        list
    })
}

fn check_list(rt: &TaskRt, list: GcRef) {
    assert!(rt.shadow_contains(list), "root published across switch");
    let (h10, t10) = rt.heap_get_pair(list).expect("pair survives");
    assert_eq!(rt.heap_get_int(h10), Some(10));
    let (h20, t20) = rt.heap_get_pair(t10).expect("pair survives");
    assert_eq!(rt.heap_get_int(h20), Some(20));
    let (h30, tnil) = rt.heap_get_pair(t20).expect("pair survives");
    assert_eq!(rt.heap_get_int(h30), Some(30));
    assert_eq!(rt.heap_get_int(tnil), Some(0));
}

fn builder_task(rt: TaskRt) {
    let list = build_list(&rt);
    rt.root(list);
    let depth = rt.shadow_len();
    rt.log(format!("built depth={depth}"));
    rt.yield_now(); // suspended mid-computation: list in a native local
    check_list(&rt, list);
    assert_eq!(rt.shadow_len(), depth, "shadow depth stable");
    rt.log("verified 1");
    rt.yield_now();
    check_list(&rt, list);
    rt.log("verified 2");
}

fn garbage_task(rt: TaskRt) {
    for _ in 0..3 {
        for i in 0..40 {
            rt.alloc_int(i); // unrooted by construction
        }
        let rep = rt.gc_collect_for_test();
        rt.log(format!("garbage reclaimed={}", rep.reclaimed_this_run));
        rt.yield_now();
    }
}

#[test]
fn roots_survive_real_switches_with_exact_counts() {
    let mut s = Scheduler::new();
    let b = s.spawn("builder", DEFAULT_STACK_BYTES, builder_task);
    let g = s.spawn("garbage", DEFAULT_STACK_BYTES, garbage_task);
    s.join(b);
    s.join(g);

    let log = s.event_log().join("\n");
    assert!(log.contains("built depth=1"), "{log}");
    assert!(log.contains("verified 1"), "{log}");
    assert!(log.contains("verified 2"), "{log}");

    let st = s.heap_stats();
    assert_eq!(st.reclaimed_objects, 120, "3 x 40 garbage: {st:?}");
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 7, "3 heads + nil + 3 pairs: {rep:?}");
    assert_eq!(rep.reclaimed_this_run, 0);
}

// --------------------------------- 4. missed-root control under switching

fn suspend_nested(rt: &TaskRt) {
    rt.yield_now();
}

fn victim_task(rt: TaskRt) {
    // `garbage` lives ONLY in a native-stack local across the switch:
    // the collector must not see it.
    let garbage = rt.alloc_int(1);
    let keeper = rt.alloc_rooted_int(99);
    suspend_nested(&rt);
    assert!(
        rt.heap_get_int(garbage).is_none(),
        "unpublished handle died across a real switch",
    );
    assert_eq!(rt.heap_get_int(keeper), Some(99), "published root lives");
    rt.log("control ok");
}

#[test]
fn unpublished_root_dies_across_real_switch() {
    let mut s = Scheduler::new();
    let v = s.spawn("victim", DEFAULT_STACK_BYTES, victim_task);
    assert!(s.drive_once(), "victim suspends in a nested call");
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 1, "only the rooted keeper: {rep:?}");
    s.join(v);
    assert!(s.event_log().iter().any(|l| l == "control ok"));
}

// --------------------------------- 5. channel through real switches

fn send_nested(rt: &TaskRt, ch: ChannelId, v: GcRef) {
    rt.send(ch, v); // suspends 2 deep
}

fn sender_task(rt: TaskRt, ch: ChannelId) {
    for n in [7, 8, 9] {
        let pair = rt.with_frame(|rt| {
            let h = rt.alloc_rooted_int(n);
            let t = rt.alloc_rooted_int(100 * n);
            rt.alloc_pair(h, t)
        });
        rt.root(pair);
        send_nested(&rt, ch, pair);
    }
    rt.log("sender done");
}

fn recv_nested(rt: &TaskRt, ch: ChannelId) -> GcRef {
    let v = rt.recv(ch); // suspends 2 deep
    rt.root(v);
    v
}

fn receiver_task(rt: TaskRt, ch: ChannelId) {
    for want in [7, 8, 9] {
        let v = recv_nested(&rt, ch);
        let (h, t) = rt.heap_get_pair(v).expect("pair survives");
        let (hv, tv) = (
            rt.heap_get_int(h).expect("head survives"),
            rt.heap_get_int(t).expect("tail survives"),
        );
        assert_eq!((hv, tv), (want, 100 * want));
        rt.log(format!("got {hv} {tv}"));
    }
    rt.log("receiver done");
}

#[test]
fn channel_moves_heap_values_through_real_switches() {
    let mut s = Scheduler::new();
    let ch = s.channel();
    let tx = s.spawn("tx", DEFAULT_STACK_BYTES, move |rt| sender_task(rt, ch));
    let rx = s.spawn("rx", DEFAULT_STACK_BYTES, move |rt| receiver_task(rt, ch));

    assert!(s.drive_once(), "sender parks with a value in flight");
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 3, "pair + 2 ints via channel: {rep:?}");
    assert_eq!(rep.reclaimed_this_run, 0);

    assert!(s.drive_once(), "receiver rendezvous");
    let rep = s.gc_collect_for_test();
    assert_eq!(rep.live_after, 3, "value via inbox: {rep:?}");

    s.join(tx);
    s.join(rx);
    // Order falls out of the rendezvous alternation: each send parks
    // until the receiver takes it, so receives interleave with sends.
    let log_owned = s.event_log();
    let log: Vec<&str> = log_owned.iter().map(String::as_str).collect();
    assert_eq!(
        log,
        [
            "got 7 700",
            "got 8 800",
            "sender done",
            "got 9 900",
            "receiver done"
        ],
    );
}

// --------------------------------- 6. overflow child traps on the guard page

#[test]
#[cfg(unix)]
fn overflow_child_traps() {
    use std::os::unix::process::ExitStatusExt;
    let exe = env!("CARGO_BIN_EXE_overflow1b");
    let out = std::process::Command::new(exe)
        .output()
        .expect("spawn overflow child");
    assert!(!out.status.success(), "child must not exit cleanly");
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGSEGV),
        "guard page trap, got {:?}",
        out.status.code(),
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("depth="), "child recursed first: {err}");
}
