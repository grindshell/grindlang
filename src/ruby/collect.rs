//! Collection methods with blocks (`SPEC-RUBY.md` §4.12): `map`, `select`, `reject`, `sum`,
//! `count`, `any?`, `all?`, and `find`.
//!
//! A builtin's signature has no type variables to type a generic `map(xs, f)`, so each call
//! becomes a loop that builds its result in a temporary, and the call is that temporary.
//! `ys = xs.select { |x| x > 1 }` becomes:
//!
//! ```text
//! local select#1 = {}
//! for _, x in ipairs(xs) do
//!   if x > 1 then select#1[#select#1 + 1] = x end
//! end
//! local ys = select#1
//! ```
//!
//! The loop runs before the statement holding the call, so a call is translated only where its
//! value is the first thing that statement evaluates, which then can't tell the difference: the
//! whole right side of an assignment, the value of a `return` or of a block, or a loop's
//! receiver, and the start of a method chain in one of those (`xs.select { … }.map { … }.sum`,
//! [`Translator::value`]). Anywhere else, running the loop first could change what the program
//! does, so it is `E0610`.
//!
//! The block's last expression is its value for each element. In Ruby, `next` directly in the
//! block would give it `nil`, and `break` would make the whole call `nil`, so both are `E0611`.
//!
//! The temporary is declared and read at the span of the method's name, and the loop's
//! variables are declared at spans no other declaration has: the index at the empty span
//! before the method's name, and a hidden element (below) at the whole call's.

use super::ast as rb;
use super::desugar::{
    Tail, Translator, binary, declare, expr, ident, index, jump, name, negate, number, stat, unary,
};
use crate::ast::{BinOp, Block, Expr, ExprKind, Ident, IterExpr, RetStat, Stat, StatKind, UnOp};
use crate::diagnostics::Span;

/// The collection methods with blocks.
pub(super) const COLLECTIONS: &[&str] = &[
    "map", "select", "reject", "sum", "count", "any?", "all?", "find",
];

/// `name`, if it is a collection method.
fn collection_name(name: &str) -> Option<&'static str> {
    COLLECTIONS.iter().copied().find(|&c| c == name)
}

