//! The Ruby surface (`SPEC-RUBY.md`, `PLAN.md` Phase 10): a second syntax for the same language,
//! a strict subset of Ruby's, aimed at game logic and dialog DSLs. Front end only, behind the
//! `ruby` feature.
//!
//! [`parse`] turns source into the Ruby-shaped [`ast::Program`], rejecting with a targeted
//! diagnostic every Ruby construct Grindlang leaves out that shows in the syntax (`E05xx`).
//! [`translate`] turns that into the shared [`crate::ast::Module`], which the resolver, checker,
//! lowering, and all three backends then run as they would a Lua module; [`parse_module`] does
//! both. Translation rejects what needs more than syntax to rule out (`E052x`, `E06xx`).
//!
//! A host compiles a script with `Engine::compile_ruby` (with the `jit` feature), and
//! [`crate::unparse::to_lua`] prints what a script translated into.

pub mod ast;
mod collect;
mod desugar;
mod dsl;
pub mod lexer;
mod methods;
mod parser;
mod scope;

pub use dsl::{HandlerKeyword, Schema};

use crate::diagnostics::Diagnostics;
use crate::types::TypeConfig;

/// How a Ruby-surface script meets its host, beyond the [`TypeConfig`] every script has.
#[derive(Clone, Debug, Default)]
pub struct RubyConfig {
    /// The host memory binding `@name` is a field of (`SPEC-RUBY.md` §4.10). Without one,
    /// `@name` is an error.
    pub ivar_memory: Option<String>,
    /// The DSL keywords (§5), declared with [`handler`](Self::handler) and
    /// [`data`](Self::data).
    keywords: Vec<dsl::Keyword>,
}

impl RubyConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make `@name` the field `name` of the memory binding `memory`.
    pub fn ivar_memory(mut self, memory: impl Into<String>) -> Self {
        self.ivar_memory = Some(memory.into());
        self
    }
}

/// Lex and parse Ruby-surface source into an [`ast::Program`].
///
/// Returns every diagnostic collected if lexing failed, or the first parse error.
///
/// ```
/// use grindlang::ruby::ast::Item;
/// let program = grindlang::ruby::parse("def double(x)\n  x * 2\nend\n").unwrap();
/// assert!(matches!(&program.items[0], Item::Def(d) if d.name.node == "double"));
/// ```
pub fn parse(src: &str) -> Result<ast::Program, Diagnostics> {
    let (tokens, docs) = lexer::lex(src)?;
    parser::parse(src, tokens, docs)
}

/// Translate a parsed program into the shared tree, for a host whose memory bindings and
/// memory methods `cfg` declares (a call on a memory binding is a memory method call only if
/// the host declares the method).
///
/// Returns every diagnostic collected on failure.
pub fn translate(
    program: &ast::Program,
    ruby: &RubyConfig,
    cfg: &TypeConfig,
) -> Result<crate::ast::Module, Diagnostics> {
    desugar::translate(program, ruby, cfg)
}

/// Parse and translate Ruby-surface source into the shared tree, ready for
/// [`crate::analyze_module`] or [`crate::compile_module`] with the same `cfg`.
///
/// ```
/// use grindlang::ruby::{RubyConfig, parse_module};
/// use grindlang::TypeConfig;
/// let cfg = TypeConfig::default();
/// let module = parse_module("def double(x)\n  x * 2\nend\n", &RubyConfig::new(), &cfg).unwrap();
/// let (_, info) = grindlang::analyze_module(&module, &cfg).unwrap();
/// assert_eq!(info.exports["double"].to_string(), "fn(number) -> number");
/// ```
pub fn parse_module(
    src: &str,
    ruby: &RubyConfig,
    cfg: &TypeConfig,
) -> Result<crate::ast::Module, Diagnostics> {
    translate(&parse(src)?, ruby, cfg)
}
