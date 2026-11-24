//! Production GC heap (Phase 3, Stage 1): stop-the-world non-moving
//! mark-sweep with type descriptors in object headers (D1, frozen ADR-008).
//!
//! Design ported from Spike 1 (`spikes/spike-1-gc-scheduler`, reference
//! only): generational indices detect stale references, tracing follows
//! the payload descriptor. Differences from the spike: payloads hold
//! executor values (`RtVal`) directly (so containers nest without
//! boxing scalars), and shared access goes through the scheduler's
//! `Mutex<Heap>` — no `Rc<RefCell>` anywhere (the ownership redesign
//! required before M:N).

use crate::value::RtVal;

/// A heap reference: slot index plus the generation that owned it.
/// Stale copies (wrong generation, or freed slots) resolve to `None`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GcRef {
    index: u32,
    generation: u32,
}

impl GcRef {
    pub fn from_parts(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    /// Slot index (low 32 bits of the ADR-008 handle codec).
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Generation at allocation (high 32 bits of the codec). A copy
    /// whose generation no longer owns the slot is stale and resolves
    /// to `None` — never to a stranger's object (ADR-008).
    pub fn generation(&self) -> u32 {
        self.generation
    }
}

/// Type descriptor in the object header: tells the tracer which
/// payload fields are references (ADR-008: descriptors, not maps).
/// The Phase 4 JIT families (`Range`, `Result`, `Option`, `Closure`)
/// are heap objects with the frozen handle codec, allocated through
/// the frozen per-family allocator names; the MIR executor keeps its
/// immediate/`Arc` representations and never creates these payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescKind {
    Str,
    Array,
    Struct,
    Range,
    Result,
    Option,
    Closure,
}

/// Heap payload. Composites and JIT value families live on the GC
/// heap; scalars are executor immediates (`RtVal::Int` etc.) and need
/// no tracing. `None` is the immediate word 0, never a payload —
/// only `Some` allocates (`Payload::Option`).
#[derive(Clone, Debug)]
pub enum Payload {
    Str(String),
    Array(Vec<RtVal>),
    Struct {
        name: String,
        fields: Vec<(String, RtVal)>,
    },
    Range {
        lo: i64,
        hi: i64,
    },
    Result {
        is_ok: bool,
        inner: RtVal,
    },
    Option {
        inner: RtVal,
    },
    Closure {
        body: u32,
        captures: Vec<RtVal>,
    },
}

struct Header {
    desc: DescKind,
    marked: bool,
    /// Allocating task id (Wave 4 Stage 2): stamped from the
    /// profiling owner at insert; the sweep tallies per-task
    /// live counts from it. `u32::MAX` means off-task (system).
    owner: u32,
}

struct Slot {
    generation: u32,
    cell: Option<(Header, Payload)>,
}

/// What one collection did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcReport {
    /// Objects reclaimed by this collection.
    pub collected: usize,
    /// Objects still live after this collection.
    pub live: usize,
}

/// Cumulative heap counters (allocation behavior is MEASURED, never
/// asserted in tests — rough numbers, spike practice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HeapStats {
    pub objects_live: usize,
    pub bytes_since_gc: u64,
    pub allocated_total: u64,
    pub reclaimed_total: u64,
    pub collections: u64,
}

pub struct Heap {
    slots: Vec<Slot>,
    free: Vec<u32>,
    bytes_since_gc: u64,
    allocated_total: u64,
    reclaimed_total: u64,
    collections: u64,
    /// Live-slot count, maintained incrementally so `stats()` stays
    /// O(1). Safepoint helpers call `stats()` on hot paths (every
    /// loop/call/entry safepoint in task mode); scanning all slots
    /// there turned per-safepoint cost O(live heap) and made
    /// FFI-heavy task runs super-linear (Phase 7 Stage 2 finding).
    /// Invariant: exactly the number of slots with `cell.is_some()`
    /// — `insert` is the only `None → Some` transition, the
    /// `collect` sweep recounts from scratch.
    live: usize,
}

