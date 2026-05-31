//! The flat **value representation** at the ABI boundary (`PLAN.md` Phase 6).
//!
//! Grindlang is statically typed, so a value carries **no runtime tag**: how a machine word
//! is interpreted is fixed by the value's static [`Type`]. [`Repr`] is the small set of
//! machine-level representation classes those types collapse to, and [`Slot`] is the single
//! 64-bit word that carries one value across the boundary.
//!
//! The (Phase 7) cranelift backend maps each [`Repr`] to a concrete cranelift type and
//! passes [`Slot`]s in registers. This module commits to the *shape* of that contract
//! without yet committing to raw pointers — a reference value's [`Slot`] holds an opaque
//! handle (Phase 7 turns it into an arena address).

use crate::types::Type;

/// The machine-level representation class of a value at the ABI boundary.
///
/// Every concrete [`Type`] collapses to exactly one of these. The mapping is the contract
/// the (Phase 7) JIT lowers against; see [`Repr::cranelift_type`] for the intended cranelift
/// types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Repr {
    /// `f64` — the `number` type. cranelift `F64`.
    Number,
    /// `i8` carrying `0`/`1` — the `bool` type. cranelift `I8`.
    Bool,
    /// A pointer-sized handle into the arena — `string`, `array`, `map`, `record`,
    /// `userdata`, functions, and any optional type. `nil` is the null handle. cranelift
    /// `I64`.
    Ptr,
    /// No value — `()`/[`Type::Unit`], the result of a `return`-less function. Lowered as a
    /// function with no return value (not an actual slot).
    Unit,
}

impl Repr {
    /// The representation class of `ty`.
    ///
    /// Note (v1 ABI): an `Optional` of a scalar (`number?`, `bool?`) is represented as a
    /// nullable [`Repr::Ptr`] handle (the scalar is boxed in the arena, `nil` = null) rather
    /// than via a niche. A niche/unboxed-optional optimization is deferred to a later pass;
    /// keeping all optionals pointer-shaped keeps Phase 7 codegen uniform.
    pub fn of(ty: &Type) -> Repr {
        match ty {
            Type::Number => Repr::Number,
            Type::Bool => Repr::Bool,
            Type::Unit => Repr::Unit,
            Type::String
            | Type::Array(_)
            | Type::Map(_)
            | Type::Record(_)
            | Type::Userdata(_)
            | Type::Function(_)
            | Type::Optional(_)
            | Type::Nil => Repr::Ptr,
            // Unresolved inference vars / poison: be conservative and treat as a pointer.
            // Well-typed programs never reach codegen with these.
            Type::Var(_) | Type::Error => Repr::Ptr,
        }
    }

    /// The cranelift type name this representation lowers to in Phase 7. Returned as a
    /// string so this crate carries no cranelift dependency until the backend lands.
    pub fn cranelift_type(self) -> &'static str {
        match self {
            Repr::Number => "f64",
            Repr::Bool => "i8",
            Repr::Ptr => "i64",
            Repr::Unit => "(none)",
        }
    }

    /// Size in bytes of this representation when stored in a [`Slot`] / arena cell.
    pub fn size(self) -> usize {
        match self {
            Repr::Number | Repr::Ptr => 8,
            Repr::Bool => 1,
            Repr::Unit => 0,
        }
    }
}

/// One ABI machine word: a 64-bit cell whose meaning is fixed by the value's static
/// [`Repr`]. The (Phase 7) JIT passes these in registers; the runtime and tests use it to
/// talk about values abstractly without committing to raw pointers yet.
///
/// There is no tag — reading a [`Slot`] with the wrong accessor is a *logic* error the type
/// system is responsible for preventing, exactly as in the compiled code.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(u64);

impl Slot {
    /// The null handle — the representation of `nil` for a [`Repr::Ptr`] value.
    pub const NULL: Slot = Slot(0);

    /// Wrap an `f64` number (stored as its IEEE-754 bit pattern).
    pub fn from_number(n: f64) -> Slot {
        Slot(n.to_bits())
    }
    /// Read this slot as an `f64` number.
    pub fn as_number(self) -> f64 {
        f64::from_bits(self.0)
    }

    /// Wrap a `bool` (`0`/`1`).
    pub fn from_bool(b: bool) -> Slot {
        Slot(b as u64)
    }
    /// Read this slot as a `bool` (non-zero = true).
    pub fn as_bool(self) -> bool {
        self.0 != 0
    }

    /// Wrap an opaque arena handle. Phase 7 replaces this with a real address.
    pub fn from_handle(h: u64) -> Slot {
        Slot(h)
    }
    /// Read the opaque handle.
    pub fn handle(self) -> u64 {
        self.0
    }
    /// Whether this is the null handle (`nil`).
    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw bits (for the JIT / serialization).
    pub fn bits(self) -> u64 {
        self.0
    }
    /// Reconstruct a slot from raw bits.
    pub fn from_bits(b: u64) -> Slot {
        Slot(b)
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slot(0x{:016x})", self.0)
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

    #[test]
    fn repr_cranelift_types_and_sizes() {
        assert_eq!(Repr::Number.cranelift_type(), "f64");
        assert_eq!(Repr::Bool.cranelift_type(), "i8");
        assert_eq!(Repr::Ptr.cranelift_type(), "i64");
        assert_eq!(Repr::Number.size(), 8);
        assert_eq!(Repr::Bool.size(), 1);
        assert_eq!(Repr::Unit.size(), 0);
    }

    #[test]
    fn slot_number_round_trips() {
        for n in [0.0, -1.5, 123.456_f64, f64::INFINITY, 1e15] {
            assert_eq!(Slot::from_number(n).as_number(), n);
        }
        // NaN round-trips bitwise.
        assert!(Slot::from_number(f64::NAN).as_number().is_nan());
    }

    #[test]
    fn slot_bool_and_handle_round_trip() {
        assert!(Slot::from_bool(true).as_bool());
        assert!(!Slot::from_bool(false).as_bool());
        assert_eq!(Slot::from_handle(42).handle(), 42);
        assert!(Slot::NULL.is_null());
        assert!(!Slot::from_handle(1).is_null());
    }
}