impl Translator<'_> {
    /// `e` where its value is the first thing its statement evaluates (the module docs): a
    /// collection method starting its method chain becomes a loop, added to `out`, and the
    /// temporary that loop builds its result in.
    pub(super) fn value(&mut self, e: &rb::Expr, out: &mut Vec<Stat>) -> Expr {
        let mut chain = Vec::new();
        self.chain_calls(e, &mut chain);
        let outer = (
            std::mem::replace(&mut self.chain, chain),
            std::mem::take(&mut self.loops),
        );
        let v = self.expr(e);
        out.append(&mut self.loops);
        (self.chain, self.loops) = outer;
        v
    }

    /// The collection method calls that start `e`'s method chain: `e`, if it is one, and those
    /// starting its receiver's chain.
    fn chain_calls(&self, e: &rb::Expr, out: &mut Vec<Span>) {
        let recv = match &e.kind {
            rb::ExprKind::MethodCall { recv, name, .. } | rb::ExprKind::Field { recv, name } => {
                if self.is_collection(recv, &name.node) {
                    out.push(e.span);
                }
                recv
            }
            rb::ExprKind::Index { base, .. } => base,
            rb::ExprKind::Paren(inner) => inner,
            _ => return,
        };
        self.chain_calls(recv, out);
    }

    /// Whether `recv.name` calls a collection method: `name` is one, and neither a method the
    /// host declares on the memory `recv` names nor a function of `Math`.
    fn is_collection(&self, recv: &rb::Expr, name: &str) -> bool {
        collection_name(name).is_some()
            && !self.memory_method(recv, name)
            && !matches!(&recv.kind, rb::ExprKind::Const(c) if c == "Math")
    }

    /// Whether `e` starts with a collection method call, whose loop is statements.
    pub(super) fn collects(&self, e: &rb::Expr) -> bool {
        let mut chain = Vec::new();
        self.chain_calls(e, &mut chain);
        !chain.is_empty()
    }

    /// Whether `e` is a collection method call that [`Self::value`] is making a temporary of,
    /// which is read again as cheaply as a local.
    pub(super) fn in_chain(&self, e: &rb::Expr) -> bool {
        self.chain.contains(&e.span)
    }

    /// `recv.m { |x| … }`, where `m` is a collection method and `span` the whole call: the loop,
    /// added to the loops of the value being translated, and the temporary it builds the
    /// result in.
    pub(super) fn collection(
        &mut self,
        recv: &rb::Expr,
        method: &Ident,
        args: &[rb::Arg],
        block: Option<&rb::Block>,
        span: Span,
    ) -> Expr {
        let m = collection_name(&method.node).expect("a collection method");
        let nil = expr(ExprKind::Nil, span);
        if !args.is_empty() {
            self.error(
                "E0602",
                format!("`{m}` takes no arguments, only a block"),
                method.span,
            );
            return nil;
        }
        if block.is_none() && m != "sum" {
            let hint = if m == "count" {
                "; for the number of elements, use `length`"
            } else {
                ""
            };
            self.error(
                "E0602",
                format!("`{m}` takes a block: `{m} {{ |x| … }}`{hint}"),
                method.span,
            );
            return nil;
        }
        if let Some(b) = block
            && b.params.len() > 1
        {
            self.error(
                "E0602",
                format!("`{m}` takes one block parameter, an element of the array or range"),
                b.span,
            );
            return nil;
        }
        if !self.chain.contains(&span) {
            self.error(
                "E0610",
                format!(
                    "`{m}` runs a loop before its statement, so it must be the whole right side \
                     of an assignment, the value of a `return` or a block, or a loop's receiver, \
                     or start a method chain there; assign it to a local first"
                ),
                method.span,
            );
            return nil;
        }

        let at = method.span;
        let temp = self.temp(m.trim_end_matches('?'));
        let t = || name(&temp, at);
        // Each pass's element is the block's parameter, unless there is none, or the method
        // keeps the element and the block assigns the parameter, which mustn't change what is
        // kept. Then it is a hidden `x#`, which the parameter starts as.
        let param = block.and_then(|b| b.params.first());
        let keeps = matches!(m, "select" | "reject" | "find");
        let elem = match param {
            Some(p) if !(keeps && self.scopes.assigned_params.contains(&p.span)) => p.clone(),
            _ => ident(&format!("{}#", param.map_or("_", |p| &p.node)), span),
        };
        let mut stats = Vec::new();
        if let Some(p) = param
            && p.span != elem.span
        {
            let local = StatKind::Local {
                names: vec![p.clone()],
                exprs: vec![name(&elem.node, elem.span)],
            };
            stats.push(stat(local, p.span));
        }

        // A range counts through its numbers, as `each` does (§4.5); anything else is an array.
        let over = match self.range(recv) {
            Some(bounds) => Err(bounds),
            None => Ok(self.expr(recv)),
        };

        let outer = (
            std::mem::take(&mut self.chain),
            std::mem::take(&mut self.loops),
        );
        let mut ret = None;
        let value = self.within(Some(m), |tr| match block {
            Some(b) => tr.pass(m, b, &mut stats, &mut ret),
            None => Some(name(&elem.node, elem.span)),
        });
        (self.chain, self.loops) = outer;

        if let Some(v) = value {
            let set = |value: Expr| {
                let assign = StatKind::Assign {
                    targets: vec![t()],
                    exprs: vec![value],
                };
                stat(assign, span)
            };
            let push = |item: Expr| {
                let len = unary(UnOp::Len, t(), at);
                let slot = index(t(), binary(BinOp::Add, len, number(1.0, at), at), false, at);
                let assign = StatKind::Assign {
                    targets: vec![slot],
                    exprs: vec![item],
                };
                stat(assign, span)
            };
            let when = |cond: Expr, stats: Vec<Stat>| {
                let then = Block {
                    stats,
                    ret: None,
                    span,
                };
                let if_ = StatKind::If {
                    arms: vec![(cond, then)],
                    else_block: None,
                };
                stat(if_, span)
            };
            let not = |v: Expr| {
                let at = v.span;
                negate(v, at)
            };
            let item = || name(&elem.node, elem.span);
            let stop = || stat(StatKind::Break, span);
            let consume = match m {
                "map" => push(v),
                "select" => when(v, vec![push(item())]),
                "reject" => when(not(v), vec![push(item())]),
                "sum" => set(binary(BinOp::Add, t(), v, span)),
                "count" => when(v, vec![set(binary(BinOp::Add, t(), number(1.0, at), span))]),
                "any?" => when(v, vec![set(expr(ExprKind::Bool(true), at)), stop()]),
                "all?" => when(not(v), vec![set(expr(ExprKind::Bool(false), at)), stop()]),
                "find" => when(v, vec![set(item()), stop()]),
                _ => unreachable!("every collection method is consumed"),
            };
            stats.push(consume);
        }

        let body = Block {
            stats,
            ret,
            span: block.map_or(span, |b| b.span),
        };
        let each = match over {
            Ok(arg) => {
                let key = ident(
                    "_",
                    Span {
                        start: at.start,
                        end: at.start,
                    },
                );
                StatKind::GenericFor {
                    names: vec![key, elem],
                    iter: IterExpr::IPairs { arg, span: at },
                    body,
                }
            }
            Err((start, end)) => StatKind::NumericFor {
                var: elem,
                start,
                end,
                step: None,
                body,
            },
        };
        let init = match m {
            "map" | "select" | "reject" => ExprKind::Table(Vec::new()),
            "sum" | "count" => ExprKind::Number(0.0),
            "any?" => ExprKind::Bool(false),
            "all?" => ExprKind::Bool(true),
            _ => ExprKind::Nil,
        };
        let local = StatKind::Local {
            names: vec![ident(&temp, at)],
            exprs: vec![expr(init, at)],
        };
        self.loops.push(stat(local, span));
        self.loops.push(stat(each, span));
        t()
    }

    /// A collection method's block as one pass of its loop, into `out`: every statement but the
    /// last, and then the value of the last, which it returns. `None` when the block gives no
    /// value: it ends in a `return`, or it has an error.
    fn pass(
        &mut self,
        m: &str,
        block: &rb::Block,
        out: &mut Vec<Stat>,
        ret: &mut Option<RetStat>,
    ) -> Option<Expr> {
        if let Some(names) = self.scopes.hoisted.get(&block.span) {
            out.push(declare(names.clone()));
        }
        let stmts = &block.body.stmts;
        let Some((last, init)) = stmts.split_last() else {
            self.error(
                "E0611",
                format!(
                    "`{m}`'s block is empty: its last expression is its value for each element"
                ),
                block.span,
            );
            return None;
        };
        for (i, s) in init.iter().enumerate() {
            self.stmt(s, Tail::None, out, ret);
            if let Some(word) = jump(s) {
                // `next` and `break` here are `E0611` already.
                if word == "return" {
                    self.error(
                        "E0606",
                        "this code never runs: it comes after a `return`",
                        stmts[i + 1].span,
                    );
                }
                return None;
            }
        }
        match &last.kind {
            rb::StmtKind::Expr(e) => Some(self.rhs(e, out)),
            // A `return` leaves the loop with no value to give; `next` and `break` are `E0611`.
            rb::StmtKind::Return(_) | rb::StmtKind::Next | rb::StmtKind::Break => {
                self.stmt(last, Tail::None, out, ret);
                None
            }
            _ => {
                self.error(
                    "E0611",
                    format!(
                        "the last statement of `{m}`'s block must be an expression: it is the \
                         block's value for each element"
                    ),
                    last.span,
                );
                None
            }
        }
    }
}
