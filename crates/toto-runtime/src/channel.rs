//! Channels (spec §13.1): unbuffered rendezvous (`chan<T>()`) and
//! bounded (`chan<T>(n)`) channels with `close`/`closed`. Data lives
//! here; rendezvous POLICY lives in the scheduler (`sched.rs`) — the
//! executor never touches channels directly (uniform yield model).
//!
//! A channel handle (`RtVal::Chan`) is scheduler-owned: the table holds
//! the only strong state, and queued values are GC roots gathered at
//! every collection. Handles themselves are never traced (cloning a
//! handle does not need the GC).

use std::collections::VecDeque;

use toto_compiler::hir::Ty;
use toto_compiler::span::Span;

use crate::value::RtVal;

/// Task identifier: index into the scheduler's slot table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TaskId(pub usize);

/// Channel identifier: index into the channel table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChannelId(pub usize);

/// A parked sender: value held in the waiter (a GC root while parked).
#[derive(Clone, Debug)]
pub struct SendWaiter {
    pub task: TaskId,
    pub value: RtVal,
    pub span: Span,
}

/// A parked receiver.
#[derive(Clone, Debug)]
pub struct RecvWaiter {
    pub task: TaskId,
    pub span: Span,
}

#[derive(Debug)]
pub struct Channel {
    /// Element type (for zero values on closed-drain and diagnostics).
    pub elem: Ty,
    /// Buffer capacity. `0` = unbuffered rendezvous (no buffer).
    pub cap: usize,
    pub buf: VecDeque<RtVal>,
    pub closed: bool,
    pub send_waiters: VecDeque<SendWaiter>,
    pub recv_waiters: VecDeque<RecvWaiter>,
}

impl Channel {
    fn new(elem: Ty, cap: usize) -> Self {
        Self {
            elem,
            cap,
            buf: VecDeque::new(),
            closed: false,
            send_waiters: VecDeque::new(),
            recv_waiters: VecDeque::new(),
        }
    }

    /// A parked task counts as suspended work (for deadlock detection).
    pub fn parked_count(&self) -> usize {
        self.send_waiters.len() + self.recv_waiters.len()
    }
}

#[derive(Debug, Default)]
pub struct ChannelTable {
    channels: Vec<Channel>,
}

impl ChannelTable {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
        }
    }

    pub fn create(&mut self, elem: Ty, cap: usize) -> ChannelId {
        let id = self.channels.len();
        self.channels.push(Channel::new(elem, cap));
        ChannelId(id)
    }

    pub fn get(&self, id: ChannelId) -> Option<&Channel> {
        self.channels.get(id.0)
    }

    pub fn get_mut(&mut self, id: ChannelId) -> Option<&mut Channel> {
        self.channels.get_mut(id.0)
    }

    /// Every queued or in-flight value is a GC root.
    pub fn roots(&self) -> Vec<RtVal> {
        let mut out = Vec::new();
        for ch in &self.channels {
            out.extend(ch.buf.iter().cloned());
            out.extend(ch.send_waiters.iter().map(|w| w.value.clone()));
        }
        out
    }

    pub fn parked_count(&self) -> usize {
        self.channels.iter().map(Channel::parked_count).sum()
    }

    /// Queued (buffered + in-flight waiter-held) values on one channel
    /// (Phase 6, Stage 3): the Q4 revalidation count. Post-settle runs
    /// hold no parked waiters (every task joined), so a nonzero count
    /// means buffered values — reachable only on bounded channels,
    /// never on state-declared (always unbuffered) ones.
    pub fn queued_len(&self, id: ChannelId) -> Option<usize> {
        self.channels
            .get(id.0)
            .map(|ch| ch.buf.len() + ch.send_waiters.len())
    }

    /// Drop every waiter entry belonging to `released` (scope-failure
    /// release sweep): stale entries would keep send values rooted
    /// forever and could consume channel values for dead waits.
    /// Returns the number of entries removed.
    pub fn sweep_released(&mut self, released: &std::collections::HashSet<usize>) -> usize {
        let mut removed = 0;
        for ch in &mut self.channels {
            let before = ch.send_waiters.len() + ch.recv_waiters.len();
            ch.send_waiters.retain(|w| !released.contains(&w.task.0));
            ch.recv_waiters.retain(|w| !released.contains(&w.task.0));
            removed += before - (ch.send_waiters.len() + ch.recv_waiters.len());
        }
        removed
    }
}
