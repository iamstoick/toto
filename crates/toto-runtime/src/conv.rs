//! Value conversion between executor values (`RtVal`) and the
//! compiler's tree-walking values (`interp::Value`).
//!
//! Builtin calls and `ops` operations run on `interp::Value` through the
//! shared `builtins`/`ops` modules, so the runtime agrees with the
//! sequential interpreters by construction. Conversion is memoized:
//! sharing-preserving AND cycle-safe (the same `GcRef` maps to the same
//! `Rc`), so cyclic heap structures convert to cyclic `Value` graphs,
//! which `ops` and `format_value` already handle exactly as in the
//! interpreters. `push`/`pop`/`len` and field/index reads are implemented
//! natively in the executor (see `exec.rs`) to preserve aliasing; only
//! those paths skip conversion.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use toto_compiler::interp::{ClosureCode, ClosureVal, StructVal, Value};

use crate::heap::{GcRef, Heap, Payload};
use crate::value::{RtClosure, RtVal};

/// Conversion can only fail on function values at first-order positions.
/// Checked programs never produce those (builtin results are never
/// functions). Channels map to `"<chan>"` for display: the only builtin
/// that accepts one is `print`, and every other builtin rejects channels
/// statically — so the mapping is unreachable elsewhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvError {
    Function,
}

/// Memoized conversion to tree-walking values. Callers share one memo
/// across a whole instruction's operands so cross-operand sharing is
/// preserved too (`a == a` on cyclic `a` stays true).
pub fn to_value_memo(
    v: &RtVal,
    heap: &Heap,
    memo: &mut HashMap<GcRef, Value>,
) -> Result<Value, ConvError> {
    match v {
        RtVal::Int(n) => Ok(Value::Int(*n)),
        RtVal::Float(f) => Ok(Value::Float(*f)),
        RtVal::Bool(b) => Ok(Value::Bool(*b)),
        RtVal::Unit => Ok(Value::Unit),
        RtVal::None => Ok(Value::None),
        RtVal::Range(a, b) => Ok(Value::Range(*a, *b)),
        RtVal::Heap(r) => {
            if let Some(shared) = memo.get(r) {
                return Ok(shared.clone());
            }
            match heap.get(*r) {
                Some(Payload::Str(s)) => Ok(Value::Str(Rc::new(s.clone()))),
                Some(Payload::Array(elems)) => {
                    let cell = Rc::new(RefCell::new(Vec::new()));
                    memo.insert(*r, Value::Array(cell.clone()));
                    for e in elems {
                        let c = to_value_memo(e, heap, memo)?;
                        cell.borrow_mut().push(c);
                    }
                    Ok(Value::Array(cell))
                }
                Some(Payload::Struct { name, fields }) => {
                    let cell = Rc::new(RefCell::new(StructVal {
                        name: name.clone(),
                        fields: Vec::new(),
                    }));
                    memo.insert(*r, Value::Struct(cell.clone()));
                    for (n, e) in fields {
                        let c = to_value_memo(e, heap, memo)?;
                        cell.borrow_mut().fields.push((n.clone(), c));
                    }
                    Ok(Value::Struct(cell))
                }
                // JIT value families (Phase 4). Not memo-shelled like
                // arrays/structs: cycles through wrappers are untypable
                // in checked programs (infinite types), and neither
                // executor builds them, so plain recursion terminates.
                Some(Payload::Range { lo, hi }) => Ok(Value::Range(*lo, *hi)),
                Some(Payload::Result { is_ok, inner }) => {
                    let c = Box::new(to_value_memo(inner, heap, memo)?);
                    if *is_ok {
                        Ok(Value::Ok(c))
                    } else {
                        Ok(Value::Err(c))
                    }
                }
                Some(Payload::Option { inner }) => {
                    Ok(Value::Some(Box::new(to_value_memo(inner, heap, memo)?)))
                }
                Some(Payload::Closure { .. }) => {
                    // Heap closures surface here only for display
                    // (`<closure>`) and type names: code identity is
                    // irrelevant, so a dummy body.
                    Ok(Value::Closure(Rc::new(ClosureVal {
                        code: ClosureCode::Mir {
                            params: Vec::new(),
                            ret: toto_compiler::hir::Ty::Unit,
                            body: std::sync::Arc::new(toto_compiler::mir::MirBody {
                                params: Vec::new(),
                                ret: toto_compiler::hir::Ty::Unit,
                                captures: Vec::new(),
                                locals: Vec::new(),
                                blocks: Vec::new(),
                                entry: 0,
                                body_span: toto_compiler::span::Span::new(0, 0),
                            }),
                        },
                        captured: Vec::new(),
                    })))
                }
                None => Ok(Value::Unit),
            }
        }
        RtVal::Chan(_) => Ok(Value::Str(Rc::new("<chan>".to_string()))),
        RtVal::Func(idx) => Ok(Value::Func(*idx)),
        RtVal::Closure(c) => {
            let RtClosure { body, captured, .. } = &**c;
            let mut out = Vec::with_capacity(captured.len());
            for ((name, _), v) in body.captures.iter().zip(captured.iter()) {
                out.push((name.clone(), to_value_memo(v, heap, memo)?));
            }
            Ok(Value::Closure(Rc::new(ClosureVal {
                code: ClosureCode::Mir {
                    params: body.params.clone(),
                    ret: body.ret.clone(),
                    body: body.clone(),
                },
                captured: out,
            })))
        }
        RtVal::Ok(inner) => Ok(Value::Ok(Box::new(to_value_memo(inner, heap, memo)?))),
        RtVal::Err(inner) => Ok(Value::Err(Box::new(to_value_memo(inner, heap, memo)?))),
        RtVal::Some(inner) => Ok(Value::Some(Box::new(to_value_memo(inner, heap, memo)?))),
    }
}

