//! Ruby methods on values, and what each becomes (`SPEC-RUBY.md` §4.11).
//!
//! The table only renames: each method is an operator, a builtin of the catalog
//! (`runtime::builtins`, the single source of builtins), or an element access. `recv.name` with
//! no arguments or block is a field read unless `name` is in the table, so a record field named
//! `size` is read as `r[:size]`. A call on a host memory binding is a memory method call, and
//! `Math.f(x)` is the builtin `math.f(x)`.

use super::ast as rb;
use super::desugar::{Translator, binary, builtin, call, expr, index, name, number, unary};
use crate::ast::{BinOp, Expr, ExprKind, Ident, UnOp};
use crate::diagnostics::Span;
use crate::runtime::builtins;

/// The loops written as methods with a block (§4.5): each is a statement.
const LOOPS: &[&str] = &[
    "each",
    "each_with_index",
    "each_pair",
    "times",
    "upto",
    "step",
];

/// The collection methods with blocks (§4.12), which a later step translates.
const COLLECTIONS: &[&str] = &[
    "map", "select", "reject", "sum", "count", "any?", "all?", "find",
];

/// The methods the table translates, with how many arguments each takes (`None`: any number).
const TABLE: &[(&str, Option<usize>)] = &[
    ("length", Some(0)),
    ("size", Some(0)),
    ("nil?", Some(0)),
    ("empty?", Some(0)),
    ("zero?", Some(0)),
    ("to_s", Some(0)),
    ("to_f", Some(0)),
    ("abs", Some(0)),
    ("floor", Some(0)),
    ("ceil", Some(0)),
    ("clamp", Some(2)),
    ("upcase", Some(0)),
    ("downcase", Some(0)),
    ("div", Some(1)),
    ("first", Some(0)),
    ("last", Some(0)),
    ("push", Some(1)),
    ("call", None),
    ("freeze", Some(0)),
];

/// Whether `name` is a loop method (§4.5).
pub(super) fn is_loop(name: &str) -> bool {
    LOOPS.contains(&name)
}

