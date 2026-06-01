//! The curated **builtin catalog** and its reference implementations (`PLAN.md` Phase 6).
//!
//! Grindlang ships no ambient stdlib; it offers a small, deterministic, pure subset of
//! `math.*` and `string.*` plus the plain `tostring`/`tonumber` conversions. This module is
//! the **single source of truth** for those builtins:
//!
//!   * The **catalog** ([`value_sig`], [`member_sig`], [`namespace_field_type`]) describes
//!     each builtin's signature in [`Type`] terms. The type checker ([`crate::types`]) and
//!     the IR lowering ([`crate::ir`]) both consult it, so a builtin's type lives in exactly
//!     one place. It is always compiled (no `interp`/`jit` feature needed).
//!   * The **implementations** (behind the `interp` feature) evaluate a builtin over runtime
//!     [`Value`](crate::value::Value)s. Both the AST interpreter and the IR `Vm` call into
//!     them, so the builtins also have a single executable definition. The (Phase 7) JIT
//!     will lower calls to the same catalog entries (emitting native code or runtime calls).

use crate::types::Type;

/// How a builtin's arguments are checked, when a plain positional parameter list can't fully
/// express it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgRule {
    /// Each argument is checked positionally against [`Sig::params`].
    Fixed,
    /// A single argument that must be a *printable scalar* (`number`, `bool`, or `string`).
    /// Used by `tostring`. [`Sig::params`] is empty.
    Scalar,
    /// `params[0]` is a format string; any further arguments are accepted unchecked. Used by
    /// `string.format`.
    FormatVariadic,
}

/// How many arguments a builtin accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arity {
    Exact(usize),
    AtLeast(usize),
}

/// The signature of a builtin: positionally checked parameter types, return type, argument
/// discipline, and arity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sig {
    pub params: Vec<Type>,
    pub ret: Type,
    pub rule: ArgRule,
    pub arity: Arity,
}

/// Signature of a plain value builtin: `tostring` or `tonumber`. Returns `None` for any
/// other name.
pub fn value_sig(name: &str) -> Option<Sig> {
    Some(match name {
        "tostring" => Sig {
            params: vec![],
            ret: Type::String,
            rule: ArgRule::Scalar,
            arity: Arity::Exact(1),
        },
        "tonumber" => Sig {
            params: vec![Type::String],
            ret: Type::optional(Type::Number),
            rule: ArgRule::Fixed,
            arity: Arity::Exact(1),
        },
        _ => return None,
    })
}

/// Signature of a namespace member builtin (`math.*`, `string.*`). Returns `None` if the
/// namespace has no such member.
pub fn member_sig(ns: &str, member: &str) -> Option<Sig> {
    let fixed = |params: Vec<Type>, ret: Type| Sig {
        arity: Arity::Exact(params.len()),
        params,
        ret,
        rule: ArgRule::Fixed,
    };
    Some(match (ns, member) {
        ("math", "floor") | ("math", "ceil") | ("math", "abs") | ("math", "sqrt") => {
            fixed(vec![Type::Number], Type::Number)
        }
        ("math", "min") | ("math", "max") | ("math", "pow") => {
            fixed(vec![Type::Number, Type::Number], Type::Number)
        }
        ("string", "len") => fixed(vec![Type::String], Type::Number),
        ("string", "upper") | ("string", "lower") => fixed(vec![Type::String], Type::String),
        ("string", "sub") => fixed(vec![Type::String, Type::Number, Type::Number], Type::String),
        ("string", "find") => fixed(
            vec![Type::String, Type::String],
            Type::optional(Type::Number),
        ),
        ("string", "format") => Sig {
            params: vec![Type::String],
            ret: Type::String,
            rule: ArgRule::FormatVariadic,
            arity: Arity::AtLeast(1),
        },
        _ => return None,
    })
}

/// Type of a namespace value field (`math.pi`, `math.huge`). Returns `None` if the namespace
/// has no such field.
pub fn namespace_field_type(ns: &str, field: &str) -> Option<Type> {
    match (ns, field) {
        ("math", "pi") | ("math", "huge") => Some(Type::Number),
        _ => None,
    }
}

