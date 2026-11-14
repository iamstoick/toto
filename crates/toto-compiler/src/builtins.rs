//! Minimal stdlib as interpreter builtins (spec §7). This catalog is
//! FROZEN as of spec approval: no additions without a stop-and-ask.

use std::rc::Rc;

use std::cell::RefCell;

use crate::diag::Diag;
use crate::interp::{Interp, TraceFrame, Value, format_value};
use crate::span::Span;

/// All builtin names. `len` is one name dispatching on string vs array.
pub const BUILTIN_NAMES: &[&str] = &[
    "print",
    "println",
    "len",
    "split",
    "trim",
    "upper",
    "lower",
    "contains",
    "to_string",
    "parse_int",
    "to_int",
    "to_float",
    "is_nan",
    "abs",
    "min",
    "max",
    "sqrt",
    "floor",
    "ceil",
    "push",
    "pop",
    "read_file",
    "write_file",
    "file_exists",
];

pub fn is_builtin(name: &str) -> bool {
    BUILTIN_NAMES.contains(&name)
}

fn fail(code: &'static str, message: String, span: Span) -> (Diag, Vec<TraceFrame>) {
    (Diag::new(code, message, span, None), Vec::new())
}

fn arity(name: &str, args: &[Value], want: usize, span: Span) -> Result<(), (Diag, Vec<TraceFrame>)> {
    if args.len() == want {
        Ok(())
    } else {
        Err(fail(
            "E0204",
            format!("wrong number of arguments: `{name}` takes {want}, got {}", args.len()),
            span,
        ))
    }
}

fn as_str<'v>(v: &'v Value, span: Span, name: &str, pos: usize) -> Result<&'v str, (Diag, Vec<TraceFrame>)> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(fail(
            "E0210",
            format!(
                "`{name}` argument {} must be a string, got {}",
                pos + 1,
                other.type_name()
            ),
            span,
        )),
    }
}

fn as_int(v: &Value, span: Span, name: &str, pos: usize) -> Result<i64, (Diag, Vec<TraceFrame>)> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(fail(
            "E0210",
            format!(
                "`{name}` argument {} must be an int, got {}",
                pos + 1,
                other.type_name()
            ),
            span,
        )),
    }
}

fn as_float(v: &Value, span: Span, name: &str, pos: usize) -> Result<f64, (Diag, Vec<TraceFrame>)> {
    match v {
        Value::Float(n) => Ok(*n),
        other => Err(fail(
            "E0210",
            format!(
                "`{name}` argument {} must be a float, got {}",
                pos + 1,
                other.type_name()
            ),
            span,
        )),
    }
}

type BResult = Result<Value, (Diag, Vec<TraceFrame>)>;

