//! Spike 1 demo: exercises the scheduler, channel, and forced GC,
//! then prints the measured numbers quoted in the spike report.

use spike_1_gc_scheduler::{Action, ChannelId, Cx, Scheduler, Step};

struct Producer {
    ch: ChannelId,
    next: i64,
}

struct Consumer {
    ch: ChannelId,
    sum: i64,
    want: i64,
}

impl Step for Producer {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.next > 20 {
            cx.log("producer done");
            return Action::Done;
        }
        // One deliberately unrooted temp per step: pure garbage that the
        // forced collection must reclaim (documents the rooting rule).
        cx.alloc_int(-self.next);
        // Nested frame: the temporaries die with this call.
        let value = cx.with_frame(|cx| {
            let n = cx.alloc_rooted_int(self.next);
            let tag = cx.alloc_rooted_int(1000 + self.next);
            cx.alloc_pair(n, tag)
        });
        // `value` was popped with the frame, so root it for the send.
        cx.root(value);
        self.next += 1;
        Action::Send(self.ch, value)
    }
}

impl Step for Consumer {
    fn step(&mut self, cx: &mut Cx) -> Action {
        if self.sum >= self.want {
            cx.log(format!("consumer done sum={}", self.sum));
            return Action::Done;
        }
        match cx.take_inbox() {
            None => Action::Recv(self.ch),
            Some(v) => {
                cx.root(v);
                let (head, _) = cx.heap_get_pair(v).expect("pair survives");
                let n = cx.heap_get_int(head).expect("int survives");
                self.sum += n;
                Action::Yield
            }
        }
    }
}

fn main() {
    let mut s = Scheduler::new().with_threshold(4096);
    let ch = s.channel();
    let prod = s.spawn("producer", Box::new(Producer { ch, next: 1 }));
    let cons = s.spawn(
        "consumer",
        Box::new(Consumer {
            ch,
            sum: 0,
            want: 210,
        }),
    );
    s.join(prod);
    let mid = s.gc_collect_for_test();
    s.join(cons);
    s.run_all();

    let st = s.heap_stats();
    println!("spike-1 demo measurements (rough spike numbers):");
    println!("  objects allocated:       {}", st.allocated_objects);
    println!("  bytes allocated (approx): {}", st.allocated_bytes_approx);
    println!("  objects reclaimed:       {}", st.reclaimed_objects);
    println!("  live objects at end:     {}", st.live_objects);
    println!("  max live objects:        {}", st.max_live_objects);
    println!("  gc runs:                 {}", st.gc_runs);
    println!(
        "  last forced gc:          {} reclaimed, {} live, {} us",
        mid.reclaimed_this_run, mid.live_after, mid.pause_us
    );
    println!("  last gc pause (us):      {}", st.last_pause_us);
    println!("  total gc pause (us):     {}", st.total_pause_us);
    println!(
        "  max shadow depth (slots): {}",
        s.max_shadow_depth_observed()
    );
    for line in s.event_log() {
        println!("  log: {line}");
    }
}