/// Format a number the way `tostring` does: integral values without a trailing `.0`. Shared
/// with [`crate::value::Value`]'s `Display`. Kept feature-free since it only touches `f64`.
pub fn num_to_string(n: f64) -> String {
    if n.is_finite() && n == n.trunc() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

// ---- reference implementations (any backend) --------------------------------
//
// The evaluators below are the single source of truth for builtin behavior. Both backends
// call into them at runtime — the IR VM / tree-walking interpreter directly, and the JIT via
// its runtime shims (`codegen::rt`) — so they compile whenever a backend is enabled.

#[cfg(any(feature = "interp", feature = "jit"))]
pub use imp::{field_value, member_call, value_call};

#[cfg(any(feature = "interp", feature = "jit"))]
mod imp {
    use super::num_to_string;
    use crate::value::{RunError, Value};

    fn number(v: &Value) -> Result<f64, RunError> {
        v.as_f64()
            .ok_or_else(|| RunError::Internal("builtin expected a number".into()))
    }

    fn string_of(v: &Value) -> Result<String, RunError> {
        v.as_string()
            .ok_or_else(|| RunError::Internal("builtin expected a string".into()))
    }

    /// Evaluate a namespace value field: `math.pi`, `math.huge`.
    pub fn field_value(ns: &str, field: &str) -> Result<Value, RunError> {
        match (ns, field) {
            ("math", "pi") => Ok(Value::Number(std::f64::consts::PI)),
            ("math", "huge") => Ok(Value::Number(f64::INFINITY)),
            _ => Err(RunError::Internal(format!(
                "`{ns}.{field}` is not a builtin constant"
            ))),
        }
    }

    /// Evaluate a plain value builtin: `tostring`, `tonumber`.
    pub fn value_call(name: &str, args: &[Value]) -> Result<Value, RunError> {
        match name {
            "tostring" => {
                let v = args.first().unwrap_or(&Value::Nil);
                Ok(Value::string(match v {
                    Value::Str(s) => s.to_string(),
                    Value::Number(n) => num_to_string(*n),
                    Value::Bool(b) => b.to_string(),
                    other => other.to_string(),
                }))
            }
            "tonumber" => {
                let v = args.first().unwrap_or(&Value::Nil);
                match v {
                    Value::Str(s) => Ok(s
                        .trim()
                        .parse::<f64>()
                        .map(Value::Number)
                        .unwrap_or(Value::Nil)),
                    Value::Number(n) => Ok(Value::Number(*n)),
                    _ => Ok(Value::Nil),
                }
            }
            _ => Err(RunError::Internal(format!("unknown builtin `{name}`"))),
        }
    }

    /// Evaluate a namespace member builtin: `math.floor`, `string.sub`, etc.
    pub fn member_call(ns: &str, name: &str, args: &[Value]) -> Result<Value, RunError> {
        let num =
            |i: usize| -> Result<f64, RunError> { number(args.get(i).unwrap_or(&Value::Nil)) };
        let string = |i: usize| -> Result<String, RunError> {
            string_of(args.get(i).unwrap_or(&Value::Nil))
        };
        match (ns, name) {
            ("math", "floor") => Ok(Value::Number(num(0)?.floor())),
            ("math", "ceil") => Ok(Value::Number(num(0)?.ceil())),
            ("math", "abs") => Ok(Value::Number(num(0)?.abs())),
            ("math", "sqrt") => Ok(Value::Number(num(0)?.sqrt())),
            ("math", "min") => Ok(Value::Number(num(0)?.min(num(1)?))),
            ("math", "max") => Ok(Value::Number(num(0)?.max(num(1)?))),
            ("math", "pow") => Ok(Value::Number(num(0)?.powf(num(1)?))),
            ("string", "len") => Ok(Value::Number(string(0)?.len() as f64)),
            ("string", "upper") => Ok(Value::string(string(0)?.to_uppercase())),
            ("string", "lower") => Ok(Value::string(string(0)?.to_lowercase())),
            ("string", "sub") => {
                let s = string(0)?;
                let i = num(1)? as isize;
                let j = num(2)? as isize;
                Ok(Value::string(lua_sub(&s, i, j)))
            }
            ("string", "find") => {
                let s = string(0)?;
                let pat = string(1)?;
                match s.find(pat.as_str()) {
                    // 1-based start index of a plain (non-pattern) match.
                    Some(byte_idx) => Ok(Value::Number((byte_idx + 1) as f64)),
                    None => Ok(Value::Nil),
                }
            }
            ("string", "format") => Ok(Value::string(lua_format(&string(0)?, &args[1..])?)),
            _ => Err(RunError::Internal(format!(
                "unknown builtin member `{ns}.{name}`"
            ))),
        }
    }

    /// Lua-style `string.sub`: 1-based, inclusive, with negative indices counting from the
    /// end.
    fn lua_sub(s: &str, i: isize, j: isize) -> String {
        let bytes = s.as_bytes();
        let len = bytes.len() as isize;
        let norm = |x: isize| -> isize {
            if x < 0 {
                (len + x + 1).max(1)
            } else {
                x.max(1)
            }
        };
        let start = norm(i);
        let end = if j < 0 { len + j + 1 } else { j.min(len) };
        if start > end || start > len {
            return String::new();
        }
        let a = (start - 1) as usize;
        let b = end as usize;
        String::from_utf8_lossy(&bytes[a..b]).into_owned()
    }

    /// Minimal `string.format`: supports `%d`/`%i`, `%f`, `%s`, `%g`, and `%%`. Each verb
    /// consumes the next argument in order. Width/precision modifiers are not supported (v1).
    fn lua_format(fmt: &str, args: &[Value]) -> Result<String, RunError> {
        let mut out = String::new();
        let mut arg_i = 0;
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('d') | Some('i') => {
                    let n = number(args.get(arg_i).unwrap_or(&Value::Nil))?;
                    arg_i += 1;
                    out.push_str(&format!("{}", n as i64));
                }
                Some('f') => {
                    let n = number(args.get(arg_i).unwrap_or(&Value::Nil))?;
                    arg_i += 1;
                    out.push_str(&format!("{n:.6}"));
                }
                Some('g') => {
                    let n = number(args.get(arg_i).unwrap_or(&Value::Nil))?;
                    arg_i += 1;
                    out.push_str(&num_to_string(n));
                }
                Some('s') => {
                    let v = args.get(arg_i).unwrap_or(&Value::Nil);
                    arg_i += 1;
                    out.push_str(&v.to_string());
                }
                Some(other) => {
                    return Err(RunError::Runtime(format!(
                        "unsupported string.format verb `%{other}`"
                    )));
                }
                None => return Err(RunError::Runtime("trailing `%` in format string".into())),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_sigs_are_catalogued() {
        assert_eq!(value_sig("tostring").unwrap().ret, Type::String);
        assert_eq!(
            value_sig("tonumber").unwrap().ret,
            Type::optional(Type::Number)
        );
        assert!(value_sig("nope").is_none());
    }

    #[test]
    fn member_sigs_match_checker_table() {
        assert_eq!(member_sig("math", "floor").unwrap().ret, Type::Number);
        assert_eq!(
            member_sig("math", "min").unwrap().params,
            vec![Type::Number, Type::Number]
        );
        assert_eq!(member_sig("string", "upper").unwrap().ret, Type::String);
        assert_eq!(
            member_sig("string", "find").unwrap().ret,
            Type::optional(Type::Number)
        );
        let fmt = member_sig("string", "format").unwrap();
        assert_eq!(fmt.rule, ArgRule::FormatVariadic);
        assert_eq!(fmt.arity, Arity::AtLeast(1));
        assert!(member_sig("string", "nope").is_none());
        assert!(member_sig("nope", "floor").is_none());
    }

    #[test]
    fn namespace_fields_catalogued() {
        assert_eq!(namespace_field_type("math", "pi"), Some(Type::Number));
        assert_eq!(namespace_field_type("math", "huge"), Some(Type::Number));
        assert!(namespace_field_type("math", "tau").is_none());
    }

    #[test]
    fn num_to_string_drops_trailing_zero() {
        assert_eq!(num_to_string(42.0), "42");
        assert_eq!(num_to_string(3.5), "3.5");
    }
}
