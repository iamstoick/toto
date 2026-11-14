//! Non-moving mark-sweep heap with type descriptors in object headers.
//!
//! Every object header carries a `&'static TypeDesc` that tells the marker
//! which payload slots hold heap references. The marker never guesses:
//! a slot is followed only if the object's descriptor says it is a
//! reference slot. This is the "descriptors in headers" option from D1
//! (chosen over a side table of pointer maps because the header already
//! carries a type tag per D8, so the map rides along for free).

use std::time::Instant;

/// Handle to a heap object. `(index, generation)` pairs make stale handles
/// detectable: reusing a freed slot bumps its generation, so a handle to
/// a reclaimed object resolves to `None` instead of a stranger's object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GcRef {
    index: u32,
    generation: u32,
}

/// How a descriptor identifies reference fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefMap {
    /// No slot holds a heap reference (ints, strings).
    NoRefs,
    /// Bit `i` set means `slots[i]` may hold a `GcRef` (fixed shapes).
    Mask(u64),
    /// Every slot holds a `GcRef` (variable-length arrays).
    AllRefs,
}

/// Header-adjacent type descriptor. One static per object shape.
#[derive(Debug)]
pub struct TypeDesc {
    pub name: &'static str,
    pub refs: RefMap,
}

pub static DESC_INT: TypeDesc = TypeDesc {
    name: "Int",
    refs: RefMap::NoRefs,
};
pub static DESC_STR: TypeDesc = TypeDesc {
    name: "Str",
    refs: RefMap::NoRefs,
};
pub static DESC_PAIR: TypeDesc = TypeDesc {
    name: "Pair",
    refs: RefMap::Mask(0b11),
};
pub static DESC_ARRAY: TypeDesc = TypeDesc {
    name: "Array",
    refs: RefMap::AllRefs,
};

/// One payload slot. Only `Slot::Ref` is ever followed by the marker,
/// and only when the object's descriptor allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Int(i64),
    Ref(GcRef),
    Nil,
}

/// Object payload. `Text` carries no slots, so it needs no ref map.
#[derive(Debug)]
pub enum Payload {
    Slots(Vec<Slot>),
    Text(String),
}

#[derive(Debug)]
pub struct Header {
    pub desc: &'static TypeDesc,
    pub marked: bool,
}

#[derive(Debug)]
struct Entry {
    generation: u32,
    header: Header,
    payload: Payload,
}

/// Cumulative heap statistics. Reported by the demo binary and the tests;
// rough spike numbers, not benchmark-grade.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    pub allocated_objects: u64,
    pub allocated_bytes_approx: u64,
    pub reclaimed_objects: u64,
    pub live_objects: usize,
    pub max_live_objects: usize,
    pub gc_runs: u64,
    pub last_pause_us: u128,
    pub total_pause_us: u128,
}

#[derive(Clone, Copy, Debug)]
pub struct GcReport {
    pub reclaimed_this_run: usize,
    pub live_after: usize,
    pub pause_us: u128,
}

pub struct Heap {
    entries: Vec<Option<Entry>>,
    generations: Vec<u32>,
    free: Vec<u32>,
    bytes_since_gc: u64,
    stats: HeapStats,
}

