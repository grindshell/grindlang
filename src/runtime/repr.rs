//! The machine-level **value representation** the JIT lowers against.
//!
//! Grindlang is statically typed, so a compiled value carries **no runtime tag**: how a machine
//! word is interpreted is fixed by the value's static [`Type`]. [`Repr`] is the small set of
//! representation classes those types collapse to. The cranelift backend maps each one to a
//! concrete cranelift type (`codegen::translate::clif_ty`) and to the tag its runtime shims use
//! to decode an operand (`codegen::rt::repr_tag`).

use crate::types::Type;

/// The machine-level representation class of a value.
///
/// Every concrete [`Type`] collapses to exactly one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Repr {
    /// `f64` — the `number` type.
    Number,
    /// `i8` carrying `0`/`1` — the `bool` type.
    Bool,
    /// An `i64` handle into the per-call value table — `string`, `array`, `map`, `record`,
    /// `userdata`, functions, and any optional type. `nil` is the null handle.
    Ptr,
    /// No value — `()`/[`Type::Unit`], the result of a `return`-less function. Lowered as a
    /// function with no return value.
    Unit,
}

impl Repr {
    /// The representation class of `ty`.
    ///
    /// An `Optional` of a scalar (`number?`, `bool?`) is a nullable [`Repr::Ptr`] handle (the
    /// scalar boxed, `nil` = null) rather than a niche. An unboxed-optional representation is
    /// deferred; keeping all optionals pointer-shaped keeps codegen uniform.
    pub fn of(ty: &Type) -> Repr {
        match ty {
            Type::Number => Repr::Number,
            Type::Bool => Repr::Bool,
            Type::Unit => Repr::Unit,
            Type::String
            | Type::Array(_)
            | Type::Map(_)
            | Type::Record(_)
            | Type::Tuple(_)
            | Type::Userdata(_)
            | Type::Function(_)
            | Type::Optional(_)
            | Type::Nil => Repr::Ptr,
            // Unresolved inference vars / poison: be conservative and treat as a pointer.
            // Well-typed programs never reach codegen with these.
            Type::Var(_) | Type::Error => Repr::Ptr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_of_scalar_types() {
        assert_eq!(Repr::of(&Type::Number), Repr::Number);
        assert_eq!(Repr::of(&Type::Bool), Repr::Bool);
        assert_eq!(Repr::of(&Type::Unit), Repr::Unit);
        assert_eq!(Repr::of(&Type::String), Repr::Ptr);
    }

    #[test]
    fn repr_of_reference_and_optional_types() {
        assert_eq!(Repr::of(&Type::array(Type::Number)), Repr::Ptr);
        assert_eq!(Repr::of(&Type::map(Type::Bool)), Repr::Ptr);
        assert_eq!(Repr::of(&Type::optional(Type::Number)), Repr::Ptr);
        assert_eq!(Repr::of(&Type::Nil), Repr::Ptr);
        assert_eq!(Repr::of(&Type::Userdata("Mem".into())), Repr::Ptr);
    }
}
