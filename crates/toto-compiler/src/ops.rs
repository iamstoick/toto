//! Shared value operations (Phase 2, Stage 2).
//!
//! Pure Toto value semantics used by BOTH the tree-walking interpreter
//! and the MIR interpreter: arithmetic, equality, ordering, indexing,
//! field access, unary operators, and `?` propagation. One implementation,
//! so the two executors cannot drift apart; the differential harness
//! (tests) proves they agree on the corpus.
//!
//! Every fallible op returns `(Diag, Vec<TraceFrame>)` with an empty
//! trace; callers merge it into their own trace exactly like a directly
//! raised panic.

use std::rc::Rc;

use crate::ast::BinOp;
use crate::diag::Diag;
use crate::interp::TraceFrame;
use crate::interp::Value;
use crate::span::Span;

pub type OpResult<T> = Result<T, (Diag, Vec<TraceFrame>)>;

fn fail(code: &'static str, message: String, span: Span) -> (Diag, Vec<TraceFrame>) {
    fail_help(code, message, span, None)
}

fn fail_help(
    code: &'static str,
    message: String,
    span: Span,
    help: Option<String>,
) -> (Diag, Vec<TraceFrame>) {
    (Diag::new(code, message, span, help), Vec::new())
}

/// What a function returns, for `?` propagation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FnRetKind {
    Result,
    Option,
    Other,
}

/// Conversion hint shared by all type-mismatch panics.
pub fn int_float_hint(a: &Value, b: &Value) -> Option<String> {
    if matches!(
        (a, b),
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_))
    ) {
        Some(
            "int and float do not mix implicitly; convert explicitly with to_int or to_float"
                .to_string(),
        )
    } else {
        None
    }
}

pub fn arith(
    l: Value,
    r: Value,
    span: Span,
    verb: &str,
    op: fn(i64, i64) -> Option<i64>,
) -> OpResult<Value> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => match op(a, b) {
            Some(n) => Ok(Value::Int(n)),
            None => Err(fail("E0201", "integer overflow".to_string(), span)),
        },
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(match verb {
            "add" => a + b,
            "subtract" => a - b,
            _ => a * b,
        })),
        (Value::Str(a), Value::Str(b)) if verb == "add" => {
            let mut s = a.as_ref().clone();
            s.push_str(b.as_ref());
            Ok(Value::Str(Rc::new(s)))
        }
        (a, b) => Err(fail_help(
            "E0203",
            format!("cannot {verb} {} and {}", a.type_name(), b.type_name()),
            span,
            int_float_hint(&a, &b),
        )),
    }
}

/// Integer division with the explicit zero check (spec §5.2, trap decision
/// D8-Phase-2: explicit check in codegen, so the check is explicit here).
pub fn div_values(l: Value, r: Value, span: Span) -> OpResult<Value> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                return Err(fail("E0202", "division by zero".to_string(), span));
            }
            match a.checked_div(b) {
                Some(n) => Ok(Value::Int(n)),
                None => Err(fail("E0201", "integer overflow".to_string(), span)),
            }
        }
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a / b)),
        (a, b) => Err(fail_help(
            "E0203",
            format!("cannot divide {} by {}", a.type_name(), b.type_name()),
            span,
            int_float_hint(&a, &b),
        )),
    }
}

pub fn mod_values(l: Value, r: Value, span: Span) -> OpResult<Value> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                // Unified Phase 4 wording: every executor reports
                // "division by zero" for `% 0` (E0202, same span).
                // Spec §5.2 pins codes, not messages.
                return Err(fail("E0202", "division by zero".to_string(), span));
            }
            // i64::MIN % -1 is the only overflow; its value is 0.
            Ok(Value::Int(a.wrapping_rem(b)))
        }
        (a, b) => Err(fail_help(
            "E0203",
            format!(
                "cannot take modulo of {} and {}",
                a.type_name(),
                b.type_name()
            ),
            span,
            int_float_hint(&a, &b),
        )),
    }
}

pub fn neg_value(v: Value, inner_span: Span, span: Span) -> OpResult<Value> {
    match v {
        Value::Int(i) => match i.checked_neg() {
            Some(n) => Ok(Value::Int(n)),
            None => Err(fail("E0201", "integer overflow".to_string(), span)),
        },
        Value::Float(f) => Ok(Value::Float(-f)),
        other => Err(fail(
            "E0203",
            format!("cannot negate {}", other.type_name()),
            inner_span,
        )),
    }
}