impl Heap {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            generations: Vec::new(),
            free: Vec::new(),
            bytes_since_gc: 0,
            stats: HeapStats::default(),
        }
    }

    pub fn stats(&self) -> HeapStats {
        self.stats
    }

    pub fn bytes_since_gc(&self) -> u64 {
        self.bytes_since_gc
    }

    fn insert(&mut self, desc: &'static TypeDesc, payload: Payload) -> GcRef {
        let bytes = 32
            + match &payload {
                Payload::Slots(s) => s.len() as u64 * 8,
                Payload::Text(t) => t.len() as u64,
            };
        let r = match self.free.pop() {
            Some(index) => {
                self.generations[index as usize] += 1;
                let generation = self.generations[index as usize];
                self.entries[index as usize] = Some(Entry {
                    generation,
                    header: Header {
                        desc,
                        marked: false,
                    },
                    payload,
                });
                GcRef { index, generation }
            }
            None => {
                let index = self.entries.len() as u32;
                self.generations.push(1);
                self.entries.push(Some(Entry {
                    generation: 1,
                    header: Header {
                        desc,
                        marked: false,
                    },
                    payload,
                }));
                GcRef {
                    index,
                    generation: 1,
                }
            }
        };
        self.stats.allocated_objects += 1;
        self.stats.allocated_bytes_approx += bytes;
        self.bytes_since_gc += bytes;
        r
    }

    pub fn alloc_int(&mut self, v: i64) -> GcRef {
        self.insert(&DESC_INT, Payload::Slots(vec![Slot::Int(v)]))
    }

    pub fn alloc_str(&mut self, v: &str) -> GcRef {
        self.insert(&DESC_STR, Payload::Text(v.to_owned()))
    }

    pub fn alloc_pair(&mut self, head: GcRef, tail: GcRef) -> GcRef {
        self.insert(
            &DESC_PAIR,
            Payload::Slots(vec![Slot::Ref(head), Slot::Ref(tail)]),
        )
    }

    pub fn alloc_array(&mut self, elems: &[GcRef]) -> GcRef {
        self.insert(
            &DESC_ARRAY,
            Payload::Slots(elems.iter().map(|&r| Slot::Ref(r)).collect()),
        )
    }

    /// Resolve a handle. `None` means the object was reclaimed (stale
    /// handle) — by design, never a stranger's object.
    pub fn get(&self, r: GcRef) -> Option<(&Header, &Payload)> {
        let e = self.entries.get(r.index as usize)?.as_ref()?;
        if e.generation != r.generation {
            return None;
        }
        Some((&e.header, &e.payload))
    }

    pub fn get_int(&self, r: GcRef) -> Option<i64> {
        match self.get(r)?.1 {
            Payload::Slots(s) => match s.first() {
                Some(Slot::Int(v)) => Some(*v),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn get_pair(&self, r: GcRef) -> Option<(GcRef, GcRef)> {
        match self.get(r)?.1 {
            Payload::Slots(s) => match (s.first(), s.get(1)) {
                (Some(Slot::Ref(a)), Some(Slot::Ref(b))) => Some((*a, *b)),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn get_array_len(&self, r: GcRef) -> Option<usize> {
        match self.get(r)?.1 {
            Payload::Slots(s) => Some(s.len()),
            _ => None,
        }
    }

    pub fn get_str(&self, r: GcRef) -> Option<&str> {
        match self.get(r)?.1 {
            Payload::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn desc_name(&self, r: GcRef) -> Option<&'static str> {
        Some(self.get(r)?.0.desc.name)
    }

    /// TEST-ONLY forced collection hook. Stops the world (the spike is
    /// single-threaded, so the world is just the calling thread), marks
    /// from exactly `roots`, and sweeps everything else.
    ///
    /// Not a stdlib API. Production code must use safepoint collection.
    pub fn gc_collect_for_test(&mut self, roots: &[GcRef]) -> GcReport {
        let t0 = Instant::now();
        for entry in self.entries.iter_mut().flatten() {
            entry.header.marked = false;
        }
        let mut stack: Vec<GcRef> = roots.to_vec();
        while let Some(r) = stack.pop() {
            let Some(i) = self.entries.get_mut(r.index as usize) else {
                continue;
            };
            let Some(e) = i.as_mut() else { continue };
            if e.generation != r.generation || e.header.marked {
                continue;
            }
            e.header.marked = true;
            let slots: &[Slot] = match &e.payload {
                Payload::Slots(s) => s,
                Payload::Text(_) => &[],
            };
            match e.header.desc.refs {
                RefMap::NoRefs => {}
                RefMap::Mask(mask) => {
                    for (i, s) in slots.iter().enumerate() {
                        if i < 64
                            && (mask >> i) & 1 == 1
                            && let Slot::Ref(child) = s
                        {
                            stack.push(*child);
                        }
                    }
                }
                RefMap::AllRefs => {
                    for s in slots {
                        if let Slot::Ref(child) = s {
                            stack.push(*child);
                        }
                    }
                }
            }
        }
        let mut reclaimed = 0;
        let mut live = 0;
        for (index, entry) in self.entries.iter_mut().enumerate() {
            let dead = match entry {
                Some(e) => !e.header.marked,
                None => continue,
            };
            if dead {
                *entry = None;
                self.free.push(index as u32);
                reclaimed += 1;
            } else {
                live += 1;
            }
        }
        let pause_us = t0.elapsed().as_micros();
        self.stats.gc_runs += 1;
        self.stats.reclaimed_objects += reclaimed as u64;
        self.stats.live_objects = live;
        self.stats.max_live_objects = self.stats.max_live_objects.max(live);
        self.stats.last_pause_us = pause_us;
        self.stats.total_pause_us += pause_us;
        self.bytes_since_gc = 0;
        GcReport {
            reclaimed_this_run: reclaimed,
            live_after: live,
            pause_us,
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}
