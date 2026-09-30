//! The Ruby surface (`SPEC-RUBY.md`, `PLAN.md` Phase 10): a second syntax for the same language,
//! a strict subset of Ruby's, aimed at game logic and dialog DSLs. Front end only, behind the
//! `ruby` feature.
//!
//! [`parse`] turns source into the Ruby-shaped [`ast::Program`], rejecting with a targeted
//! diagnostic every Ruby construct Grindlang leaves out that shows in the syntax (`E05xx`).
//! Translating a program into the shared [`crate::ast::Module`], which the resolver, checker,
//! lowering, and all three backends then run, is the next step of the plan (R2).

pub mod ast;
pub mod lexer;
mod parser;

use crate::diagnostics::Diagnostics;

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
