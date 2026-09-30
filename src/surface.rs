//! The surface syntax a module was written in, and how diagnostics quote code in it.
//!
//! Everything after parsing — the resolver, the checker, lowering, and the backends — works on
//! the shared [`ast::Module`](crate::ast::Module), whichever syntax it was parsed from. The
//! resolver and the checker still quote code in their diagnostics (an operator, the test that
//! narrows an optional, a loop), so they ask the module's [`Surface`] how to write it instead of
//! spelling it in Lua inline. There are two surfaces: Lua's (`SPEC.md`) and Ruby's
//! (`SPEC-RUBY.md`, whose front end is behind the `ruby` feature; its wording here is not).

use crate::ast::{BinOp, UnOp};

/// The syntax a module was parsed from. It changes how diagnostics spell code, never what a
/// program means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Surface {
    /// Lua's syntax (`SPEC.md`).
    #[default]
    Lua,
    /// A subset of Ruby's syntax (`SPEC-RUBY.md`), translated into the shared tree.
    Ruby,
}

impl Surface {
    /// How binary operator `op` is written.
    pub fn binop(self, op: BinOp) -> &'static str {
        match self {
            Surface::Lua => op.symbol(),
            Surface::Ruby => match op {
                BinOp::FloorDiv => ".div",
                BinOp::Pow => "**",
                BinOp::Concat => "#{…}",
                BinOp::Ne => "!=",
                BinOp::And => "&&",
                BinOp::Or => "||",
                _ => op.symbol(),
            },
        }
    }

    /// How unary operator `op` is written.
    pub fn unop(self, op: UnOp) -> &'static str {
        match self {
            Surface::Lua => op.symbol(),
            Surface::Ruby => match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
                UnOp::Len => ".length",
            },
        }
    }

    /// The test that narrows an optional to its value (`SPEC.md` §5.3), written after it.
    pub fn nil_test(self) -> &'static str {
        match self {
            Surface::Lua => "~= nil",
            Surface::Ruby => ".nil?",
        }
    }

    /// The loop over an array, as a message names it, quoted.
    pub fn array_loop(self) -> &'static str {
        match self {
            Surface::Lua => "`ipairs`",
            Surface::Ruby => "`each` with one block parameter (or `map`, `select`, `sum`, …)",
        }
    }

    /// The loop over a map, as a message names it, quoted.
    pub fn map_loop(self) -> &'static str {
        match self {
            Surface::Lua => "`pairs`",
            Surface::Ruby => "`each` with two block parameters",
        }
    }

    /// The binding of several values at once that takes a multi-value call (`SPEC.md` §5.5).
    pub fn parallel_binding(self) -> &'static str {
        match self {
            Surface::Lua => "a parallel `local`/assignment",
            Surface::Ruby => "a multiple assignment (`a, b = …`)",
        }
    }

    /// The expression that picks one of two values by a condition, which is what an `and`/`or`
    /// whose operands differ in type was likely meant to be (`SPEC.md` §3.5).
    pub fn choice(self) -> &'static str {
        match self {
            Surface::Lua => "cond and a or b",
            Surface::Ruby => "cond ? a : b",
        }
    }

    /// The statement that goes on to a loop's next pass (`SPEC.md` §3.3), quoted.
    pub fn continue_stat(self) -> &'static str {
        match self {
            Surface::Lua => "`continue`",
            Surface::Ruby => "`next`",
        }
    }

    /// A closure calling the named function `name`, which makes it a value (`SPEC.md` §5.5).
    pub fn closure_calling(self, name: &str) -> String {
        match self {
            Surface::Lua => format!("function(x) return {name}(x) end"),
            Surface::Ruby => format!("->(x) {{ {name}(x) }}"),
        }
    }

    /// Why top-level code can't `return` (`E0309`), past the rule itself.
    pub fn top_level_return(self) -> &'static str {
        match self {
            Surface::Lua => "the only top-level `return` is a module's trailing export table",
            Surface::Ruby => "a script exports its public `def`s and its constants",
        }
    }

    /// What starts a doc-comment line holding an annotation, as a message quotes it.
    pub fn doc_comment(self) -> &'static str {
        match self {
            Surface::Lua => "---",
            Surface::Ruby => "# ",
        }
    }

    /// Advice for an optional used as a condition, where the surface has some: in Ruby, `if x`
    /// tests whether `x` is `nil`, which Grindlang spells out.
    pub fn optional_condition_hint(self) -> Option<&'static str> {
        match self {
            Surface::Lua => None,
            Surface::Ruby => {
                Some("to test whether it is nil, write `unless x.nil?` or `if x.nil?`")
            }
        }
    }

    /// Advice for `+` with a string operand, where the surface has some: Ruby authors join
    /// strings with `+`, which Grindlang's `+` doesn't do.
    pub fn string_plus_hint(self) -> Option<&'static str> {
        match self {
            Surface::Lua => None,
            Surface::Ruby => Some("to join strings, use interpolation: `\"#{a}#{b}\"`"),
        }
    }
}
