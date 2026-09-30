//! Translating a Ruby-shaped [`rb::Program`] into the shared [`ast::Module`] (`SPEC-RUBY.md`
//! §4), which the resolver, the checker, lowering, and every backend then run as they would a
//! Lua module.
//!
//! The translator decides what the Ruby parser left open — whether `x` is a local or a call
//! ([`scope`]), whether `r.name` is a field or a method (`methods.rs`) — and rewrites each Ruby
//! form into the Lua-shaped construct SPEC-RUBY.md names for it. It needs no types: where a
//! choice depends on them (`a[i]` on an array or a map, `each` over one or the other), it emits
//! a construct that decides later (a zero-based index) or one the checker rejects with a Ruby
//! hint.
//!
//! ## Spans
//!
//! The resolver and the checker key what they learn about a name by the span of its
//! occurrence (its binding, and whether a guard narrowed it there), so every `Name` this
//! translator makes up gets a span no other name has, unless the two are the same variable read
//! at the same point in the program, where both would learn the same things:
//!
//! - a builtin a method becomes (`math` in `x.abs`) takes the method name's span;
//! - the `tostring` around an interpolation takes the two bytes before the interpolated
//!   expression (`#{`), where no other node starts;
//! - the memory binding `@name` reads takes the span of its `@`;
//! - a `case` subject, read again for each `when`, takes that `when` keyword's span;
//! - a hoisted `local x` takes the span of `x` in its first assignment, which is a use of the
//!   same local.
//!
//! Only names are looked up by span, so a call may share the span of the name it calls, and a
//! builtin's `math.abs` the span of its `math`.

use std::collections::HashSet;

use super::RubyConfig;
use super::ast as rb;
use super::scope::{self, Scopes};
use crate::ast::{
    self, BinOp, Block, ConstDecl, Expr, ExprKind, Field, FuncBody, FuncDecl, Ident, IterExpr,
    RetStat, Spanned, Stat, StatKind, TopDecl, UnOp,
};
use crate::diagnostics::{Diagnostic, Diagnostics, Span};
use crate::surface::Surface;
use crate::types::TypeConfig;

/// The name of the temporary a `case` evaluates its subject into, when the subject isn't a
/// local or constant it can read again. No Ruby name can contain a `#`.
const CASE_SUBJECT: &str = "case#";

/// The longest string interpolation or `when` list translated, in operands: each is a chain of
/// operators as deep as it is long.
const MAX_CHAIN: usize = 100;

/// Translate `program` for a host whose memory bindings and methods `cfg` declares.
pub(super) fn translate(
    program: &rb::Program,
    ruby: &RubyConfig,
    cfg: &TypeConfig,
) -> Result<ast::Module, Diagnostics> {
    let defs = program
        .items
        .iter()
        .filter_map(|item| match item {
            rb::Item::Def(d) => Some(d.name.node.clone()),
            _ => None,
        })
        .collect();
    let mut t = Translator {
        ruby,
        cfg,
        scopes: scope::analyze(program),
        defs,
        diags: Diagnostics::new(),
        in_def: false,
    };
    let module = t.program(program);
    if t.diags.has_errors() {
        Err(t.diags)
    } else {
        Ok(module)
    }
}

/// Whether a body's last statement returns its value (`SPEC-RUBY.md` §4.2).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tail {
    /// Not the last statement of a `def` or lambda body, or not in one at all.
    None,
    /// An expression here becomes `return expr`.
    Return,
}

pub(super) struct Translator<'a> {
    pub(super) ruby: &'a RubyConfig,
    pub(super) cfg: &'a TypeConfig,
    pub(super) scopes: Scopes,
    /// The names of the program's `def`s.
    pub(super) defs: HashSet<String>,
    pub(super) diags: Diagnostics,
    /// Whether the code being translated is in a `def`, including a block or lambda in one.
    pub(super) in_def: bool,
}

// ---- building nodes ------------------------------------------------------------------------

