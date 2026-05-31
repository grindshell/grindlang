//! # Runtime & host ABI (`PLAN.md` Phase 6)
//!
//! This module defines the **runtime representation and calling convention** that the
//! (Phase 7) cranelift backend will target. It is deliberately *ABI-first*: it pins the
//! contract — value representation, heap layouts, the host calling convention, and the
//! builtin catalog — while keeping raw-pointer / `unsafe` work for Phase 7, where cranelift
//! dictates the concrete addresses. The crate carries **no cranelift dependency** here.
//!
//! ## The contract
//!
//! * **Values** ([`repr`]). A value is one machine word ([`Slot`]) whose interpretation is
//!   fixed by its static [`Type`](crate::types::Type) — there is no runtime tag. Types
//!   collapse to a [`Repr`]: `number` → `f64`, `bool` → `i8`, every reference / optional →
//!   an `i64` arena handle, `()` → no value.
//! * **Allocation** ([`arena`]). Values created during one invocation live in a bump
//!   [`Arena`] that is reset wholesale when the call returns — no GC. Persistent state goes
//!   through host memory instead.
//! * **Heap layouts** ([`layout`]). Exact `#[repr(C)]` layouts for `string`, `array`, `map`,
//!   and `record` so native code can load/store fields at constant offsets.
//! * **Host ABI** ([`host`]). [`FnAbi`] is the calling convention of a registered host (or
//!   script) function; [`MemorySchema`] is the typed-accessor ABI for a host memory binding.
//! * **Builtins** ([`builtins`]). The single source of truth for the curated
//!   `math.*`/`string.*`/`tostring`/`tonumber` set: signatures (consumed by the checker and
//!   IR) plus, behind the `interp` feature, the reference implementations both interpreters
//!   run.
//!
//! ## Validation (the Phase 6 exit criterion)
//!
//! "Runtime callable from the interpreter; ABI documented and tested." The interpreters now
//! execute every builtin through [`builtins`], so the runtime is exercised end to end by the
//! existing interpreter and differential suites; the representation, arena, layout, and ABI
//! pieces are unit-tested in their respective submodules.

pub mod arena;
pub mod builtins;
pub mod host;
pub mod layout;
pub mod repr;

pub use arena::{Arena, ArenaRef};
pub use host::{FnAbi, HostError, MemoryField, MemorySchema};
pub use repr::{Repr, Slot};
