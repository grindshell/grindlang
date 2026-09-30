//! The surface syntax a module was written in, and how diagnostics quote code in it.
//!
//! Everything after parsing — the resolver, the checker, lowering, and the backends — works on
//! the shared [`ast::Module`](crate::ast::Module), whichever syntax it was parsed from. The
//! resolver and the checker still quote code in their diagnostics (an operator, the test that
//! narrows an optional, a loop), so they ask the module's [`Surface`] how to write it instead of
//! spelling it in Lua inline. Lua is the only surface so far; `PLAN.md` Phase 10 plans a second.

use crate::ast::{BinOp, UnOp};

/// The syntax a module was parsed from. It changes how diagnostics spell code, never what a
/// program means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Surface {
    /// Lua's syntax (`SPEC.md`).
    #[default]
    Lua,
}

impl Surface {
    /// How binary operator `op` is written.
    pub fn binop(self, op: BinOp) -> &'static str {
        match self {
            Surface::Lua => op.symbol(),
        }
    }

    /// How unary operator `op` is written.
    pub fn unop(self, op: UnOp) -> &'static str {
        match self {
            Surface::Lua => op.symbol(),
        }
    }

    /// The test that narrows an optional to its value (`SPEC.md` §5.3), written after it.
    pub fn nil_test(self) -> &'static str {
        match self {
            Surface::Lua => "~= nil",
        }
    }

    /// The iterator a loop over an array uses.
    pub fn array_loop(self) -> &'static str {
        match self {
            Surface::Lua => "ipairs",
        }
    }

    /// The iterator a loop over a map uses.
    pub fn map_loop(self) -> &'static str {
        match self {
            Surface::Lua => "pairs",
        }
    }

    /// The binding of several values at once that takes a multi-value call (`SPEC.md` §5.5).
    pub fn parallel_binding(self) -> &'static str {
        match self {
            Surface::Lua => "a parallel `local`/assignment",
        }
    }

    /// The expression that picks one of two values by a condition, which is what an `and`/`or`
    /// whose operands differ in type was likely meant to be (`SPEC.md` §3.5).
    pub fn choice(self) -> &'static str {
        match self {
            Surface::Lua => "cond and a or b",
        }
    }

    /// A closure calling the named function `name`, which makes it a value (`SPEC.md` §5.5).
    pub fn closure_calling(self, name: &str) -> String {
        match self {
            Surface::Lua => format!("function(x) return {name}(x) end"),
        }
    }

    /// Why top-level code can't `return` (`E0309`), past the rule itself.
    pub fn top_level_return(self) -> &'static str {
        match self {
            Surface::Lua => "the only top-level `return` is a module's trailing export table",
        }
    }
}