pub fn not_value(v: Value, inner_span: Span) -> OpResult<Value> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(fail(
            "E0203",
            format!("`!` needs a bool, got {}", other.type_name()),
            inner_span,
        )),
    }
}

pub fn equality(l: Value, r: Value, span: Span, want_eq: bool) -> OpResult<Value> {
    // `Ok`/`Err` share the `Result` family and `Some`/`None` the
    // `Option` family: same-type cross-variant comparison is `false`
    // (the checker guarantees the type arguments agree).
    let same_type = match (&l, &r) {
        (Value::Struct(_), Value::Struct(_)) => struct_name(&l) == struct_name(&r),
        (Value::Closure(_), Value::Func(_)) | (Value::Func(_), Value::Closure(_)) => false,
        (Value::Ok(_) | Value::Err(_), Value::Ok(_) | Value::Err(_)) => true,
        (Value::Some(_) | Value::None, Value::Some(_) | Value::None) => true,
        // Null-pointer checks read naturally (`p == None`); anything
        // else against a pointer is E0203 as usual.
        (Value::Ptr(_), Value::None) | (Value::None, Value::Ptr(_)) => true,
        _ => std::mem::discriminant(&l) == std::mem::discriminant(&r),
    };
    if !same_type {
        return Err(fail_help(
            "E0203",
            format!("cannot compare {} with {}", l.type_name(), r.type_name()),
            span,
            int_float_hint(&l, &r),
        ));
    }
    let eq = values_equal(&l, &r, 0);
    Ok(Value::Bool(if want_eq { eq } else { !eq }))
}

pub fn values_equal(a: &Value, b: &Value, depth: usize) -> bool {
    if depth > 100 {
        // Cycle guard (spec §5.4): fall back to identity.
        return match (a, b) {
            (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
            (Value::Struct(x), Value::Struct(y)) => Rc::ptr_eq(x, y),
            _ => false,
        };
    }
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Unit, Value::Unit) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Range(a, b), Value::Range(c, d)) => a == c && b == d,
        (Value::Closure(x), Value::Closure(y)) => Rc::ptr_eq(x, y),
        (Value::Func(x), Value::Func(y)) => x == y,
        (Value::Ok(x), Value::Ok(y)) => values_equal(x, y, depth + 1),
        (Value::Err(x), Value::Err(y)) => values_equal(x, y, depth + 1),
        (Value::Ok(_), Value::Err(_)) | (Value::Err(_), Value::Ok(_)) => false,
        (Value::Some(x), Value::Some(y)) => values_equal(x, y, depth + 1),
        (Value::None, Value::None) => true,
        (Value::Some(_), Value::None) | (Value::None, Value::Some(_)) => false,
        // Pointers compare by address (never printed, so no ASLR leak).
        (Value::Ptr(x), Value::Ptr(y)) => x == y,
        (Value::Ptr(p), Value::None) | (Value::None, Value::Ptr(p)) => *p == 0,
        (Value::Array(x), Value::Array(y)) => {
            if Rc::ptr_eq(x, y) {
                return true;
            }
            let x = x.borrow();
            let y = y.borrow();
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(e, f)| values_equal(e, f, depth + 1))
        }
        (Value::Struct(x), Value::Struct(y)) => {
            if Rc::ptr_eq(x, y) {
                return true;
            }
            let x = x.borrow();
            let y = y.borrow();
            x.name == y.name
                && x.fields.len() == y.fields.len()
                && x.fields
                    .iter()
                    .zip(y.fields.iter())
                    .all(|((n, e), (m, f))| n == m && values_equal(e, f, depth + 1))
        }
        _ => false,
    }
}

pub fn ordering(op: BinOp, l: Value, r: Value, span: Span) -> OpResult<Value> {
    let ord = match (&l, &r) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => {
            // NaN comparisons are false (IEEE).
            match a.partial_cmp(b) {
                Some(o) => o,
                None => return Ok(Value::Bool(false)),
            }
        }
        (Value::Str(a), Value::Str(b)) => a.cmp(b),
        _ => {
            return Err(fail_help(
                "E0203",
                format!("cannot order {} and {}", l.type_name(), r.type_name()),
                span,
                int_float_hint(&l, &r),
            ));
        }
    };
    let result = match op {
        BinOp::Lt => ord == std::cmp::Ordering::Less,
        BinOp::LtEq => ord != std::cmp::Ordering::Greater,
        BinOp::Gt => ord == std::cmp::Ordering::Greater,
        BinOp::GtEq => ord != std::cmp::Ordering::Less,
        _ => unreachable!("ordering called with non-ordering operator"),
    };
    Ok(Value::Bool(result))
}

