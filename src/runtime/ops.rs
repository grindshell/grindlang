//! The **evaluation rules** every backend shares: arithmetic, ordering, the unary operators,
//! and the 1-based array index discipline.
//!
//! The AST interpreter and the IR VM evaluate every operator through here, and the JIT's shims
//! call the same array and table helpers, so a rule is written once. The JIT compiles arithmetic
//! and number comparisons to native instructions instead of calling [`arith`]/[`compare`]; those
//! emit the same formulas (`codegen::translate`), and the differential suites hold them equal.
//!
//! Operands are assumed to have passed type checking. A value of the wrong shape is therefore a
//! [`RunError::Internal`] invariant violation, not a user error. The failures a well-typed
//! program can still reach — ordering a NaN, an out-of-range array write — are
//! [`RunError::Runtime`].

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::ast::{BinOp, UnOp};
use crate::runtime::builtins::num_to_string;
use crate::value::{RunError, Value};

/// The number a type-checked operand holds.
pub fn number(v: &Value) -> Result<f64, RunError> {
    v.as_f64()
        .ok_or_else(|| RunError::Internal(format!("expected a number, found {}", v.type_name())))
}

/// The text a type-checked string operand holds.
pub fn string(v: &Value) -> Result<&str, RunError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(RunError::Internal(format!(
            "expected a string, found {}",
            other.type_name()
        ))),
    }
}

/// Numeric `+ - * / // % ^`. `%` is floored (the result takes the divisor's sign), as in Lua.
///
/// # Panics
/// On any other operator.
pub fn arith(op: BinOp, a: f64, b: f64) -> f64 {
    match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        BinOp::FloorDiv => (a / b).floor(),
        BinOp::Mod => a - (a / b).floor() * b,
        BinOp::Pow => a.powf(b),
        _ => unreachable!("`{}` is not arithmetic", op.symbol()),
    }
}

/// The error for ordering a NaN. The JIT detects that case natively and raises the same message.
pub const NAN_COMPARISON: &str = "comparison with NaN";

/// Order two numbers or two strings for `< <= > >=`. Ordering a NaN is an error rather than
/// IEEE's silent `false`, so a comparison never quietly takes the wrong branch.
pub fn compare(l: &Value, r: &Value) -> Result<Ordering, RunError> {
    match (l, r) {
        (Value::Number(a), Value::Number(b)) => a
            .partial_cmp(b)
            .ok_or_else(|| RunError::Runtime(NAN_COMPARISON.into())),
        (Value::Str(a), Value::Str(b)) => Ok(a.cmp(b)),
        _ => Err(RunError::Internal(format!(
            "cannot compare {} with {}",
            l.type_name(),
            r.type_name()
        ))),
    }
}

/// Evaluate a binary operator whose operands are both already evaluated: every operator except
/// `and`/`or`, which short-circuit and so are control flow in each backend.
pub fn binary(op: BinOp, l: &Value, r: &Value) -> Result<Value, RunError> {
    use BinOp::*;
    Ok(match op {
        Add | Sub | Mul | Div | FloorDiv | Mod | Pow => {
            Value::Number(arith(op, number(l)?, number(r)?))
        }
        Concat => Value::string([string(l)?, string(r)?].concat()),
        Lt | Le | Gt | Ge => {
            let ord = compare(l, r)?;
            Value::Bool(match op {
                Lt => ord == Ordering::Less,
                Le => ord != Ordering::Greater,
                Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            })
        }
        Eq => Value::Bool(l.ref_eq(r)),
        Ne => Value::Bool(!l.ref_eq(r)),
        And | Or => {
            return Err(RunError::Internal(format!(
                "`{}` must short-circuit, not evaluate both operands",
                op.symbol()
            )));
        }
    })
}

/// Evaluate a unary operator.
pub fn unary(op: UnOp, v: &Value) -> Result<Value, RunError> {
    Ok(match op {
        UnOp::Neg => Value::Number(-number(v)?),
        UnOp::Not => Value::Bool(!v.is_truthy()),
        UnOp::Len => Value::Number(len(v)?),
    })
}