/// Whether `name` is a method rather than a field when written `recv.name`.
pub(super) fn is_method(name: &str) -> bool {
    LOOPS.contains(&name) || COLLECTIONS.contains(&name) || TABLE.iter().any(|(n, _)| *n == name)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl Translator<'_> {
    /// `recv.name(args)`, or with `parens` false, `recv.name` alone.
    pub(super) fn method(
        &mut self,
        recv: &rb::Expr,
        method: &Ident,
        args: &[rb::Arg],
        block: Option<&rb::Block>,
        span: Span,
        parens: bool,
    ) -> Expr {
        let m = method.node.as_str();
        if self.is_memory(recv) {
            let rb::ExprKind::Name(mem) = &recv.kind else {
                unreachable!("a memory binding is a name")
            };
            let declared = self
                .cfg
                .methods
                .get(mem)
                .is_some_and(|ms| ms.contains_key(m));
            if declared {
                if let Some(b) = block {
                    self.error(
                        "E0521",
                        format!("the memory method `{mem}.{m}` doesn't take a block"),
                        b.span,
                    );
                }
                let args = self.pos_args(args);
                return expr(
                    ExprKind::MethodCall {
                        receiver: Box::new(name(mem, recv.span)),
                        method: method.clone(),
                        args,
                    },
                    span,
                );
            }
            if !parens && !is_method(m) {
                return field(name(mem, recv.span), method, span);
            }
            if !is_method(m) {
                self.error(
                    "E0601",
                    format!("the host memory `{mem}` has no method `{m}`"),
                    method.span,
                );
                return expr(ExprKind::Nil, span);
            }
        }
        if let rb::ExprKind::Const(c) = &recv.kind
            && c == "Math"
        {
            return self.math(method, args, block, span, parens);
        }

        if is_loop(m) {
            let msg = if block.is_some() {
                format!(
                    "`{m}` is a loop, so it's a statement; it can't be used as a value. To build \
                     a value in it, assign to a local declared before the loop"
                )
            } else {
                format!("`{m}` is a loop, which needs a block: `{m} {{ |x| … }}`")
            };
            self.error("E0602", msg, method.span);
            return expr(ExprKind::Nil, span);
        }
        if COLLECTIONS.contains(&m) {
            return self.not_yet(&format!("the collection method `{m}`"), method.span);
        }
        let Some(&(_, arity)) = TABLE.iter().find(|(n, _)| *n == m) else {
            // A field read: `r.name`. A name no field can have, or a call, is a method this
            // table doesn't have.
            let plain = !m.ends_with('?') && !m.ends_with('!');
            if !parens && plain {
                let base = self.expr(recv);
                return field(base, method, span);
            }
            self.error(
                "E0601",
                format!(
                    "the method `{m}` is not supported in Grindlang (SPEC-RUBY.md §4.11 lists \
                     the methods it has)"
                ),
                method.span,
            );
            return expr(ExprKind::Nil, span);
        };
        if let Some(b) = block {
            self.error("E0521", format!("`{m}` doesn't take a block"), b.span);
        }
        if let Some(want) = arity
            && args.len() != want
        {
            self.error(
                "E0602",
                format!(
                    "`{m}` takes {want} argument{}, found {}",
                    plural(want),
                    args.len()
                ),
                method.span,
            );
            return expr(ExprKind::Nil, span);
        }
        if m == "push" {
            self.error(
                "E0602",
                "`push` adds to an array, so it's a statement, not a value",
                method.span,
            );
            return expr(ExprKind::Nil, span);
        }
        if m == "last" && !self.pure(recv) {
            self.error(
                "E0602",
                "`last` reads its receiver twice, and this one has a call in it; assign it to a \
                 local first",
                recv.span,
            );
            return expr(ExprKind::Nil, span);
        }

        let x = self.expr(recv);
        let mut args = self.pos_args(args).into_iter();
        let at = method.span;
        match m {
            "length" | "size" => unary(UnOp::Len, x, span),
            "nil?" => binary(BinOp::Eq, x, expr(ExprKind::Nil, at), span),
            "empty?" => {
                let len = unary(UnOp::Len, x, span);
                binary(BinOp::Eq, len, number(0.0, at), span)
            }
            "zero?" => binary(BinOp::Eq, x, number(0.0, at), span),
            "to_s" => call(name("tostring", at), vec![x], span),
            "to_f" => call(name("tonumber", at), vec![x], span),
            "abs" | "floor" | "ceil" => builtin("math", m, at, vec![x], span),
            "clamp" => {
                let (lo, hi) = (args.next().expect("arity"), args.next().expect("arity"));
                let low = builtin("math", "max", at, vec![x, lo], span);
                builtin("math", "min", at, vec![low, hi], span)
            }
            "upcase" => builtin("string", "upper", at, vec![x], span),
            "downcase" => builtin("string", "lower", at, vec![x], span),
            "div" => binary(BinOp::FloorDiv, x, args.next().expect("arity"), span),
            "first" => index(x, number(1.0, at), false, span),
            "last" => {
                let len = unary(UnOp::Len, x.clone(), recv.span);
                index(x, len, false, span)
            }
            "call" => call(x, args.collect(), span),
            "freeze" => x,
            _ => unreachable!("every method in TABLE is translated"),
        }
    }

    /// `Math.f(x)` and `Math.name`: the builtin namespace `math`.
    fn math(
        &mut self,
        method: &Ident,
        args: &[rb::Arg],
        block: Option<&rb::Block>,
        span: Span,
        parens: bool,
    ) -> Expr {
        let m = method.node.as_str();
        if let Some(b) = block {
            self.error("E0521", format!("`Math.{m}` doesn't take a block"), b.span);
        }
        if builtins::member_sig("math", m).is_some() {
            let args = self.pos_args(args);
            return builtin("math", m, method.span, args, span);
        }
        if !parens && builtins::namespace_field_type("math", m).is_some() {
            return field(name("math", method.span), method, span);
        }
        self.error(
            "E0601",
            format!("`Math.{m}` is not supported in Grindlang"),
            method.span,
        );
        expr(ExprKind::Nil, span)
    }
}

fn field(base: Expr, name: &Ident, span: Span) -> Expr {
    expr(
        ExprKind::Field {
            base: Box::new(base),
            name: name.clone(),
        },
        span,
    )
}
