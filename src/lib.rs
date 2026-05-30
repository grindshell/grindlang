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
//! * Phase 0–1: diagnostics, lexer, AST, parser.
//! * Phase 2: resolver + constraint enforcement.
//! * **Phase 3 (current):** static type inference & checking.
//! * Phase 4: reference interpreter (semantics oracle).
//! * Phase 5–7: typed IR → cranelift JIT.
//! * Phase 8–9: host embedding API, hardening, docs.
//!
//! ## Front-end entry points
//!
//! [`parse`] runs the lexer and parser, returning the untyped [`ast::Module`]. [`check`]
//! additionally runs the resolver against a [`resolve::ResolveConfig`], returning the
//! module plus its [`resolve::Resolution`]. [`analyze`] runs the full front end —
//! parse, resolve, and type-check — against a [`types::TypeConfig`], returning the module,
//! its [`resolve::Resolution`], and its [`types::TypeInfo`] (the export signature). All
//! surface a batch of [`Diagnostics`] on failure.

pub mod ast;
pub mod diagnostics;
pub mod lexer;
pub mod parser;
pub mod resolve;
pub mod types;

pub use ast::Module;
pub use diagnostics::{Diagnostic, Diagnostics, Severity, Span};
pub use resolve::{Binding, Resolution, ResolveConfig};
pub use types::{FnType, Type, TypeConfig, TypeInfo};

/// Lex and parse Grindlang source into an untyped [`ast::Module`].
///
/// Returns every diagnostic collected if lexing or parsing failed.
///
/// ```
/// let module = grindlang::parse("function double(x) return x * 2 end").unwrap();
/// assert_eq!(module.decls.len(), 1);
/// ```
pub fn parse(src: &str) -> Result<Module, Diagnostics> {
    let tokens = lexer::lex(src)?;
    parser::parse(tokens)
}

/// Parse and resolve Grindlang source against host configuration `cfg`.
///
/// This runs the front end ([`parse`]) followed by name resolution and constraint
/// enforcement ([`resolve::resolve`]). Returns the [`ast::Module`] together with its
/// [`resolve::Resolution`], or every collected diagnostic on failure.
///
/// ```
/// use grindlang::ResolveConfig;
/// let (module, res) = grindlang::check(
///     "function double(x) return x * 2 end",
///     &ResolveConfig::default(),
/// )
/// .unwrap();
/// assert_eq!(module.decls.len(), 1);
/// assert!(res.symbols.iter().any(|s| s.name == "x"));
/// ```
pub fn check(src: &str, cfg: &ResolveConfig) -> Result<(Module, Resolution), Diagnostics> {
    let module = parse(src)?;
    let resolution = resolve::resolve(&module, cfg)?;
    Ok((module, resolution))
}

/// Parse, resolve, and type-check Grindlang source against host configuration `cfg`.
///
/// This is the full Phase 0–3 front end. Returns the [`ast::Module`], its
/// [`resolve::Resolution`], and its [`types::TypeInfo`] (the export signature), or every
/// collected diagnostic on failure. The [`types::TypeConfig`] is the single source of
/// truth for host names and their types — the resolver config is derived from it.
///
/// ```
/// use grindlang::TypeConfig;
/// let (_module, _res, info) =
///     grindlang::analyze("function double(x) return x * 2 end", &TypeConfig::default())
///         .unwrap();
/// assert_eq!(info.exports["double"].to_string(), "fn(number) -> number");
/// ```
pub fn analyze(src: &str, cfg: &TypeConfig) -> Result<(Module, Resolution, TypeInfo), Diagnostics> {
    let module = parse(src)?;
    let resolution = resolve::resolve(&module, &cfg.to_resolve_config())?;
    let info = types::typecheck(&module, &resolution, cfg)?;
    Ok((module, resolution, info))
}