/// Fresh-memo conversion (single value; see `to_value_memo`).
pub fn to_value(v: &RtVal, heap: &Heap) -> Result<Value, ConvError> {
    to_value_memo(v, heap, &mut HashMap::new())
}

/// Memoized conversion back. The same source allocation maps to the same
/// `GcRef` (shell allocated first, so cycles resolve). Builtin results
/// are fresh and acyclic; the memo matters for write-back of mutated
/// containers, which may share structure with the heap.
pub fn from_value_memo(
    v: &Value,
    heap: &mut Heap,
    memo: &mut HashMap<*const (), GcRef>,
) -> Result<RtVal, ConvError> {
    match v {
        Value::Int(n) => Ok(RtVal::Int(*n)),
        Value::Float(f) => Ok(RtVal::Float(*f)),
        Value::Bool(b) => Ok(RtVal::Bool(*b)),
        Value::Unit => Ok(RtVal::Unit),
        Value::None => Ok(RtVal::None),
        Value::Range(a, b) => Ok(RtVal::Range(*a, *b)),
        Value::Str(s) => Ok(RtVal::Heap(heap.alloc_str(s))),
        Value::Array(cell) => {
            let key = Rc::as_ptr(cell) as *const ();
            if let Some(r) = memo.get(&key) {
                return Ok(RtVal::Heap(*r));
            }
            let r = heap.alloc_array(Vec::new());
            memo.insert(key, r);
            let mut out = Vec::with_capacity(cell.borrow().len());
            for e in cell.borrow().iter() {
                out.push(from_value_memo(e, heap, memo)?);
            }
            if let Some(Payload::Array(elems)) = heap.get_mut(r) {
                *elems = out;
            }
            Ok(RtVal::Heap(r))
        }
        Value::Struct(cell) => {
            let key = Rc::as_ptr(cell) as *const ();
            if let Some(r) = memo.get(&key) {
                return Ok(RtVal::Heap(*r));
            }
            let name = cell.borrow().name.clone();
            let r = heap.alloc_struct(&name, Vec::new());
            memo.insert(key, r);
            let mut out = Vec::with_capacity(cell.borrow().fields.len());
            for (n, e) in cell.borrow().fields.iter() {
                out.push((n.clone(), from_value_memo(e, heap, memo)?));
            }
            if let Some(Payload::Struct { fields, .. }) = heap.get_mut(r) {
                *fields = out;
            }
            Ok(RtVal::Heap(r))
        }
        Value::Ok(inner) => Ok(RtVal::Ok(Box::new(from_value_memo(inner, heap, memo)?))),
        Value::Err(inner) => Ok(RtVal::Err(Box::new(from_value_memo(inner, heap, memo)?))),
        Value::Some(inner) => Ok(RtVal::Some(Box::new(from_value_memo(inner, heap, memo)?))),
        Value::Closure(_) | Value::Func(_) => Err(ConvError::Function),
    }
}

/// Fresh-memo conversion back (see `from_value_memo`).
pub fn from_value(v: &Value, heap: &mut Heap) -> Result<RtVal, ConvError> {
    from_value_memo(v, heap, &mut HashMap::new())
}