impl Heap {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            bytes_since_gc: 0,
            allocated_total: 0,
            reclaimed_total: 0,
            collections: 0,
            live: 0,
        }
    }

    pub fn stats(&self) -> HeapStats {
        HeapStats {
            objects_live: self.live,
            bytes_since_gc: self.bytes_since_gc,
            allocated_total: self.allocated_total,
            reclaimed_total: self.reclaimed_total,
            collections: self.collections,
        }
    }

    pub fn bytes_since_gc(&self) -> u64 {
        self.bytes_since_gc
    }

    fn insert(&mut self, desc: DescKind, payload: Payload, bytes: u64) -> GcRef {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                let i = self.slots.len() as u32;
                self.slots.push(Slot {
                    generation: 1,
                    cell: None,
                });
                i
            }
        };
        let slot = &mut self.slots[index as usize];
        let generation = slot.generation.max(1);
        slot.generation = generation;
        slot.cell = Some((
            Header {
                desc,
                marked: false,
                owner: crate::profile::current_owner(),
            },
            payload,
        ));
        self.bytes_since_gc += bytes;
        self.allocated_total += 1;
        self.live += 1;
        GcRef { index, generation }
    }

    /// Approximate accounting: header plus payload bytes. Only the
    /// threshold trigger consumes this; tests never assert exact bytes.
    pub fn alloc_str(&mut self, v: &str) -> GcRef {
        let bytes = 32 + v.len() as u64;
        self.insert(DescKind::Str, Payload::Str(v.to_string()), bytes)
    }

    pub fn alloc_array(&mut self, elems: Vec<RtVal>) -> GcRef {
        let bytes = 32 + 16 * elems.len() as u64;
        self.insert(DescKind::Array, Payload::Array(elems), bytes)
    }

    pub fn alloc_struct(&mut self, name: &str, fields: Vec<(String, RtVal)>) -> GcRef {
        let bytes = 32 + fields.iter().map(|(n, _)| 16 + n.len() as u64).sum::<u64>();
        self.insert(
            DescKind::Struct,
            Payload::Struct {
                name: name.to_string(),
                fields,
            },
            bytes,
        )
    }

    /// JIT `Range` value (frozen `toto_alloc_range` family).
    pub fn alloc_range(&mut self, lo: i64, hi: i64) -> GcRef {
        self.insert(DescKind::Range, Payload::Range { lo, hi }, 32 + 16)
    }

    /// JIT `Ok`/`Err` value (frozen `toto_alloc_result` family).
    pub fn alloc_result(&mut self, is_ok: bool, inner: RtVal) -> GcRef {
        self.insert(DescKind::Result, Payload::Result { is_ok, inner }, 32 + 16)
    }

    /// JIT `Some` value (frozen `toto_alloc_option` family). `None`
    /// is the immediate 0 and never allocates.
    pub fn alloc_some(&mut self, inner: RtVal) -> GcRef {
        self.insert(DescKind::Option, Payload::Option { inner }, 32 + 16)
    }

    /// JIT closure value (frozen `toto_alloc_closure` family):
    /// compiled-body id plus by-value captures (spec §5.7). Captures
    /// are traced like array elements.
    pub fn alloc_closure(&mut self, body: u32, captures: Vec<RtVal>) -> GcRef {
        let bytes = 32 + 16 * captures.len() as u64;
        self.insert(
            DescKind::Closure,
            Payload::Closure { body, captures },
            bytes,
        )
    }

    fn resolve(&self, r: GcRef) -> Option<usize> {
        let slot = self.slots.get(r.index as usize)?;
        match &slot.cell {
            Some(_) if slot.generation == r.generation => Some(r.index as usize),
            _ => None,
        }
    }

    pub fn get(&self, r: GcRef) -> Option<&Payload> {
        let i = self.resolve(r)?;
        self.slots[i].cell.as_ref().map(|(_, p)| p)
    }

    pub fn get_mut(&mut self, r: GcRef) -> Option<&mut Payload> {
        let i = self.resolve(r)?;
        self.slots[i].cell.as_mut().map(|(_, p)| p)
    }

    pub fn desc(&self, r: GcRef) -> Option<DescKind> {
        let i = self.resolve(r)?;
        self.slots[i].cell.as_ref().map(|(h, _)| h.desc)
    }

    /// Liveness query for code-version retention (Phase 6, Stage 2a):
    /// true iff the slot still holds this generation's object.
    ///
    /// Exactness property (pinned by `gc.rs`): NO under-retention —
    /// every object reachable at the last collection resolves, and
    /// every allocated-but-uncollected object resolves. The only
    /// imprecision is bounded OVER-retention: unreachable objects
    /// resolve until the next collection sweeps them (released at
    /// the next forced collection). Retire checks force-collect
    /// first, so decisions are exact; between checks a dead
    /// closure may retain its module for at most one collection.
    pub fn is_live(&self, r: GcRef) -> bool {
        self.resolve(r).is_some()
    }

    /// Every resident closure payload as `(handle, body)`: handles
    /// are generation-checked (stale copies excluded), bodies are
    /// the compiled-function ids meaningful to the owning module.
    /// Liveness is NOT decided here — filter with `is_live` after
    /// a collection for exact liveness (see its bound above).
    /// Used by the version-retention check: a module is retained
    /// while any resident closure names it.
    pub fn closure_refs(&self) -> Vec<(GcRef, u32)> {
        let mut out = Vec::new();
        for (i, slot) in self.slots.iter().enumerate() {
            let Some((_, Payload::Closure { body, .. })) = &slot.cell else {
                continue;
            };
            let r = GcRef::from_parts(i as u32, slot.generation);
            // Generation always matches here (same slot just read),
            // but route through `resolve` so the invariant has one
            // home.
            if self.resolve(r).is_some() {
                out.push((r, *body));
            }
        }
        out
    }

    /// Stop-the-world collection over `roots`. Single-threaded in Stage
    /// 1; M:N coordinates at safepoints (Stage 2). Returns exact
    /// reclaimed/live counts (spike practice: tests assert these).
    pub fn collect(&mut self, roots: &[GcRef]) -> GcReport {
        // Mark. Child references are extracted under a short borrow;
        // resolution happens after, so no borrow is held across.
        let mut stack: Vec<usize> = Vec::new();
        for r in roots {
            if let Some(i) = self.resolve(*r) {
                stack.push(i);
            }
        }
        while let Some(i) = stack.pop() {
            let kids: Vec<GcRef> = {
                let slot = &mut self.slots[i];
                let Some((header, payload)) = slot.cell.as_mut() else {
                    continue;
                };
                if header.marked {
                    continue;
                }
                header.marked = true;
                let mut kids = Vec::new();
                match payload {
                    Payload::Str(_) => {}
                    Payload::Array(elems) => {
                        for v in elems.iter() {
                            crate::value::heap_refs_of(v, &mut kids);
                        }
                    }
                    Payload::Struct { fields, .. } => {
                        for (_, v) in fields.iter() {
                            crate::value::heap_refs_of(v, &mut kids);
                        }
                    }
                    Payload::Range { .. } => {}
                    Payload::Result { inner, .. } | Payload::Option { inner } => {
                        crate::value::heap_refs_of(inner, &mut kids);
                    }
                    Payload::Closure { captures, .. } => {
                        for v in captures.iter() {
                            crate::value::heap_refs_of(v, &mut kids);
                        }
                    }
                }
                kids
            };
            for r in kids {
                if let Some(j) = self.resolve(r) {
                    stack.push(j);
                }
            }
        }
        // Sweep.
        let mut collected = 0usize;
        let mut live = 0usize;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            let take = match &mut slot.cell {
                Some((header, _)) if header.marked => {
                    header.marked = false;
                    live += 1;
                    false
                }
                Some(_) => true,
                None => false,
            };
            if take {
                slot.cell = None;
                slot.generation = slot.generation.wrapping_add(1).max(1);
                self.free.push(i as u32);
                collected += 1;
            }
        }
        self.bytes_since_gc = 0;
        self.reclaimed_total += collected as u64;
        self.collections += 1;
        self.live = live;
        GcReport { collected, live }
    }

    /// Approximate per-object bytes (Wave 4 Stage 2): mirrors the
    /// estimates in the `alloc_*` constructors (same basis as
    /// `bytes_since_gc`, so tallies stay consistent with it).
    fn payload_bytes(payload: &Payload) -> u64 {
        match payload {
            Payload::Str(v) => 32 + v.len() as u64,
            Payload::Array(elems) => 32 + 16 * elems.len() as u64,
            Payload::Struct { fields, .. } => {
                32 + fields.iter().map(|(n, _)| 16 + n.len() as u64).sum::<u64>()
            }
            Payload::Range { .. } => 32 + 16,
            Payload::Result { .. } => 32 + 16,
            Payload::Option { .. } => 32 + 16,
            Payload::Closure { captures, .. } => 32 + 16 * captures.len() as u64,
        }
    }

    /// Per-task + per-type live tally over the current heap (Wave 4
    /// Stage 2): call right after a collection for exact numbers
    /// (every object tagged at insert; the sweep's `live` set is
    /// what is tallied — unreachable objects already freed).
    pub fn profile_tally(&self) -> crate::profile::HeapProfile {
        use std::collections::HashMap;
        let mut tasks: HashMap<u32, crate::profile::TaskHeap> = HashMap::new();
        let mut kinds: HashMap<String, usize> = HashMap::new();
        for slot in self.slots.iter() {
            // `cell.is_some()` is exactly the sweep's live set
            // (freed slots hold `None` until reused).
            let Some((header, payload)) = slot.cell.as_ref() else {
                continue;
            };
            let t = tasks.entry(header.owner).or_default();
            t.objects += 1;
            t.bytes += Self::payload_bytes(payload);
            *kinds.entry(format!("{:?}", header.desc)).or_insert(0) += 1;
        }
        crate::profile::HeapProfile {
            tasks: tasks.into_iter().collect(),
            kinds: kinds.into_iter().collect(),
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}
