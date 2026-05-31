//! The **host ABI** (`PLAN.md` Phase 6): how compiled scripts call into Rust.
//!
//! Two boundaries cross from Grindlang into the host:
//!
//!   * **Registered host functions** — Rust closures the embedder exposes by name (e.g.
//!     `roll(n)`). Each has a statically known signature; this module derives its calling
//!     convention ([`FnAbi`]) as a list of parameter [`Repr`]s and a return [`Repr`]. The
//!     (Phase 7) JIT builds a cranelift signature from that and emits a trampoline that
//!     marshals [`Slot`](super::repr::Slot)s to/from the closure.
//!   * **Host memory** — a Lua-userdata-like binding (e.g. `mem`) backed by Rust-owned
//!     state that persists across invocations. A memory binding is a [`Type::Record`]; this
//!     module turns it into a [`MemorySchema`] of typed field accessors, each of which the
//!     backend compiles to a direct load/store (or accessor call) into host state.
//!
//! Keeping the ABI described in terms of [`Repr`] (not cranelift types) means the rest of
//! the crate carries no backend dependency until Phase 7.

use crate::types::{FnType, Type};

use super::repr::Repr;

/// An error raised by a host-registered function across the ABI boundary. The JIT translates
/// a returned error into the engine's `Err` channel (the interpreters surface it as
/// [`crate::interp::RunError::Host`]).
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);

/// The ABI calling convention of a function: the [`Repr`] of each parameter and of the
/// return value. Used for both registered host functions and top-level script functions
/// (their boundaries are identical).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnAbi {
    pub params: Vec<Repr>,
    pub ret: Repr,
}

impl FnAbi {
    /// Derive the calling convention of `ft`.
    pub fn of(ft: &FnType) -> FnAbi {
        FnAbi {
            params: ft.params.iter().map(Repr::of).collect(),
            ret: Repr::of(&ft.ret),
        }
    }

    /// A human-readable cranelift-style signature, e.g. `(f64, i8) -> i64`. Handy for ABI
    /// documentation and tests; Phase 7 builds the real `cranelift::Signature`.
    pub fn cranelift_signature(&self) -> String {
        let params: Vec<&str> = self.params.iter().map(|r| r.cranelift_type()).collect();
        let ret = match self.ret {
            Repr::Unit => String::new(),
            r => format!(" -> {}", r.cranelift_type()),
        };
        format!("({}){ret}", params.join(", "))
    }
}

/// One field of a host memory binding: its name, static type, and [`Repr`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryField {
    pub name: String,
    pub ty: Type,
    pub repr: Repr,
}

/// The accessor schema of a host memory binding. Each field becomes a typed get/set the
/// backend compiles to a direct access into host-owned state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySchema {
    pub fields: Vec<MemoryField>,
}

impl MemorySchema {
    /// Derive the schema of a memory binding from its type. Returns `None` unless the binding
    /// is a [`Type::Record`] (the only memory shape with statically known fields; a
    /// `map`-typed binding is accessed dynamically, not via fixed accessors).
    pub fn of(ty: &Type) -> Option<MemorySchema> {
        match ty {
            Type::Record(fields) => Some(MemorySchema {
                fields: fields
                    .iter()
                    .map(|(name, ty)| MemoryField {
                        name: name.clone(),
                        ty: ty.clone(),
                        repr: Repr::of(ty),
                    })
                    .collect(),
            }),
            _ => None,
        }
    }

    /// Look up a field's accessor by name.
    pub fn field(&self, name: &str) -> Option<&MemoryField> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn fn_abi_maps_params_and_ret() {
        let ft = FnType {
            params: vec![Type::Number, Type::Bool],
            ret: Box::new(Type::String),
        };
        let abi = FnAbi::of(&ft);
        assert_eq!(abi.params, vec![Repr::Number, Repr::Bool]);
        assert_eq!(abi.ret, Repr::Ptr);
        assert_eq!(abi.cranelift_signature(), "(f64, i8) -> i64");
    }

    #[test]
    fn fn_abi_unit_return_has_no_arrow() {
        let ft = FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Unit),
        };
        assert_eq!(FnAbi::of(&ft).cranelift_signature(), "(f64)");
    }

    #[test]
    fn memory_schema_from_record() {
        let mut fields = BTreeMap::new();
        fields.insert("gold".to_string(), Type::Number);
        fields.insert("name".to_string(), Type::String);
        let schema = MemorySchema::of(&Type::Record(fields)).unwrap();
        assert_eq!(schema.fields.len(), 2);
        assert_eq!(schema.field("gold").unwrap().repr, Repr::Number);
        assert_eq!(schema.field("name").unwrap().repr, Repr::Ptr);
        assert!(schema.field("missing").is_none());
    }

    #[test]
    fn non_record_memory_has_no_static_schema() {
        assert!(MemorySchema::of(&Type::map(Type::Number)).is_none());
    }
}
