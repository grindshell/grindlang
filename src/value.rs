//! The runtime [`Value`] and [`RunError`] — the values that flow through a running Grindlang
//! program and across the host boundary.
//!
//! This module is **always compiled**, independent of any feature. `Value` is the shared
//! currency of the whole runtime: the cranelift JIT ([`crate::codegen`]) threads it through
//! its per-call context, the embedding API ([`crate::api`]) marshals Rust types to and from
//! it, and the tree-walking interpreter ([`crate::interp`]) evaluates directly over it. It
//! used to live in `interp`, which made it (and its `serde` impls) require the `interp`
//! feature even though the JIT — the primary backend — needs it just the same.
//!
//! ## Data values vs. callables
//!
//! The variants split in two:
//!
//! * **Data values** — `nil`, `bool`, `number`, `string`, `array`, `table` — are the values
//!   that cross the host boundary (host functions, memory, call arguments/results, `serde`).
//!   They are always available.
//! * **Callable values** — [`Value::Function`] (a script closure) and [`Value::Native`] (a
//!   host function captured *as a value*) — exist only with the `interp` feature. Only the
//!   tree-walking interpreter represents functions as first-class values; the JIT keeps host
//!   functions out-of-band in its runtime context and never materializes script closures, so
//!   compiled code never produces these variants.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use crate::runtime::builtins::num_to_string;

/// An error raised while executing a Grindlang program (by either the interpreter or the
/// JIT runtime).
#[derive(Clone, Debug, thiserror::Error)]
pub enum RunError {
    /// A host-registered function returned an error.
    #[error("host error: {0}")]
    Host(String),
    /// A runtime fault that should have been impossible after type checking (e.g. an
    /// out-of-range array write). Indicates either a script edge case or a checker gap.
    #[error("runtime error: {0}")]
    Runtime(String),
    /// An invariant the resolver/checker should have guaranteed was violated.
    #[error("internal interpreter error: {0}")]
    Internal(String),
    /// A function was requested by a name the module does not export.
    #[error("no exported function named `{0}`")]
    UnknownExport(String),
}

/// A host-registered native function: takes the evaluated arguments and returns a value.
/// Only meaningful with the `interp` feature, where a host function can be captured as a
/// [`Value::Native`]. (The JIT calls host functions through its runtime context instead.)
#[cfg(feature = "interp")]
pub type NativeFn = Rc<dyn Fn(&[Value]) -> Result<Value, RunError>>;

/// A runtime value.
///
/// The callable variants ([`Value::Function`], [`Value::Native`]) require the `interp`
/// feature; see the [module docs](self).
#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Number(f64),
    Str(Rc<str>),
    /// A 1-based, mutable, homogeneous array.
    Array(Rc<RefCell<Vec<Value>>>),
    /// A mutable string-keyed table — the runtime representation of records, maps, and
    /// host memory alike.
    Table(Rc<RefCell<BTreeMap<String, Value>>>),
    /// A script function or closure (interpreter only).
    #[cfg(feature = "interp")]
    Function(Rc<crate::interp::Func>),
    /// A host-registered native function captured as a value (interpreter only).
    #[cfg(feature = "interp")]
    Native(NativeFn),
}

impl Value {
    pub fn string(s: impl Into<String>) -> Value {
        Value::Str(Rc::from(s.into().as_str()))
    }

    pub fn array(items: Vec<Value>) -> Value {
        Value::Array(Rc::new(RefCell::new(items)))
    }

    pub fn table(entries: BTreeMap<String, Value>) -> Value {
        Value::Table(Rc::new(RefCell::new(entries)))
    }

    /// An empty mutable table — convenient for building host memory.
    pub fn empty_table() -> Value {
        Value::table(BTreeMap::new())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The string contents, if this is a string value.
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        }
    }

    /// A clone of the elements, if this is an array.
    pub fn as_array(&self) -> Option<Vec<Value>> {
        match self {
            Value::Array(a) => Some(a.borrow().clone()),
            _ => None,
        }
    }

    /// Read a field/key, if this is a table. Returns `None` for a missing key.
    pub fn field(&self, key: &str) -> Option<Value> {
        match self {
            Value::Table(t) => t.borrow().get(key).cloned(),
            _ => None,
        }
    }

    /// A short name for this value's runtime shape, e.g. `"number"`, `"array"`. Useful for
    /// host-facing error messages (the embedding API uses it for marshaling diagnostics).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::Str(_) => "string",
            Value::Array(_) => "array",
            Value::Table(_) => "table",
            #[cfg(feature = "interp")]
            Value::Function(_) | Value::Native(_) => "function",
        }
    }
}

#[cfg(feature = "interp")]
impl Value {
    /// Lua-style truthiness: only `nil` and `false` are falsy.
    pub(crate) fn is_truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => f.write_str("nil"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => f.write_str(&num_to_string(*n)),
            Value::Str(s) => f.write_str(s),
            Value::Array(a) => {
                f.write_str("[")?;
                for (i, v) in a.borrow().iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{v}")?;
                }
                f.write_str("]")
            }
            Value::Table(t) => {
                f.write_str("{")?;
                for (i, (k, v)) in t.borrow().iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                f.write_str("}")
            }
            #[cfg(feature = "interp")]
            Value::Function(_) | Value::Native(_) => f.write_str("function"),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.type_name(), self)
    }
}

// ---- serde marshaling (feature `serde`) -------------------------------------
//
// Lets hosts marshal Grindlang values (notably host memory) across process boundaries —
// e.g. as JSON. Scalars/arrays/tables map to the obvious serde forms; `nil` is unit.
// Function values cannot cross the boundary and serialize to an error.

#[cfg(feature = "serde")]
impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Value::Nil => s.serialize_unit(),
            Value::Bool(b) => s.serialize_bool(*b),
            Value::Number(n) => s.serialize_f64(*n),
            Value::Str(st) => s.serialize_str(st),
            Value::Array(a) => {
                let a = a.borrow();
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a.iter() {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Value::Table(t) => {
                let t = t.borrow();
                let mut map = s.serialize_map(Some(t.len()))?;
                for (k, v) in t.iter() {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
            #[cfg(feature = "interp")]
            Value::Function(_) | Value::Native(_) => Err(serde::ser::Error::custom(
                "cannot serialize a function value",
            )),
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Value;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a Grindlang value (nil, bool, number, string, array, or table)")
            }
            fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
                Ok(Value::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
                Ok(Value::Number(n as f64))
            }
            fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
                Ok(Value::Number(n as f64))
            }
            fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
                Ok(Value::Number(n))
            }
            fn visit_str<E>(self, s: &str) -> Result<Value, E> {
                Ok(Value::string(s))
            }
            fn visit_string<E>(self, s: String) -> Result<Value, E> {
                Ok(Value::string(s))
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Nil)
            }
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Nil)
            }
            fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
                <Value as serde::Deserialize>::deserialize(d)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(v) = seq.next_element()? {
                    items.push(v);
                }
                Ok(Value::array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Value, A::Error> {
                let mut entries = BTreeMap::new();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    entries.insert(k, v);
                }
                Ok(Value::table(entries))
            }
        }
        d.deserialize_any(V)
    }
}