pub fn index_read(base: Value, index: Value, span: Span, index_span: Span) -> OpResult<Value> {
    match (base, index) {
        (Value::Array(cell), Value::Int(n)) => {
            let arr = cell.borrow();
            if n < 0 || (n as usize) >= arr.len() {
                return Err(fail(
                    "E0209",
                    format!("index {n} out of bounds for array of length {}", arr.len()),
                    span,
                ));
            }
            Ok(arr[n as usize].clone())
        }
        (Value::Array(_), other) => Err(fail(
            "E0209",
            format!("array index must be int, got {}", other.type_name()),
            index_span,
        )),
        (other, _) => Err(fail(
            "E0209",
            format!("cannot index {}", other.type_name()),
            span,
        )),
    }
}

pub fn index_write(
    base: Value,
    index: Value,
    value: Value,
    span: Span,
    index_span: Span,
) -> OpResult<()> {
    match (base, index) {
        (Value::Array(cell), Value::Int(n)) => {
            let mut arr = cell.borrow_mut();
            if n < 0 || (n as usize) >= arr.len() {
                return Err(fail(
                    "E0209",
                    format!("index {n} out of bounds for array of length {}", arr.len()),
                    span,
                ));
            }
            arr[n as usize] = value;
            Ok(())
        }
        (Value::Array(_), other) => Err(fail(
            "E0209",
            format!("array index must be int, got {}", other.type_name()),
            index_span,
        )),
        (other, _) => Err(fail(
            "E0209",
            format!("cannot index {}", other.type_name()),
            span,
        )),
    }
}

pub fn field_read(base: Value, name: &str, span: Span) -> OpResult<Value> {
    match base {
        Value::Struct(cell) => {
            let s = cell.borrow();
            match s.fields.iter().find(|(n, _)| n == name) {
                Some((_, v)) => Ok(v.clone()),
                None => Err(fail(
                    "E0209",
                    format!("struct {} has no field `{name}`", s.name),
                    span,
                )),
            }
        }
        other => Err(fail(
            "E0209",
            format!("cannot access field of {}", other.type_name()),
            span,
        )),
    }
}

pub fn field_write(base: Value, name: &str, value: Value, span: Span) -> OpResult<()> {
    match base {
        Value::Struct(cell) => {
            let mut s = cell.borrow_mut();
            match s.fields.iter_mut().find(|(n, _)| n == name) {
                Some((_, slot)) => {
                    *slot = value;
                    Ok(())
                }
                None => Err(fail(
                    "E0209",
                    format!("struct {} has no field `{name}`", s.name),
                    span,
                )),
            }
        }
        other => Err(fail(
            "E0209",
            format!("cannot assign field on {}", other.type_name()),
            span,
        )),
    }
}

/// `?` propagation: `Ok`/`Some` unwrap regardless of context; `Err`/`None`
/// early-return only inside a matching function (spec §11, §12.1). The
/// checker guarantees the error type matches, so propagation is identity
/// and the original error value is preserved as the cause. The `E0204` arm
/// is unreachable in checked programs (both interpreters prove it only via
/// the checker, never by executing it).
pub fn propagate(v: Value, enclosing: FnRetKind, span: Span) -> Result<Value, Propagate> {
    match (v, enclosing) {
        (Value::Ok(b), _) => Ok(*b),
        (Value::Some(b), _) => Ok(*b),
        (Value::Err(e), FnRetKind::Result) => Err(Propagate::Return(Value::Err(e))),
        (Value::None, FnRetKind::Option) => Err(Propagate::Return(Value::None)),
        (_, _) => Err(Propagate::Bug(span)),
    }
}

/// Outcome of `?` that is not a plain unwrap.
pub enum Propagate {
    /// Early-return this value from the enclosing function.
    Return(Value),
    /// Unreachable in checked programs; carries the `?` span.
    Bug(Span),
}

fn struct_name(v: &Value) -> String {
    match v {
        Value::Struct(cell) => cell.borrow().name.clone(),
        _ => String::new(),
    }
}
