//! Spike 1b demo: nested yields on real stacks + channel + forced GC,
//! then the measured numbers quoted in the spike report.

use std::time::Instant;

use spike_1b_stack_switching::{ChannelId, DEFAULT_STACK_BYTES, Scheduler, TaskRt};

fn level3(rt: &TaskRt, ch: ChannelId, n: i64) {
    // Fourth frame: allocate, root, send with everything published.
    let tag = rt.with_frame(|rt| {
        let a = rt.alloc_rooted_int(n);
        let b = rt.alloc_rooted_int(1000 + n);
        rt.alloc_pair(a, b)
    });
    rt.root(tag);
    rt.send(ch, tag);
}

fn level2(rt: &TaskRt, ch: ChannelId, n: i64) {
    level3(rt, ch, n);
}

fn producer(rt: TaskRt, ch: ChannelId) {
    for n in 1..=10 {
        // One unrooted temp per round: garbage the GC must reclaim.
        rt.alloc_int(-n);
        level2(&rt, ch, n);
    }
    rt.log("producer done");
}

fn consume(rt: &TaskRt, v: spike_1_gc_scheduler::heap::GcRef) -> i64 {
    // Nested receive-side processing with its own frame.
    rt.with_frame(|rt| {
        let (h, _) = rt.heap_get_pair(v).expect("pair survives");
        rt.heap_get_int(h).expect("int survives")
    })
}

fn consumer(rt: TaskRt, ch: ChannelId) {
    let mut count = 0;
    while count < 10 {
        let v = rt.recv(ch);
        rt.root(v);
        let n = consume(&rt, v);
        count += 1;
        rt.log(format!("consumer got {n} (#{count})"));
    }
    rt.log("consumer done");
}

fn ping(rt: TaskRt) {
    for _ in 0..5000 {
        rt.yield_now();
    }
}

fn main() {
    // --- Functional scenario: nested send/recv + forced GCs.
    let mut s = Scheduler::new().with_threshold(4096);
    let ch = s.channel();
    let prod = s.spawn("producer", DEFAULT_STACK_BYTES, move |rt| producer(rt, ch));
    let cons = s.spawn("consumer", DEFAULT_STACK_BYTES, move |rt| consumer(rt, ch));
    s.join(prod);
    let mid = s.gc_collect_for_test();
    s.join(cons);
    s.run_all();

    let st = s.heap_stats();
    println!("spike-1b demo measurements (rough spike numbers):");
    println!("  objects allocated:       {}", st.allocated_objects);
    println!("  objects reclaimed:       {}", st.reclaimed_objects);
    println!("  live objects at end:     {}", st.live_objects);
    println!("  gc runs:                 {}", st.gc_runs);
    println!(
        "  mid forced gc:           {} reclaimed, {} live, {} us",
        mid.reclaimed_this_run, mid.live_after, mid.pause_us
    );
    for (name, h) in [("producer", prod), ("consumer", cons)] {
        let (base, limit) = s.stack_range(h);
        println!(
            "  task {name}: base=0x{base:x} limit=0x{limit:x} usable={} high_water={:?}",
            s.stack_usable(h),
            s.high_water(h),
        );
    }
    println!("  total coroutine resumes: {}", s.switches());

    // --- Switch-cost micro-scenario: two tasks, 2 x 5000 yields.
    let mut t = Scheduler::new();
    let a = t.spawn("a", DEFAULT_STACK_BYTES, ping);
    let b = t.spawn("b", DEFAULT_STACK_BYTES, ping);
    let t0 = Instant::now();
    t.join(a);
    t.join(b);
    let dt = t0.elapsed();
    let n = t.switches() as f64;
    println!(
        "  switch cost: {:.1} ns/resume ({} resumes in {:?}, debug build)",
        dt.as_nanos() as f64 / n,
        t.switches(),
        dt,
    );
}