/// Call a builtin. The interpreter owns `output`; print writes there.
pub fn call_builtin(name: &str, args: Vec<Value>, span: Span, interp: &mut Interp) -> BResult {
    match name {
        "print" | "println" => {
            arity(name, &args, 1, span)?;
            let mut text = format_value(&args[0], interp.fns());
            if name == "println" {
                text.push('\n');
            }
            let bytes = text.len() as i64;
            interp.output.extend_from_slice(text.as_bytes());
            Ok(Value::Int(bytes))
        }
        "len" => {
            arity(name, &args, 1, span)?;
            match &args[0] {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::Array(cell) => Ok(Value::Int(cell.borrow().len() as i64)),
                other => Err(fail(
                    "E0210",
                    format!("`len` needs a string or array, got {}", other.type_name()),
                    span,
                )),
            }
        }
        "split" => {
            arity(name, &args, 2, span)?;
            let s = as_str(&args[0], span, name, 0)?.to_string();
            let d = as_str(&args[1], span, name, 1)?.to_string();
            let parts: Vec<Value> = if d.is_empty() {
                s.chars()
                    .map(|c| Value::Str(Rc::new(c.to_string())))
                    .collect()
            } else {
                s.split(d.as_str())
                    .map(|p| Value::Str(Rc::new(p.to_string())))
                    .collect()
            };
            Ok(Value::Array(Rc::new(RefCell::new(parts))))
        }
        "trim" => {
            arity(name, &args, 1, span)?;
            let s = as_str(&args[0], span, name, 0)?;
            Ok(Value::Str(Rc::new(s.trim().to_string())))
        }
        "upper" => {
            arity(name, &args, 1, span)?;
            let s = as_str(&args[0], span, name, 0)?;
            Ok(Value::Str(Rc::new(s.to_uppercase())))
        }
        "lower" => {
            arity(name, &args, 1, span)?;
            let s = as_str(&args[0], span, name, 0)?;
            Ok(Value::Str(Rc::new(s.to_lowercase())))
        }
        "contains" => {
            arity(name, &args, 2, span)?;
            let s = as_str(&args[0], span, name, 0)?;
            let sub = as_str(&args[1], span, name, 1)?;
            Ok(Value::Bool(s.contains(sub)))
        }
        "to_string" => {
            arity(name, &args, 1, span)?;
            match &args[0] {
                Value::Int(n) => Ok(Value::Str(Rc::new(n.to_string()))),
                Value::Float(f) => Ok(Value::Str(Rc::new(format!("{f:?}")))),
                Value::Bool(b) => Ok(Value::Str(Rc::new(b.to_string()))),
                other => Err(fail(
                    "E0210",
                    format!("`to_string` needs int, float, or bool, got {}", other.type_name()),
                    span,
                )),
            }
        }
        "parse_int" => {
            arity(name, &args, 1, span)?;
            let s = as_str(&args[0], span, name, 0)?.trim().to_string();
            let digits = s.strip_prefix(['+', '-']).unwrap_or(&s);
            let valid = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
            if valid {
                match s.parse::<i64>() {
                    Ok(n) => Ok(Value::Int(n)),
                    Err(_) => Err(fail(
                        "E0210",
                        format!("`parse_int` got out-of-range integer literal `{s}`"),
                        span,
                    )),
                }
            } else {
                Err(fail(
                    "E0210",
                    format!("`parse_int` got invalid integer literal `{s}`"),
                    span,
                ))
            }
        }
        "to_int" => {
            arity(name, &args, 1, span)?;
            let f = as_float(&args[0], span, name, 0)?;
            if f.is_nan() || f.is_infinite() || !(f >= -9223372036854775808.0 && f < 9223372036854775808.0) {
                return Err(fail(
                    "E0210",
                    "`to_int` cannot convert NaN, infinity, or out-of-range float".to_string(),
                    span,
                ));
            }
            #[allow(clippy::cast_possible_truncation)]
            Ok(Value::Int(f as i64))
        }
        "to_float" => {
            arity(name, &args, 1, span)?;
            let n = as_int(&args[0], span, name, 0)?;
            #[allow(clippy::cast_precision_loss)]
            Ok(Value::Float(n as f64))
        }
        "is_nan" => {
            arity(name, &args, 1, span)?;
            Ok(Value::Bool(as_float(&args[0], span, name, 0)?.is_nan()))
        }
        "abs" => {
            arity(name, &args, 1, span)?;
            let n = as_int(&args[0], span, name, 0)?;
            match n.checked_abs() {
                Some(v) => Ok(Value::Int(v)),
                None => Err(fail("E0201", "integer overflow".to_string(), span)),
            }
        }
        "min" => {
            arity(name, &args, 2, span)?;
            Ok(Value::Int(as_int(&args[0], span, name, 0)?.min(as_int(&args[1], span, name, 1)?)))
        }
        "max" => {
            arity(name, &args, 2, span)?;
            Ok(Value::Int(as_int(&args[0], span, name, 0)?.max(as_int(&args[1], span, name, 1)?)))
        }
        "sqrt" => {
            arity(name, &args, 1, span)?;
            Ok(Value::Float(as_float(&args[0], span, name, 0)?.sqrt()))
        }
        "floor" => {
            arity(name, &args, 1, span)?;
            Ok(Value::Float(as_float(&args[0], span, name, 0)?.floor()))
        }
        "ceil" => {
            arity(name, &args, 1, span)?;
            Ok(Value::Float(as_float(&args[0], span, name, 0)?.ceil()))
        }
        "push" => {
            arity(name, &args, 2, span)?;
            match &args[0] {
                Value::Array(cell) => {
                    let mut arr = cell.borrow_mut();
                    arr.push(args[1].clone());
                    Ok(Value::Int(arr.len() as i64))
                }
                other => Err(fail(
                    "E0210",
                    format!("`push` needs an array, got {}", other.type_name()),
                    span,
                )),
            }
        }
        "pop" => {
            arity(name, &args, 1, span)?;
            match &args[0] {
                Value::Array(cell) => match cell.borrow_mut().pop() {
                    Some(v) => Ok(v),
                    None => Err(fail("E0210", "pop from empty array".to_string(), span)),
                },
                other => Err(fail(
                    "E0210",
                    format!("`pop` needs an array, got {}", other.type_name()),
                    span,
                )),
            }
        }
        "read_file" => {
            arity(name, &args, 1, span)?;
            let path = as_str(&args[0], span, name, 0)?;
            match std::fs::read_to_string(path) {
                Ok(contents) => Ok(Value::Str(Rc::new(contents))),
                Err(e) => Err(fail(
                    "E0210",
                    format!("couldn't read file `{path}`: {e}"),
                    span,
                )),
            }
        }
        "write_file" => {
            arity(name, &args, 2, span)?;
            let path = as_str(&args[0], span, name, 0)?;
            let contents = as_str(&args[1], span, name, 1)?;
            match std::fs::write(path, contents) {
                Ok(()) => Ok(Value::Int(contents.len() as i64)),
                Err(e) => Err(fail(
                    "E0210",
                    format!("couldn't write file `{path}`: {e}"),
                    span,
                )),
            }
        }
        "file_exists" => {
            arity(name, &args, 1, span)?;
            let path = as_str(&args[0], span, name, 0)?;
            Ok(Value::Bool(std::path::Path::new(path).exists()))
        }
        _ => Err(fail(
            "E0208",
            format!("undefined variable `{name}`"),
            span,
        )),
    }
}
