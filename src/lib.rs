//! # Grindlang
//!
//! A small, statically-typed, cranelift-JIT-compiled language that reuses Lua's surface
//! syntax but is a constrained subset (Starlark-style) built for one job: embedding
//! **calculations** and **dialog-tree decisions** into Grindshell.
//!
//! Grindlang scripts are **not** standalone programs. Each script evaluates to a module
//! table of exported functions and constants; the host compiles it once and calls its
//! exports many times. State persists between calls only through host-provided memory.
//!
//! See `SPEC.md` for the language definition and `PLAN.md` for the implementation
//! roadmap. This crate is being built in phases:
//!
//! * **Phase 0–1 (current):** diagnostics, lexer, AST, parser.
//! * Phase 2: resolver + constraint enforcement.
//! * Phase 3: static type inference & checking.
//! * Phase 4: reference interpreter (semantics oracle).
//! * Phase 5–7: typed IR → cranelift JIT.
//! * Phase 8–9: host embedding API, hardening, docs.
//!
//! ## Front-end entry point
//!
//! [`parse`] runs the lexer and parser, returning the untyped [`ast::Module`] or a batch
//! of [`Diagnostics`].

pub mod ast;
pub mod diagnostics;
pub mod lexer;
pub mod parser;

pub use ast::Module;
pub use diagnostics::{Diagnostic, Diagnostics, Severity, Span};

/// Lex and parse Grindlang source into an untyped [`ast::Module`].
///
/// Returns every diagnostic collected if lexing or parsing failed. This is the Phase 0–1
/// front-end entry point; resolution and type checking are layered on in later phases.
///
/// ```
/// let module = grindlang::parse("function double(x) return x * 2 end").unwrap();
/// assert_eq!(module.decls.len(), 1);
/// ```
pub fn parse(src: &str) -> Result<Module, Diagnostics> {
    let tokens = lexer::lex(src)?;
    parser::parse(tokens)
}
