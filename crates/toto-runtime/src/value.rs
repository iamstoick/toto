//! Executor values (Phase 3): immediates for scalars, GC-heap
//! references for composites, scheduler-owned channel handles.
//!
//! Value SEMANTICS (arithmetic, equality, ordering, field/index access)
//! are not reimplemented here: the executor converts to the compiler's
//! `interp::Value` and calls the shared `ops` module, so the runtime
//! agrees with the tree-walker by construction. Conversion lives in
//! `conv.rs`.

use std::sync::Arc;

use toto_compiler::hir::{HStructDecl, Ty};
use toto_compiler::mir::MirBody;

use crate::channel::ChannelId;
use crate::heap::{GcRef, Heap};

/// A runtime value. `Clone` is cheap (references are copied, payloads
/// shared) — the GC keeps referents alive, never moved (non-moving).
#[derive(Clone, Debug)]
pub enum RtVal {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
    None,
    Range(i64, i64),
    /// Composite on the GC heap (string, array, struct).
    Heap(GcRef),
    /// Scheduler-owned channel (reference-counted by the scheduler;
    /// never traced by the GC — `Arc` keeps it alive).
    Chan(ChannelId),
    /// Top-level function by index.
    Func(usize),
    /// Closure value: code plus captured values (captured by value at
    /// creation, spec §5.7).
    Closure(Arc<RtClosure>),
    Ok(Box<RtVal>),
    Err(Box<RtVal>),
    Some(Box<RtVal>),
}

#[derive(Clone, Debug)]
pub struct RtClosure {
    pub body: Arc<MirBody>,
    /// Block graph shared with executor frames (precomputed at creation
    /// so calls never clone it; `MirBody.blocks` stays canonical).
    pub blocks: Arc<[toto_compiler::mir::MBlock]>,
    pub captured: Vec<RtVal>,
}

impl RtVal {
    /// Runtime type name for diagnostics. Mirrors the tree-walker's
    /// `Value::type_name` exactly (heap payloads need a heap read).
    pub fn type_name(&self, heap: &Heap) -> &'static str {
        match self {
            RtVal::Int(_) => "int",
            RtVal::Float(_) => "float",
            RtVal::Bool(_) => "bool",
            RtVal::Unit => "unit",
            RtVal::None => "Option",
            RtVal::Range(..) => "Range",
            RtVal::Heap(r) => match heap.desc(*r) {
                Some(crate::heap::DescKind::Str) => "string",
                Some(crate::heap::DescKind::Array) => "Array",
                Some(crate::heap::DescKind::Struct) => "struct",
                None => "unknown",
            },
            RtVal::Chan(_) => "chan",
            RtVal::Func(_) | RtVal::Closure(_) => "function",
            RtVal::Ok(_) | RtVal::Err(_) => "Result",
            RtVal::Some(_) => "Option",
        }
    }
}

/// Collect every heap reference reachable from `v` (through `Ok` /
/// `Err` / `Some` wrappers). Used for root publication and gathering.
pub fn heap_refs_of(v: &RtVal, out: &mut Vec<GcRef>) {
    match v {
        RtVal::Heap(r) => out.push(*r),
        RtVal::Ok(inner) | RtVal::Err(inner) | RtVal::Some(inner) => heap_refs_of(inner, out),
        _ => {}
    }
}

/// The zero value of `T` (spec §13.1, recv on closed-and-drained).
/// Needs the heap (composites allocate) and struct declarations.
/// `Err` for element types with no zero value (`chan`, `fn`, `Map`
/// — `Map` has values only as a type, never as a runtime value).
pub fn zero_value(
    ty: &Ty,
    heap: &mut Heap,
    structs: &std::collections::HashMap<String, HStructDecl>,
) -> Option<RtVal> {
    match ty {
        Ty::Int => Some(RtVal::Int(0)),
        Ty::Float => Some(RtVal::Float(0.0)),
        Ty::String => Some(RtVal::Heap(heap.alloc_str(""))),
        Ty::Bool => Some(RtVal::Bool(false)),
        Ty::Unit => Some(RtVal::Unit),
        Ty::Range => Some(RtVal::Range(0, 0)),
        Ty::Array(_) => Some(RtVal::Heap(heap.alloc_array(Vec::new()))),
        Ty::Option(_) => Some(RtVal::None),
        Ty::Result(t, _) => Some(RtVal::Ok(Box::new(zero_value(t, heap, structs)?))),
        Ty::Struct(name) => {
            let decl = structs.get(name)?;
            let mut fields = Vec::with_capacity(decl.fields.len());
            for (fname, fty) in &decl.fields {
                fields.push((fname.clone(), zero_value(fty, heap, structs)?));
            }
            Some(RtVal::Heap(heap.alloc_struct(name, fields)))
        }
        Ty::Chan(_) | Ty::Fn(..) | Ty::Map(..) => None,
    }
}
