//! # Shared runtime
//!
//! The pieces every execution backend agrees on, so each is defined once:
//!
//! * **Value representation** ([`repr`]). How a static [`Type`](crate::types::Type) collapses
//!   to a machine [`Repr`] — `number` → `f64`, `bool` → `i8`, every reference / optional → an
//!   `i64` handle, `()` → no value. The cranelift backend lowers against it.
//! * **Builtins** ([`builtins`]). The single source of truth for the curated
//!   `math.*`/`string.*`/`tostring`/`tonumber` set: signatures (consumed by the checker and IR
//!   lowering) plus the reference implementations the interpreters and the JIT's runtime shims
//!   all call.
//!
//! The runtime values themselves ([`crate::value::Value`]) live in their own always-compiled
//! module. The JIT stores them in a per-call value table (`codegen::rt::RtCtx`) dropped when the
//! call returns; a native arena with fixed heap layouts is deferred (`PLAN.md`).

pub mod builtins;
pub mod repr;

pub use repr::Repr;