pub(super) fn expr(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

pub(super) fn name(n: &str, span: Span) -> Expr {
    expr(ExprKind::Name(n.to_string()), span)
}

pub(super) fn ident(n: &str, span: Span) -> Ident {
    Spanned::new(n.to_string(), span)
}

pub(super) fn number(n: f64, span: Span) -> Expr {
    expr(ExprKind::Number(n), span)
}

pub(super) fn call(callee: Expr, args: Vec<Expr>, span: Span) -> Expr {
    expr(
        ExprKind::Call {
            callee: Box::new(callee),
            args,
        },
        span,
    )
}

/// `ns.member(args)`, a builtin namespace's function, with `ns` and `member` at `anchor`.
pub(super) fn builtin(ns: &str, member: &str, anchor: Span, args: Vec<Expr>, span: Span) -> Expr {
    let callee = expr(
        ExprKind::Field {
            base: Box::new(name(ns, anchor)),
            name: ident(member, anchor),
        },
        anchor,
    );
    call(callee, args, span)
}

pub(super) fn binary(op: BinOp, lhs: Expr, rhs: Expr, span: Span) -> Expr {
    expr(
        ExprKind::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        span,
    )
}

pub(super) fn unary(op: UnOp, operand: Expr, span: Span) -> Expr {
    expr(
        ExprKind::Unary {
            op,
            operand: Box::new(operand),
        },
        span,
    )
}

pub(super) fn index(base: Expr, idx: Expr, zero_based: bool, span: Span) -> Expr {
    expr(
        ExprKind::Index {
            base: Box::new(base),
            index: Box::new(idx),
            zero_based,
        },
        span,
    )
}

fn stat(kind: StatKind, span: Span) -> Stat {
    Stat { kind, span }
}

/// `local a, b` with no values.
fn declare(names: Vec<Ident>) -> Stat {
    let span = names[0].span;
    stat(
        StatKind::Local {
            names,
            exprs: Vec::new(),
        },
        span,
    )
}

/// The logical negation of a condition: `a == b` and `a ~= b` swap, `not not c` is `c`, so
/// `unless x.nil?` is `x ~= nil` (`SPEC-RUBY.md` §4.8). Conditions are `bool`, so each is exact.
fn negate(e: Expr, span: Span) -> Expr {
    match e.kind {
        ExprKind::Binary {
            op: op @ (BinOp::Eq | BinOp::Ne),
            lhs,
            rhs,
        } => expr(
            ExprKind::Binary {
                op: if op == BinOp::Eq {
                    BinOp::Ne
                } else {
                    BinOp::Eq
                },
                lhs,
                rhs,
            },
            e.span,
        ),
        ExprKind::Unary {
            op: UnOp::Not,
            operand,
        } => *operand,
        kind => unary(UnOp::Not, expr(kind, e.span), span),
    }
}

/// The keyword that starts the construct at `span`, which is `len` bytes long.
fn keyword(span: Span, len: u32) -> Span {
    Span {
        start: span.start,
        end: span.start + len,
    }
}

impl<'a> Translator<'a> {
    pub(super) fn error(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::error(code, msg, span));
    }

    fn error_count(&self) -> usize {
        self.diags.iter().count()
    }

    /// A form a later step of the Ruby surface translates (`PLAN.md` Phase 10). Returns a
    /// placeholder.
    pub(super) fn not_yet(&mut self, what: &str, span: Span) -> Expr {
        self.error(
            "E0699",
            format!(
                "{what} is not supported yet: it is planned for a later step of the Ruby surface"
            ),
            span,
        );
        expr(ExprKind::Nil, span)
    }

    // ---- top level ---------------------------------------------------------------------------

    fn program(&mut self, p: &rb::Program) -> ast::Module {
        let mut decls = Vec::new();
        if let Some(names) = self.scopes.hoisted.get(&p.span) {
            decls.push(TopDecl::Stat(Box::new(declare(names.clone()))));
        }
        let mut visibility = rb::Visibility::Public;
        let mut exports: Vec<Ident> = Vec::new();
        let mut private = false;
        for item in &p.items {
            match item {
                rb::Item::Def(d) => {
                    if d.visibility.unwrap_or(visibility) == rb::Visibility::Public {
                        exports.push(d.name.clone());
                    } else {
                        private = true;
                    }
                    decls.push(TopDecl::Function(self.def(d)));
                }
                rb::Item::Visibility(v) => visibility = v.node,
                rb::Item::Stmt(s) => self.top_stmt(s, &mut decls, &mut exports),
            }
        }
        // Private `def`s are plain functions, visible to the whole file as Ruby methods are, so
        // an export table leaves them out (`SPEC-RUBY.md` §4.1).
        let export = private.then(|| {
            let fields = exports
                .into_iter()
                .map(|n| Field::Named {
                    value: name(&n.node, n.span),
                    name: n,
                })
                .collect();
            Spanned::new(fields, p.span)
        });
        ast::Module {
            decls,
            export,
            span: p.span,
            surface: Surface::Ruby,
        }
    }

    fn top_stmt(&mut self, s: &rb::Stmt, decls: &mut Vec<TopDecl>, exports: &mut Vec<Ident>) {
        // `CONST = value` directly at the top level: an exported constant.
        if let rb::StmtKind::Assign { targets, values } = &s.kind
            && let [target] = targets.as_slice()
            && let rb::ExprKind::Const(c) = &target.kind
        {
            if values.len() != 1 {
                self.error(
                    "E0604",
                    format!("`{c}` is a constant, which takes one value"),
                    s.span,
                );
                return;
            }
            let name = ident(c, target.span);
            decls.push(TopDecl::Const(ConstDecl {
                name: name.clone(),
                value: self.expr(&values[0]),
                span: s.span,
            }));
            exports.push(name);
            return;
        }
        let mut stats = Vec::new();
        let mut ret = None;
        self.stmt(s, Tail::None, &mut stats, &mut ret);
        decls.extend(stats.into_iter().map(|st| TopDecl::Stat(Box::new(st))));
        // Top-level code can't return (`E0309`); leave it for the resolver to say so.
        if let Some(ret) = ret {
            let span = ret.span;
            let block = Block {
                stats: Vec::new(),
                ret: Some(ret),
                span,
            };
            decls.push(TopDecl::Stat(Box::new(stat(StatKind::Do(block), span))));
        }
    }

    fn def(&mut self, d: &rb::Def) -> FuncDecl {
        let (annotations, diags) = crate::annotations::parse_doc_block(&d.docs, Surface::Ruby);
        for diag in diags {
            self.diags.push(diag);
        }
        let outer = std::mem::replace(&mut self.in_def, true);
        let block = self.body(&d.body, Some(d.span), Tail::Return);
        self.in_def = outer;
        FuncDecl {
            name: d.name.clone(),
            local: false,
            body: FuncBody {
                params: d.params.clone(),
                block,
                span: d.span,
            },
            annotations,
            span: d.span,
        }
    }

    // ---- statements --------------------------------------------------------------------------

    /// A body as a block. `scope` is what opens the body's scope, when it does, whose hoisted
    /// locals are declared first.
    pub(super) fn body(&mut self, body: &rb::Body, scope: Option<Span>, tail: Tail) -> Block {
        let mut stats = Vec::new();
        if let Some(owner) = scope
            && let Some(names) = self.scopes.hoisted.get(&owner)
        {
            stats.push(declare(names.clone()));
        }
        let mut ret = None;
        for (i, s) in body.stmts.iter().enumerate() {
            let last = i + 1 == body.stmts.len();
            self.stmt(
                s,
                if last { tail } else { Tail::None },
                &mut stats,
                &mut ret,
            );
            if !last && matches!(s.kind, rb::StmtKind::Return(_) | rb::StmtKind::Break) {
                let word = if ret.is_some() { "return" } else { "break" };
                self.error(
                    "E0606",
                    format!("this code never runs: it comes after a `{word}`"),
                    body.stmts[i + 1].span,
                );
                break;
            }
        }
        Block {
            stats,
            ret,
            span: body.span,
        }
    }

    /// One statement as a block of its own: what a modifier (`stmt if c`) wraps.
    fn single(&mut self, s: &rb::Stmt, tail: Tail) -> Block {
        let mut stats = Vec::new();
        let mut ret = None;
        self.stmt(s, tail, &mut stats, &mut ret);
        Block {
            stats,
            ret,
            span: s.span,
        }
    }

    fn stmt(&mut self, s: &rb::Stmt, tail: Tail, out: &mut Vec<Stat>, ret: &mut Option<RetStat>) {
        let span = s.span;
        match &s.kind {
            rb::StmtKind::Expr(e) => self.expr_stmt(e, span, tail, out, ret),
            rb::StmtKind::Assign { targets, values } => self.assign(targets, values, span, out),
            rb::StmtKind::OpAssign { target, op, value } => {
                self.op_assign(target, *op, value, span, out)
            }
            rb::StmtKind::While { cond, body, until } => {
                let c = self.expr(cond);
                let cond = if *until { negate(c, cond.span) } else { c };
                let body = self.body(body, None, Tail::None);
                out.push(stat(StatKind::While { cond, body }, span));
            }
            rb::StmtKind::Loop(body) => {
                let cond = expr(ExprKind::Bool(true), keyword(span, 4));
                let body = self.body(body, None, Tail::None);
                out.push(stat(StatKind::While { cond, body }, span));
            }
            rb::StmtKind::Break => out.push(stat(StatKind::Break, span)),
            rb::StmtKind::Next => {
                self.not_yet("`next`", span);
            }
            rb::StmtKind::Return(values) => {
                let exprs = values.iter().map(|v| self.expr(v)).collect();
                *ret = Some(RetStat { exprs, span });
            }
            rb::StmtKind::Modified {
                stmt: inner,
                modifier,
                cond,
            } => {
                let c = self.expr(cond);
                let negated = matches!(modifier, rb::Modifier::Unless | rb::Modifier::Until);
                let cond = if negated { negate(c, cond.span) } else { c };
                match modifier {
                    rb::Modifier::If | rb::Modifier::Unless => {
                        let block = self.single(inner, tail);
                        out.push(stat(
                            StatKind::If {
                                arms: vec![(cond, block)],
                                else_block: None,
                            },
                            span,
                        ));
                    }
                    rb::Modifier::While | rb::Modifier::Until => {
                        let body = self.single(inner, Tail::None);
                        out.push(stat(StatKind::While { cond, body }, span));
                    }
                }
            }
        }
    }

    fn expr_stmt(
        &mut self,
        e: &rb::Expr,
        span: Span,
        tail: Tail,
        out: &mut Vec<Stat>,
        ret: &mut Option<RetStat>,
    ) {
        match &e.kind {
            rb::ExprKind::If(if_) => return self.if_stmt(if_, e.span, tail, out),
            rb::ExprKind::Case(case) => return self.case_stmt(case, e.span, tail, out),
            rb::ExprKind::MethodCall {
                recv,
                name,
                args,
                block: Some(block),
            } if super::methods::is_loop(&name.node) && !self.is_memory(recv) => {
                return self.iteration(recv, name, args, block, span, out);
            }
            rb::ExprKind::Binary {
                op: rb::BinOp::Append,
                lhs,
                rhs,
            } => return self.push(lhs, rhs, e.span, out),
            rb::ExprKind::MethodCall {
                recv,
                name,
                args,
                block: None,
            } if name.node == "push" && !self.is_memory(recv) => {
                match args.as_slice() {
                    [rb::Arg::Pos(item)] => self.push(recv, item, e.span, out),
                    _ => self.error(
                        "E0602",
                        "`push` takes one value, which it adds to the end of the array",
                        name.span,
                    ),
                }
                return;
            }
            // `CONST.freeze` does nothing: top-level values are frozen anyway (§4.1).
            rb::ExprKind::Field { recv, name } if name.node == "freeze" && tail == Tail::None => {
                self.expr(recv);
                return;
            }
            _ => {}
        }
        let before = self.error_count();
        let v = self.expr(e);
        if tail == Tail::Return {
            *ret = Some(RetStat {
                exprs: vec![v],
                span,
            });
        } else if matches!(v.kind, ExprKind::Call { .. } | ExprKind::MethodCall { .. }) {
            out.push(stat(StatKind::Call(v), span));
        } else if self.error_count() == before {
            self.error(
                "E0605",
                "this value is never used: only a call can be a statement, except at the end of a \
                 `def` or lambda, which returns it",
                e.span,
            );
        }
    }

    fn if_stmt(&mut self, if_: &rb::If, span: Span, tail: Tail, out: &mut Vec<Stat>) {
        let mut arms = Vec::new();
        for (c, body) in &if_.arms {
            let cond = self.expr(c);
            let cond = if if_.unless {
                negate(cond, c.span)
            } else {
                cond
            };
            arms.push((cond, self.body(body, None, tail)));
        }
        let else_block = if_.else_body.as_ref().map(|b| self.body(b, None, tail));
        out.push(stat(StatKind::If { arms, else_block }, span));
    }

    /// `case x when a, b … when lo..hi … else … end`: an `if` chain testing `x == a or x == b`,
    /// then `x >= lo and x <= hi` (`SPEC-RUBY.md` §4.4). A local or constant subject is read
    /// again for each `when`; any other is evaluated once, into a temporary.
    fn case_stmt(&mut self, case: &rb::Case, span: Span, tail: Tail, out: &mut Vec<Stat>) {
        let direct = match &case.subject.kind {
            rb::ExprKind::Name(n) if self.scopes.locals.contains(&case.subject.span) => {
                Some(n.clone())
            }
            rb::ExprKind::Const(n) => Some(n.clone()),
            _ => None,
        };
        let subject = self.expr(&case.subject);
        let reread = direct.as_deref().unwrap_or(CASE_SUBJECT);
        let mut arms = Vec::new();
        for when in &case.whens {
            let at = keyword(when.span, 4);
            if when.values.len() > MAX_CHAIN {
                self.diags.push(crate::parser::too_deeply_nested(when.span));
                continue;
            }
            let mut cond: Option<Expr> = None;
            for v in &when.values {
                let test = match &v.kind {
                    rb::ExprKind::Range { lo, hi, exclusive } => {
                        let (lo_e, hi_e) = (self.expr(lo), self.expr(hi));
                        let upper = if *exclusive { BinOp::Lt } else { BinOp::Le };
                        binary(
                            BinOp::And,
                            binary(BinOp::Ge, name(reread, at), lo_e, lo.span),
                            binary(upper, name(reread, at), hi_e, hi.span),
                            v.span,
                        )
                    }
                    _ => {
                        let value = self.expr(v);
                        binary(BinOp::Eq, name(reread, at), value, v.span)
                    }
                };
                cond = Some(match cond {
                    None => test,
                    Some(prev) => {
                        let span = prev.span.to(test.span);
                        binary(BinOp::Or, prev, test, span)
                    }
                });
            }
            let Some(cond) = cond else { continue };
            arms.push((cond, self.body(&when.body, None, tail)));
        }
        let else_block = case.else_body.as_ref().map(|b| self.body(b, None, tail));
        let chain = stat(StatKind::If { arms, else_block }, span);
        if direct.is_some() {
            out.push(chain);
            return;
        }
        let temp = stat(
            StatKind::Local {
                names: vec![ident(CASE_SUBJECT, keyword(span, 4))],
                exprs: vec![subject],
            },
            case.subject.span,
        );
        let block = Block {
            stats: vec![temp, chain],
            ret: None,
            span,
        };
        out.push(stat(StatKind::Do(block), span));
    }

    fn assign(
        &mut self,
        targets: &[rb::Expr],
        values: &[rb::Expr],
        span: Span,
        out: &mut Vec<Stat>,
    ) {
        // The scope analysis declares every target of an assignment in place, or none.
        let declare = targets.iter().all(|t| {
            matches!(t.kind, rb::ExprKind::Name(_)) && self.scopes.in_place.contains(&t.span)
        });
        let exprs: Vec<Expr> = values.iter().map(|v| self.expr(v)).collect();
        if declare {
            let names = targets
                .iter()
                .map(|t| match &t.kind {
                    rb::ExprKind::Name(n) => ident(n, t.span),
                    _ => unreachable!("checked above"),
                })
                .collect();
            out.push(stat(StatKind::Local { names, exprs }, span));
        } else {
            let targets = targets.iter().map(|t| self.target(t)).collect();
            out.push(stat(StatKind::Assign { targets, exprs }, span));
        }
    }

    /// `x += e` as `x = x + e`, and `x ||= d` as `x = x or d` (`SPEC-RUBY.md` §4.7). The target
    /// is read and written, so it may not hold a call, which would run twice.
    fn op_assign(
        &mut self,
        target: &rb::Expr,
        op: rb::AssignOp,
        value: &rb::Expr,
        span: Span,
        out: &mut Vec<Stat>,
    ) {
        if !self.pure(target) {
            self.error(
                "E0602",
                format!(
                    "`{}` would evaluate this target twice, and it has a call in it; assign what \
                     it's in to a local first",
                    op.symbol()
                ),
                target.span,
            );
            return;
        }
        let t = self.target(target);
        let read = self.place(target);
        let v = self.expr(value);
        let op = match op {
            rb::AssignOp::Add => BinOp::Add,
            rb::AssignOp::Sub => BinOp::Sub,
            rb::AssignOp::Mul => BinOp::Mul,
            rb::AssignOp::Div => BinOp::Div,
            rb::AssignOp::Mod => BinOp::Mod,
            rb::AssignOp::Pow => BinOp::Pow,
            rb::AssignOp::Or => BinOp::Or,
            rb::AssignOp::And => BinOp::And,
        };
        let rhs = binary(op, read, v, span);
        out.push(stat(
            StatKind::Assign {
                targets: vec![t],
                exprs: vec![rhs],
            },
            span,
        ));
    }

    /// `a << x` or `a.push(x)`: `a[#a + 1] = x`, a statement (`SPEC-RUBY.md` §4.9).
    fn push(&mut self, arr: &rb::Expr, item: &rb::Expr, span: Span, out: &mut Vec<Stat>) {
        if !self.pure(arr) {
            self.error(
                "E0602",
                "adding to this array would evaluate it twice, and it has a call in it; assign \
                 it to a local first",
                arr.span,
            );
            return;
        }
        let a = self.place(arr);
        let len = unary(UnOp::Len, a.clone(), arr.span);
        let at = binary(BinOp::Add, len, number(1.0, span), span);
        let target = index(a, at, false, span);
        let v = self.expr(item);
        out.push(stat(
            StatKind::Assign {
                targets: vec![target],
                exprs: vec![v],
            },
            span,
        ));
    }

    /// The iteration forms (`SPEC-RUBY.md` §4.5): each is a `for` loop, so it is a statement.
    fn iteration(
        &mut self,
        recv: &rb::Expr,
        method: &Ident,
        args: &[rb::Arg],
        block: &rb::Block,
        span: Span,
        out: &mut Vec<Stat>,
    ) {
        let m = method.node.as_str();
        // A range receiver counts, as `(a..b).each` does.
        let mut inner = recv;
        while let rb::ExprKind::Paren(e) = &inner.kind {
            inner = e;
        }
        let range = match &inner.kind {
            rb::ExprKind::Range { lo, hi, exclusive } if m == "each" => Some((lo, hi, *exclusive)),
            _ => None,
        };
        let counting = range.is_some() || matches!(m, "times" | "upto" | "step");
        let want_args = match m {
            "upto" => 1,
            "step" => 2,
            _ => 0,
        };
        if args.len() != want_args {
            self.error(
                "E0602",
                format!(
                    "`{m}` takes {want_args} argument{}, found {}",
                    if want_args == 1 { "" } else { "s" },
                    args.len()
                ),
                method.span,
            );
            return;
        }
        let (min, max, what) = match m {
            _ if counting => (0, 1, "at most one block parameter, the number"),
            "each_pair" => (2, 2, "two block parameters, a key and a value"),
            "each_with_index" => (
                1,
                2,
                "one or two block parameters, an element and its index",
            ),
            _ => (
                1,
                2,
                "one block parameter to loop over an array's elements, or two to loop over a \
                 map's keys and values",
            ),
        };
        let params = &block.params;
        if params.len() < min || params.len() > max {
            self.error("E0602", format!("`{m}` takes {what}"), block.span);
            return;
        }
        let var = |i: usize| -> Ident {
            params
                .get(i)
                .cloned()
                .unwrap_or_else(|| ident("_", method.span))
        };

        if counting {
            let (start, end, step) = match range {
                Some((lo, hi, exclusive)) => {
                    let (start, hi_e) = (self.expr(lo), self.expr(hi));
                    let end = if exclusive {
                        binary(BinOp::Sub, hi_e, number(1.0, hi.span), hi.span)
                    } else {
                        hi_e
                    };
                    (start, end, None)
                }
                None if m == "times" => {
                    let n = self.expr(recv);
                    let end = binary(BinOp::Sub, n, number(1.0, method.span), recv.span);
                    (number(0.0, method.span), end, None)
                }
                None => {
                    let start = self.expr(recv);
                    let mut args = self.pos_args(args).into_iter();
                    let end = args.next().unwrap_or_else(|| number(0.0, method.span));
                    (start, end, args.next())
                }
            };
            let body = self.body(&block.body, Some(block.span), Tail::None);
            let var = var(0);
            out.push(stat(
                StatKind::NumericFor {
                    var,
                    start,
                    end,
                    step,
                    body,
                },
                span,
            ));
            return;
        }

        let arg = self.expr(recv);
        let mut body = self.body(&block.body, Some(block.span), Tail::None);
        let (names, iter) = match (m, params.len()) {
            ("each", 2) | ("each_pair", _) => (
                vec![var(0), var(1)],
                IterExpr::Pairs {
                    arg,
                    span: method.span,
                },
            ),
            ("each_with_index", 2) => {
                // `ipairs` counts from 1 and Ruby from 0:
                // `for i#, x in ipairs(xs) do local i = i# - 1 … end`.
                let i = var(1);
                let counter = format!("{}#", i.node);
                let from_zero = binary(
                    BinOp::Sub,
                    name(&counter, i.span),
                    number(1.0, i.span),
                    i.span,
                );
                let local = StatKind::Local {
                    names: vec![i.clone()],
                    exprs: vec![from_zero],
                };
                body.stats.insert(0, stat(local, i.span));
                (
                    vec![ident(&counter, method.span), var(0)],
                    IterExpr::IPairs {
                        arg,
                        span: method.span,
                    },
                )
            }
            _ => (
                vec![ident("_", method.span), var(0)],
                IterExpr::IPairs {
                    arg,
                    span: method.span,
                },
            ),
        };
        out.push(stat(StatKind::GenericFor { names, iter, body }, span));
    }

    // ---- assignment targets --------------------------------------------------------------------

    /// An assignment target.
    fn target(&mut self, t: &rb::Expr) -> Expr {
        match &t.kind {
            rb::ExprKind::Const(c) => {
                self.error(
                    "E0604",
                    format!(
                        "`{c}` is a constant, which is assigned only directly at the top level, \
                         once; use a lowercase name for a variable"
                    ),
                    t.span,
                );
                name(c, t.span)
            }
            _ => self.place(t),
        }
    }

    /// Reading what an assignment target names. A field is read as a field, even with a name
    /// the method table has (`r.size += 1`).
    fn place(&mut self, t: &rb::Expr) -> Expr {
        match &t.kind {
            rb::ExprKind::Name(n) | rb::ExprKind::Const(n) => name(n, t.span),
            rb::ExprKind::Ivar(f) => self.ivar(f, t.span),
            rb::ExprKind::Field { recv, name: field } => {
                let base = self.expr(recv);
                expr(
                    ExprKind::Field {
                        base: Box::new(base),
                        name: field.clone(),
                    },
                    t.span,
                )
            }
            rb::ExprKind::Index { base, index } => self.index(base, index, t.span),
            rb::ExprKind::Paren(inner) => self.place(inner),
            _ => self.expr(t),
        }
    }

    /// Whether evaluating `e` twice is the same as once: it names a place and holds no call.
    pub(super) fn pure(&self, e: &rb::Expr) -> bool {
        match &e.kind {
            rb::ExprKind::Name(n) => {
                self.scopes.locals.contains(&e.span) || self.cfg.memory.contains_key(n)
            }
            rb::ExprKind::Const(_)
            | rb::ExprKind::Ivar(_)
            | rb::ExprKind::Nil
            | rb::ExprKind::Bool(_)
            | rb::ExprKind::Number(_)
            | rb::ExprKind::Symbol(_) => true,
            rb::ExprKind::Str(parts) => parts.iter().all(|p| matches!(p, rb::StrPart::Text(_))),
            rb::ExprKind::Field { recv, name } => {
                !super::methods::is_method(&name.node) && self.pure(recv)
            }
            rb::ExprKind::Index { base, index } => self.pure(base) && self.pure(index),
            rb::ExprKind::Paren(inner) => self.pure(inner),
            _ => false,
        }
    }

    // ---- expressions -------------------------------------------------------------------------

    pub(super) fn expr(&mut self, e: &rb::Expr) -> Expr {
        let span = e.span;
        let kind = match &e.kind {
            rb::ExprKind::Nil => ExprKind::Nil,
            rb::ExprKind::Bool(b) => ExprKind::Bool(*b),
            rb::ExprKind::Number(n) => ExprKind::Number(*n),
            rb::ExprKind::Str(parts) => return self.string(parts, span),
            rb::ExprKind::Symbol(s) => ExprKind::Str(s.clone()),
            rb::ExprKind::Name(n) => return self.name(n, span),
            rb::ExprKind::Const(c) => ExprKind::Name(c.clone()),
            rb::ExprKind::Ivar(f) => return self.ivar(f, span),
            rb::ExprKind::Array(items) => ExprKind::Table(
                items
                    .iter()
                    .map(|i| Field::Positional(self.expr(i)))
                    .collect(),
            ),
            rb::ExprKind::Hash(pairs) => {
                if pairs.is_empty() {
                    self.error(
                        "E0608",
                        "an empty hash is not supported yet: Grindlang has no way to write an \
                         empty map, so a hash starts with at least one entry",
                        span,
                    );
                    return expr(ExprKind::Nil, span);
                }
                ExprKind::Table(
                    pairs
                        .iter()
                        .map(|p| match &p.key {
                            rb::Key::Label(k) => Field::Named {
                                name: k.clone(),
                                value: self.expr(&p.value),
                            },
                            rb::Key::Expr(k) => Field::Keyed {
                                key: self.expr(k),
                                value: self.expr(&p.value),
                            },
                        })
                        .collect(),
                )
            }
            rb::ExprKind::Lambda(block) => ExprKind::Function(self.lambda(block)),
            rb::ExprKind::Range { .. } => {
                self.error(
                    "E0603",
                    "a range is only a loop's receiver, `(a..b).each { |i| … }`, or a `when` \
                     value",
                    span,
                );
                ExprKind::Nil
            }
            rb::ExprKind::Call { name, args, block } => {
                return self.call(name, args, block.as_deref(), span);
            }
            rb::ExprKind::MethodCall {
                recv,
                name,
                args,
                block,
            } => return self.method(recv, name, args, block.as_deref(), span, true),
            rb::ExprKind::Field { recv, name } => {
                return self.method(recv, name, &[], None, span, false);
            }
            rb::ExprKind::CallValue { callee, args } => ExprKind::Call {
                callee: Box::new(self.expr(callee)),
                args: self.pos_args(args),
            },
            rb::ExprKind::Index { base, index } => return self.index(base, index, span),
            rb::ExprKind::Binary { op, lhs, rhs } => return self.binary(*op, lhs, rhs, span),
            rb::ExprKind::Unary { op, operand } => {
                let v = self.expr(operand);
                return match op {
                    rb::UnOp::Neg => unary(UnOp::Neg, v, span),
                    rb::UnOp::Not => negate(v, span),
                };
            }
            rb::ExprKind::Ternary { .. } => return self.not_yet("`?:`", span),
            rb::ExprKind::If(_) => {
                return self.not_yet("an `if` or `unless` used as a value", span);
            }
            rb::ExprKind::Case(_) => return self.not_yet("a `case` used as a value", span),
            rb::ExprKind::Paren(inner) => ExprKind::Paren(Box::new(self.expr(inner))),
        };
        expr(kind, span)
    }

    /// A name that isn't assigned: a local, a memory binding, or a call with no arguments.
    fn name(&mut self, n: &str, span: Span) -> Expr {
        if self.scopes.locals.contains(&span) || self.cfg.memory.contains_key(n) {
            return name(n, span);
        }
        if self.in_def && self.scopes.top_level.contains(n) && !self.defs.contains(n) {
            self.error(
                "E0524",
                format!(
                    "a `def` can't read the top-level local `{n}`: as in Ruby, a `def` sees only \
                     its own locals. Make it a constant (`{}`) or pass it in",
                    n.to_uppercase()
                ),
                span,
            );
            return expr(ExprKind::Nil, span);
        }
        call(name(n, span), Vec::new(), span)
    }

    /// `@name`: the field `name` of the memory binding the host designates for `@`
    /// (`SPEC-RUBY.md` §4.10).
    pub(super) fn ivar(&mut self, field: &str, span: Span) -> Expr {
        let mem = match &self.ruby.ivar_memory {
            Some(m) if self.cfg.memory.contains_key(m) => m.clone(),
            Some(m) => {
                let m = m.clone();
                self.error(
                    "E0524",
                    format!(
                        "`@{field}` reads the host memory `{m}`, which the host has not provided"
                    ),
                    span,
                );
                return expr(ExprKind::Nil, span);
            }
            None => {
                self.error(
                    "E0524",
                    format!(
                        "`@{field}` reads host memory, but the host has designated no memory \
                         binding for `@` (`RubyConfig::ivar_memory`)"
                    ),
                    span,
                );
                return expr(ExprKind::Nil, span);
            }
        };
        let at = Span {
            start: span.start,
            end: span.start + 1,
        };
        expr(
            ExprKind::Field {
                base: Box::new(name(&mem, at)),
                name: ident(field, span),
            },
            span,
        )
    }

    /// `base[i]`, zero-based where `base` is an array (C9, `SPEC-RUBY.md` §4.9). The parser has
    /// rejected a literal negative index (`E0525`).
    fn index(&mut self, base: &rb::Expr, idx: &rb::Expr, span: Span) -> Expr {
        let b = self.expr(base);
        let i = self.expr(idx);
        index(b, i, true, span)
    }

    fn string(&mut self, parts: &[rb::StrPart], span: Span) -> Expr {
        if parts.len() > MAX_CHAIN {
            self.diags.push(crate::parser::too_deeply_nested(span));
            return expr(ExprKind::Nil, span);
        }
        let mut pieces: Vec<Expr> = parts
            .iter()
            .map(|p| match p {
                rb::StrPart::Text(t) => expr(ExprKind::Str(t.clone()), span),
                rb::StrPart::Interp(e) => {
                    let v = self.expr(e);
                    let at = Span {
                        start: e.span.start.saturating_sub(2),
                        end: e.span.start,
                    };
                    call(name("tostring", at), vec![v], e.span)
                }
            })
            .collect();
        // `..` is right-associative, as Lua writes the same string.
        let Some(mut acc) = pieces.pop() else {
            return expr(ExprKind::Str(String::new()), span);
        };
        while let Some(piece) = pieces.pop() {
            acc = binary(BinOp::Concat, piece, acc, span);
        }
        acc
    }

    fn binary(&mut self, op: rb::BinOp, lhs: &rb::Expr, rhs: &rb::Expr, span: Span) -> Expr {
        let is_str = |e: &rb::Expr| matches!(e.kind, rb::ExprKind::Str(_));
        if op == rb::BinOp::Add && (is_str(lhs) || is_str(rhs)) {
            self.error(
                "E0600",
                "`+` doesn't join strings in Grindlang; use interpolation: `\"#{a}#{b}\"`",
                span,
            );
            return expr(ExprKind::Nil, span);
        }
        let op = match op {
            rb::BinOp::Add => BinOp::Add,
            rb::BinOp::Sub => BinOp::Sub,
            rb::BinOp::Mul => BinOp::Mul,
            rb::BinOp::Div => BinOp::Div,
            rb::BinOp::Mod => BinOp::Mod,
            rb::BinOp::Pow => BinOp::Pow,
            rb::BinOp::Eq => BinOp::Eq,
            rb::BinOp::Ne => BinOp::Ne,
            rb::BinOp::Lt => BinOp::Lt,
            rb::BinOp::Le => BinOp::Le,
            rb::BinOp::Gt => BinOp::Gt,
            rb::BinOp::Ge => BinOp::Ge,
            rb::BinOp::And => BinOp::And,
            rb::BinOp::Or => BinOp::Or,
            rb::BinOp::Append => {
                self.error(
                    "E0602",
                    "`<<` adds to an array, so it's a statement, not a value",
                    span,
                );
                return expr(ExprKind::Nil, span);
            }
        };
        let l = self.expr(lhs);
        let r = self.expr(rhs);
        binary(op, l, r, span)
    }

    fn lambda(&mut self, block: &rb::Block) -> FuncBody {
        FuncBody {
            params: block.params.clone(),
            block: self.body(&block.body, Some(block.span), Tail::Return),
            span: block.span,
        }
    }

    /// `name(args)`: a `def`, a host function, or `format`.
    fn call(
        &mut self,
        callee: &Ident,
        args: &[rb::Arg],
        block: Option<&rb::Block>,
        span: Span,
    ) -> Expr {
        let n = callee.node.as_str();
        if let Some(b) = block {
            self.error(
                "E0521",
                format!(
                    "`{n}` doesn't take a block: only the loop methods (`each`, `times`, …) and \
                     the host's DSL keywords do"
                ),
                b.span,
            );
        }
        if self.scopes.locals.contains(&callee.span) {
            self.error(
                "E0607",
                format!(
                    "`{n}` is a local here, not a method; call the lambda it holds with \
                     `{n}.(…)` or `{n}.call(…)`"
                ),
                callee.span,
            );
            return expr(ExprKind::Nil, span);
        }
        let args = self.pos_args(args);
        if n == "format" && !self.defs.contains(n) && !self.cfg.host_functions.contains_key(n) {
            return builtin("string", "format", callee.span, args, span);
        }
        call(name(n, callee.span), args, span)
    }

    /// Call arguments, which are positional outside a DSL keyword.
    pub(super) fn pos_args(&mut self, args: &[rb::Arg]) -> Vec<Expr> {
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                rb::Arg::Pos(e) => out.push(self.expr(e)),
                rb::Arg::Label { name: label, .. } => self.error(
                    "E0523",
                    format!(
                        "named arguments (`{}:`) are only for the host's DSL keywords",
                        label.node
                    ),
                    label.span,
                ),
            }
        }
        out
    }

    /// Whether `e` names a host memory binding (not a local of the same name).
    pub(super) fn is_memory(&self, e: &rb::Expr) -> bool {
        matches!(&e.kind, rb::ExprKind::Name(n)
            if !self.scopes.locals.contains(&e.span) && self.cfg.memory.contains_key(n))
    }
}