/// `#v`: a string's byte length or an array's element count.
pub fn len(v: &Value) -> Result<f64, RunError> {
    match v {
        Value::Str(s) => Ok(s.len() as f64),
        Value::Array(a) => Ok(a.borrow().len() as f64),
        other => Err(RunError::Internal(format!(
            "`#` on a {} value",
            other.type_name()
        ))),
    }
}

/// `arr[idx]`: the element at 1-based `idx`, or `nil` when `idx` is out of range or not a
/// positive integer (`SPEC.md` §5.4 — an index read always yields `T?`).
pub fn array_get(arr: &RefCell<Vec<Value>>, idx: f64) -> Value {
    if idx < 1.0 || idx.fract() != 0.0 {
        return Value::Nil;
    }
    arr.borrow()
        .get(idx as usize - 1)
        .cloned()
        .unwrap_or(Value::Nil)
}

/// `arr[idx] = v`: overwrite an element, or append when `idx` is exactly one past the end (the
/// `a[#a + 1] = v` idiom). Any other index is an error, so an array never has holes.
pub fn array_set(arr: &RefCell<Vec<Value>>, idx: f64, v: Value) -> Result<(), RunError> {
    if idx < 1.0 || idx.fract() != 0.0 {
        return Err(RunError::Runtime(format!(
            "array index {} is not a positive integer",
            num_to_string(idx)
        )));
    }
    let i = idx as usize;
    let mut items = arr.borrow_mut();
    let len = items.len();
    if i <= len {
        items[i - 1] = v;
    } else if i == len + 1 {
        items.push(v);
    } else {
        return Err(RunError::Runtime(format!(
            "array index {i} is out of range (length {len}); arrays may only grow by one"
        )));
    }
    Ok(())
}

/// `t.key = v` / `t[key] = v` on a record or map. Allocates the key only for a new entry, so
/// overwriting an existing field (what an assignment in a loop does) allocates nothing.
pub fn table_set(table: &RefCell<BTreeMap<String, Value>>, key: &str, v: Value) {
    let mut table = table.borrow_mut();
    match table.get_mut(key) {
        Some(slot) => *slot = v,
        None => {
            table.insert(key.to_owned(), v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modulo_takes_the_divisor_sign() {
        assert_eq!(arith(BinOp::Mod, 7.0, 3.0), 1.0);
        assert_eq!(arith(BinOp::Mod, -7.0, 3.0), 2.0);
        assert_eq!(arith(BinOp::Mod, 7.0, -3.0), -2.0);
        assert_eq!(arith(BinOp::FloorDiv, -7.0, 2.0), -4.0);
    }

    #[test]
    fn ordering_a_nan_is_an_error() {
        let nan = Value::Number(f64::NAN);
        assert!(matches!(
            compare(&nan, &Value::Number(1.0)),
            Err(RunError::Runtime(_))
        ));
        // Equality is not an ordering: NaN is simply unequal to itself.
        assert_eq!(
            binary(BinOp::Eq, &nan, &nan).unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn array_index_discipline() {
        let arr = RefCell::new(vec![Value::Number(1.0)]);
        assert!(matches!(array_get(&arr, 0.0), Value::Nil));
        assert!(matches!(array_get(&arr, 1.5), Value::Nil));
        assert!(matches!(array_get(&arr, 2.0), Value::Nil));
        assert_eq!(array_get(&arr, 1.0).as_f64(), Some(1.0));

        array_set(&arr, 2.0, Value::Number(2.0)).unwrap();
        array_set(&arr, 1.0, Value::Number(9.0)).unwrap();
        assert!(array_set(&arr, 4.0, Value::Nil).is_err(), "a hole");
        assert!(array_set(&arr, 0.0, Value::Nil).is_err());
        assert_eq!(Value::Array(arr.into()).to_string(), "[9, 2]");
    }
}
