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
//! - a `case` subject, read again for each `when`, takes that `when` keyword's span, and the
//!   temporary one is evaluated into, that `case` keyword's;
//! - a hoisted `local x`, or one declared before an assignment that moves into the branches of
//!   an `if`, takes the span of `x` in its first assignment, which is a use of the same local;
//! - a collection method's temporary and loop variables are placed as `collect.rs` says.
//!
//! Only names are looked up by span, so a call may share the span of the name it calls, and a
//! builtin's `math.abs` the span of its `math`. The one other node keyed by span is the
//! conditional expression, whose type the checker records as it does an `and`/`or`'s: an `if` or
//! `case` used as a value becomes one per arm, and each arm after the first takes the span from
//! its condition, or its `when`, to the end of the whole `if` or `case`.
//!
//! ## `if`, `case`, and `?:`
//!
//! Where one of these is decides what it becomes (`SPEC-RUBY.md` §4.4): an `if` statement whose
//! branches each return, assign, or run their last expression ([`Tail`]), or a conditional
//! expression, which needs each branch to be one expression.

use std::collections::HashSet;

use super::RubyConfig;
use super::ast as rb;
use super::dsl::Dsl;
use super::scope::{self, Scopes};
use crate::ast::{
    self, BinOp, Block, ConstDecl, Expr, ExprKind, Fallthrough, Field, FuncBody, FuncDecl, Ident,
    IterExpr, RetStat, Spanned, Stat, StatKind, TopDecl, UnOp,
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
        temps: 0,
        chain: Vec::new(),
        loops: Vec::new(),
        collecting: None,
        dsl: Dsl::new(program.span.end),
    };
    let module = t.program(program);
    if t.diags.has_errors() {
        Err(t.diags)
    } else {
        Ok(module)
    }
}

/// What a body's last statement does with its value (`SPEC-RUBY.md` §4.2, §4.4).
#[derive(Clone, Copy)]
pub(super) enum Tail<'t> {
    /// Nothing: not the last statement of a `def` or lambda body, or not in one at all.
    None,
    /// An expression here becomes `return expr`.
    Return,
    /// An expression here is assigned to this target: the last statement of a branch of an `if`,
    /// `case`, or `?:` that an assignment takes the value of, when its branches hold statements.
    Assign(&'t Expr),
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
    /// How many numbered temporaries have been made ([`Self::temp`]).
    temps: u32,
    /// The collection method calls starting the method chain of the value being translated, by
    /// span, which may become loops ([`Self::value`]).
    pub(super) chain: Vec<Span>,
    /// The loops those calls have become so far, which go before the statement holding them.
    pub(super) loops: Vec<Stat>,
    /// The collection method whose block is the innermost loop being translated, if it is one:
    /// `next` and `break` directly in it are `E0611`.
    pub(super) collecting: Option<&'static str>,
    /// The DSL keywords' builders, tables, and ids so far (`dsl.rs`).
    pub(super) dsl: Dsl,
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

/// A conditional expression (C2): `then` if `cond` holds, else `els`.
fn cond_expr(cond: Expr, then: Expr, els: Expr, span: Span) -> Expr {
    expr(
        ExprKind::Cond {
            cond: Box::new(cond),
            then: Box::new(then),
            els: Box::new(els),
        },
        span,
    )
}

pub(super) fn stat(kind: StatKind, span: Span) -> Stat {
    Stat { kind, span }
}

/// `target = nil`: what a branch that Ruby gives no value assigns (`SPEC-RUBY.md` §4.4).
fn nil_block(target: &Expr, span: Span) -> Block {
    let assign = StatKind::Assign {
        targets: vec![target.clone()],
        exprs: vec![expr(ExprKind::Nil, span)],
    };
    Block {
        stats: vec![stat(assign, span)],
        ret: None,
        span,
    }
}

/// Conditional expressions testing each arm's condition in turn: the first arm's takes `span`,
/// the whole `if` or `case`, and each later one the span from its own condition or `when` to the
/// end of it, which no other node has. `els` is the value when no condition holds.
fn chain(arms: Vec<(Expr, Expr, Span)>, els: Expr, span: Span) -> Expr {
    let mut acc = els;
    for (i, (cond, then, at)) in arms.into_iter().enumerate().rev() {
        let at = if i == 0 { span } else { at.to(span) };
        acc = cond_expr(cond, then, acc, at);
    }
    acc
}

/// The `if`, `unless`, `case`, or `?:` that `e` is, seen through parentheses: the forms that
/// choose between values.
fn choice(e: &rb::Expr) -> Option<&rb::Expr> {
    match &e.kind {
        rb::ExprKind::If(_) | rb::ExprKind::Case(_) | rb::ExprKind::Ternary { .. } => Some(e),
        rb::ExprKind::Paren(inner) => choice(inner),
        _ => None,
    }
}

/// The jump `s` is, if it is one: `return`, `break`, or `next`.
pub(super) fn jump(s: &rb::Stmt) -> Option<&'static str> {
    match s.kind {
        rb::StmtKind::Return(_) => Some("return"),
        rb::StmtKind::Break => Some("break"),
        rb::StmtKind::Next => Some("next"),
        _ => None,
    }
}

/// `local a, b` with no values.
pub(super) fn declare(names: Vec<Ident>) -> Stat {
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
pub(super) fn negate(e: Expr, span: Span) -> Expr {
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

    pub(super) fn error_count(&self) -> usize {
        self.diags.iter().count()
    }

    /// A new temporary's name, `what#N`, which no Ruby name can be.
    pub(super) fn temp(&mut self, what: &str) -> String {
        self.temps += 1;
        format!("{what}#{}", self.temps)
    }

    /// Translate the body of a loop or lambda with `f`, where `collecting` is the collection
    /// method whose block it is, if it is one.
    pub(super) fn within<T>(
        &mut self,
        collecting: Option<&'static str>,
        f: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = std::mem::replace(&mut self.collecting, collecting);
        let result = f(self);
        self.collecting = outer;
        result
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
                    if let Some(msg) = self.keyword_claims(&d.name.node) {
                        self.error("E0612", msg, d.name.span);
                    }
                    if d.visibility.unwrap_or(visibility) == rb::Visibility::Public {
                        exports.push(d.name.clone());
                    } else {
                        private = true;
                    }
                    decls.push(TopDecl::Function(self.def(d)));
                }
                rb::Item::Visibility(v) => visibility = v.node,
                rb::Item::Stmt(s) => {
                    if !self.top_keyword(s, &mut decls, &mut exports) {
                        self.top_stmt(s, &mut decls, &mut exports);
                    }
                }
            }
        }
        self.finish_keywords(&mut decls, &mut exports);
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
            if let Some(msg) = self.keyword_claims(c) {
                self.error("E0612", msg, target.span);
                return;
            }
            if values.len() != 1 {
                self.error(
                    "E0604",
                    format!("`{c}` is a constant, which takes one value"),
                    s.span,
                );
                return;
            }
            if let Some(choice) = choice(&values[0])
                && !self.cond_ok(choice, true)
            {
                self.error(
                    "E0609",
                    format!(
                        "`{c}` is a constant, which takes one expression, so each branch of the \
                         `if` or `case` giving its value must be one expression"
                    ),
                    choice.span,
                );
                return;
            }
            let mut before = Vec::new();
            let value = self.rhs(&values[0], &mut before);
            decls.extend(before.into_iter().map(|st| TopDecl::Stat(Box::new(st))));
            let name = ident(c, target.span);
            decls.push(TopDecl::Const(ConstDecl {
                name: name.clone(),
                value,
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
                fallthrough: Fallthrough::Nil,
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
        // An empty branch is `nil`, as in Ruby.
        if body.stmts.is_empty()
            && let Tail::Assign(target) = tail
        {
            stats.extend(nil_block(target, body.span).stats);
        }
        for (i, s) in body.stmts.iter().enumerate() {
            let last = i + 1 == body.stmts.len();
            self.stmt(
                s,
                if last { tail } else { Tail::None },
                &mut stats,
                &mut ret,
            );
            let Some(jump) = jump(s) else { continue };
            if !last {
                self.error(
                    "E0606",
                    format!("this code never runs: it comes after a `{jump}`"),
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

    pub(super) fn stmt(
        &mut self,
        s: &rb::Stmt,
        tail: Tail,
        out: &mut Vec<Stat>,
        ret: &mut Option<RetStat>,
    ) {
        let span = s.span;
        // A branch whose value is assigned ends in an expression, a jump, or `stmt if c`.
        if let Tail::Assign(_) = tail
            && !matches!(
                s.kind,
                rb::StmtKind::Expr(_)
                    | rb::StmtKind::Return(_)
                    | rb::StmtKind::Break
                    | rb::StmtKind::Next
                    | rb::StmtKind::Modified {
                        modifier: rb::Modifier::If | rb::Modifier::Unless,
                        ..
                    }
            )
        {
            self.error(
                "E0609",
                "this branch gives the `if` or `case` no value to assign: its last statement must \
                 be an expression",
                span,
            );
        }
        match &s.kind {
            rb::StmtKind::Expr(e) => self.expr_stmt(e, span, tail, out, ret),
            rb::StmtKind::Assign { targets, values } => self.assign(targets, values, span, out),
            rb::StmtKind::OpAssign { target, op, value } => {
                self.op_assign(target, *op, value, span, out)
            }
            rb::StmtKind::While { cond, body, until } => {
                let c = self.expr(cond);
                let cond = if *until { negate(c, cond.span) } else { c };
                let body = self.within(None, |t| t.body(body, None, Tail::None));
                out.push(stat(StatKind::While { cond, body }, span));
            }
            rb::StmtKind::Loop(body) => {
                let cond = expr(ExprKind::Bool(true), keyword(span, 4));
                let body = self.within(None, |t| t.body(body, None, Tail::None));
                out.push(stat(StatKind::While { cond, body }, span));
            }
            rb::StmtKind::Break | rb::StmtKind::Next => {
                let word = jump(s).expect("a jump");
                if let Some(m) = self.collecting {
                    let msg = if word == "next" {
                        format!(
                            "`next` can't end a pass of `{m}`'s block: Ruby would give the block \
                             the value `nil`. Give it its value with `if` or `?:` instead"
                        )
                    } else {
                        format!(
                            "`break` can't leave `{m}`'s block: Ruby would make `{m}` give `nil`. \
                             To stop early, loop with `each` and assign to a local"
                        )
                    };
                    self.error("E0611", msg, span);
                }
                let kind = if word == "next" {
                    StatKind::Continue
                } else {
                    StatKind::Break
                };
                out.push(stat(kind, span));
            }
            rb::StmtKind::Return(values) => {
                // `return` of a choice returns from each branch, and returns nothing after one
                // that gives no value, as a bare `return` does (§4.4).
                if let [value] = values.as_slice()
                    && let Some(choice) = choice(value)
                {
                    let mut stats = Vec::new();
                    self.choice_stmt(choice, Tail::Return, &mut stats);
                    let block = Block {
                        stats,
                        ret: None,
                        span,
                    };
                    if !crate::types::always_returns(&block) {
                        *ret = Some(RetStat {
                            exprs: Vec::new(),
                            span,
                        });
                    }
                    out.extend(block.stats);
                    return;
                }
                let exprs = match values.as_slice() {
                    [value] => vec![self.value(value, out)],
                    _ => values.iter().map(|v| self.expr(v)).collect(),
                };
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
                        // A branch ending in `y if c` gives `nil` where `c` doesn't hold.
                        let else_block = match tail {
                            Tail::Assign(target) => Some(nil_block(target, span)),
                            _ => None,
                        };
                        out.push(stat(
                            StatKind::If {
                                arms: vec![(cond, block)],
                                else_block,
                            },
                            span,
                        ));
                    }
                    rb::Modifier::While | rb::Modifier::Until => {
                        let body = self.within(None, |t| t.single(inner, Tail::None));
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
        if let Some(choice) = choice(e) {
            return self.choice_stmt(choice, tail, out);
        }
        if let Tail::Assign(target) = tail {
            let v = self.value(e, out);
            let assign = StatKind::Assign {
                targets: vec![target.clone()],
                exprs: vec![v],
            };
            out.push(stat(assign, span));
            return;
        }
        match &e.kind {
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
            rb::ExprKind::Field { recv, name }
                if name.node == "freeze" && matches!(tail, Tail::None) =>
            {
                self.expr(recv);
                return;
            }
            _ => {}
        }
        if matches!(tail, Tail::Return) {
            let v = self.value(e, out);
            *ret = Some(RetStat {
                exprs: vec![v],
                span,
            });
            return;
        }
        let before = self.error_count();
        let v = self.expr(e);
        if matches!(v.kind, ExprKind::Call { .. } | ExprKind::MethodCall { .. }) {
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

    /// An `if`, `unless`, `case`, or `?:` as a statement, whose branches each take `tail`: at the
    /// end of a body, each returns its value; as the whole right side of an assignment whose
    /// branches hold statements, each assigns it (`SPEC-RUBY.md` §4.4).
    fn choice_stmt(&mut self, choice: &rb::Expr, tail: Tail, out: &mut Vec<Stat>) {
        let span = choice.span;
        match &choice.kind {
            rb::ExprKind::If(if_) => self.if_stmt(if_, span, tail, out),
            rb::ExprKind::Case(case) => self.case_stmt(case, span, tail, out),
            rb::ExprKind::Ternary { cond, then, els } => {
                let cond = self.expr(cond);
                let then = self.branch(then, tail);
                let els = self.branch(els, tail);
                out.push(stat(
                    StatKind::If {
                        arms: vec![(cond, then)],
                        else_block: Some(els),
                    },
                    span,
                ));
            }
            _ => unreachable!("`choice` finds only these"),
        }
    }

    /// A branch of `?:` as a block of its own.
    fn branch(&mut self, e: &rb::Expr, tail: Tail) -> Block {
        let mut stats = Vec::new();
        let mut ret = None;
        self.expr_stmt(e, e.span, tail, &mut stats, &mut ret);
        Block {
            stats,
            ret,
            span: e.span,
        }
    }

    /// The `else` of an `if` or `case` statement, which Ruby gives the value `nil` when it is
    /// left out.
    fn else_block(&mut self, body: Option<&rb::Body>, span: Span, tail: Tail) -> Option<Block> {
        match (body, tail) {
            (Some(body), _) => Some(self.body(body, None, tail)),
            (None, Tail::Assign(target)) => Some(nil_block(target, span)),
            (None, _) => None,
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
        let else_block = self.else_block(if_.else_body.as_ref(), span, tail);
        out.push(stat(StatKind::If { arms, else_block }, span));
    }

    /// The name a `case` reads its subject by again for each `when`: the subject's own, if it is
    /// a local or a constant, which the tests can't change.
    fn reread(&self, subject: &rb::Expr) -> Option<String> {
        match &subject.kind {
            rb::ExprKind::Name(n) if self.scopes.locals.contains(&subject.span) => Some(n.clone()),
            rb::ExprKind::Const(n) => Some(n.clone()),
            _ => None,
        }
    }

    /// What a `when` tests of the subject read as `reread` at `at`: `x == a or x == b`, and
    /// `x >= lo and x <= hi` for a range. `None` if the `when` is too long to translate.
    fn when_test(&mut self, when: &rb::When, reread: &str, at: Span) -> Option<Expr> {
        if when.values.len() > MAX_CHAIN {
            self.diags.push(crate::parser::too_deeply_nested(when.span));
            return None;
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
        cond
    }

    /// `case x when a, b … when lo..hi … else … end`: an `if` chain testing `x == a or x == b`,
    /// then `x >= lo and x <= hi` (`SPEC-RUBY.md` §4.4). A local or constant subject is read
    /// again for each `when`; any other is evaluated once, into a temporary.
    fn case_stmt(&mut self, case: &rb::Case, span: Span, tail: Tail, out: &mut Vec<Stat>) {
        let direct = self.reread(&case.subject);
        // The subject is evaluated first, so it may be a collection method (`collect.rs`).
        let mut stats = Vec::new();
        let subject = self.value(&case.subject, &mut stats);
        let reread = direct.as_deref().unwrap_or(CASE_SUBJECT);
        let mut arms = Vec::new();
        for when in &case.whens {
            let at = keyword(when.span, 4);
            let Some(cond) = self.when_test(when, reread, at) else {
                continue;
            };
            arms.push((cond, self.body(&when.body, None, tail)));
        }
        let else_block = self.else_block(case.else_body.as_ref(), span, tail);
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
        stats.extend([temp, chain]);
        let block = Block {
            stats,
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
        if let ([target], [value]) = (targets, values)
            && let Some(choice) = choice(value)
            && !self.cond_ok(choice, true)
        {
            return self.assign_by_branch(target, choice, out);
        }
        // The scope analysis declares every target of an assignment in place, or none.
        let declare = targets.iter().all(|t| {
            matches!(t.kind, rb::ExprKind::Name(_)) && self.scopes.in_place.contains(&t.span)
        });
        let exprs: Vec<Expr> = match values {
            [value] => vec![self.rhs(value, out)],
            _ => values.iter().map(|v| self.expr(v)).collect(),
        };
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

    /// The whole right side of an assignment, or a block's value. A `case` there whose subject
    /// it can't read again evaluates the subject once, into a temporary declared just before
    /// the assignment: the right side is evaluated before the target anyway.
    pub(super) fn rhs(&mut self, value: &rb::Expr, out: &mut Vec<Stat>) -> Expr {
        if let Some(choice) = choice(value)
            && let rb::ExprKind::Case(case) = &choice.kind
            && self.reread(&case.subject).is_none()
        {
            let temp = self.temp("case");
            let subject = self.value(&case.subject, out);
            out.push(stat(
                StatKind::Local {
                    names: vec![ident(&temp, keyword(choice.span, 4))],
                    exprs: vec![subject],
                },
                case.subject.span,
            ));
            return self.case_value(case, choice.span, Some(&temp));
        }
        self.value(value, out)
    }

    /// `x = if … end` whose branches hold statements: the assignment moves into each branch,
    /// which assigns its last expression (`SPEC-RUBY.md` §4.4). A local this declares is
    /// declared first, with no value, so it is optional, as a hoisted local is (§4.3).
    fn assign_by_branch(&mut self, target: &rb::Expr, choice: &rb::Expr, out: &mut Vec<Stat>) {
        if let rb::ExprKind::Name(n) = &target.kind
            && self.scopes.in_place.contains(&target.span)
        {
            out.push(declare(vec![ident(n, target.span)]));
        }
        let target = self.target(target);
        self.choice_stmt(choice, Tail::Assign(&target), out);
    }

    /// Whether the `if`, `unless`, `case`, or `?:` `choice` can be a conditional expression:
    /// every branch, and every branch of a choice that is one, a single expression or none that
    /// needs no loop before it (`collect.rs`), and every `case` subject one it can read again —
    /// except the outermost's, when `outer`, which the whole right side of an assignment
    /// evaluates into a temporary ([`Self::rhs`]).
    fn cond_ok(&self, c: &rb::Expr, outer: bool) -> bool {
        let branch =
            |e: &rb::Expr| !self.collects(e) && choice(e).is_none_or(|c| self.cond_ok(c, false));
        let body = |b: &rb::Body| match b.stmts.as_slice() {
            [] => true,
            [
                rb::Stmt {
                    kind: rb::StmtKind::Expr(e),
                    ..
                },
            ] => branch(e),
            _ => false,
        };
        match &c.kind {
            rb::ExprKind::Ternary { then, els, .. } => branch(then) && branch(els),
            rb::ExprKind::If(if_) => {
                if_.arms.iter().all(|(_, b)| body(b)) && if_.else_body.as_ref().is_none_or(body)
            }
            rb::ExprKind::Case(case) => {
                (outer || self.reread(&case.subject).is_some())
                    && case.whens.iter().all(|w| body(&w.body))
                    && case.else_body.as_ref().is_none_or(body)
            }
            _ => true,
        }
    }

    /// An `if` or `unless` used as a value: a conditional expression per arm, the last one's
    /// `else` the `else` branch, or `nil` without one (`SPEC-RUBY.md` §4.4).
    fn if_value(&mut self, if_: &rb::If, span: Span) -> Expr {
        if if_.arms.len() > MAX_CHAIN {
            self.diags.push(crate::parser::too_deeply_nested(span));
            return expr(ExprKind::Nil, span);
        }
        let mut arms = Vec::new();
        for (c, body) in &if_.arms {
            let cond = self.expr(c);
            let cond = if if_.unless {
                negate(cond, c.span)
            } else {
                cond
            };
            arms.push((cond, self.body_value(body), c.span));
        }
        let els = match &if_.else_body {
            Some(body) => self.body_value(body),
            None => expr(ExprKind::Nil, span),
        };
        chain(arms, els, span)
    }

    /// A `case` used as a value: a conditional expression per `when`. The subject is read again
    /// for each, by its own name or, when `temp` names one, from that temporary.
    fn case_value(&mut self, case: &rb::Case, span: Span, temp: Option<&str>) -> Expr {
        let reread = match temp
            .map(str::to_string)
            .or_else(|| self.reread(&case.subject))
        {
            Some(reread) => reread,
            None => {
                self.error(
                    "E0609",
                    "a `case` used as a value here reads its subject again for each `when`, so the \
                     subject must be a local or a constant; assign it to a local first, or make \
                     the `case` the whole right side of an assignment or a `return`",
                    case.subject.span,
                );
                return expr(ExprKind::Nil, span);
            }
        };
        if case.whens.len() > MAX_CHAIN {
            self.diags.push(crate::parser::too_deeply_nested(span));
            return expr(ExprKind::Nil, span);
        }
        let mut arms = Vec::new();
        for when in &case.whens {
            let at = keyword(when.span, 4);
            let Some(cond) = self.when_test(when, &reread, at) else {
                continue;
            };
            arms.push((cond, self.body_value(&when.body), when.span));
        }
        let els = match &case.else_body {
            Some(body) => self.body_value(body),
            None => expr(ExprKind::Nil, span),
        };
        chain(arms, els, span)
    }

    /// The value of a branch of an `if` or `case` used as a value, which is one expression, or
    /// `nil` for an empty branch.
    fn body_value(&mut self, body: &rb::Body) -> Expr {
        match body.stmts.as_slice() {
            [] => expr(ExprKind::Nil, body.span),
            [
                rb::Stmt {
                    kind: rb::StmtKind::Expr(e),
                    ..
                },
            ] => self.expr(e),
            [first, ..] => {
                self.error(
                    "E0609",
                    "each branch of an `if` or `case` used as a value here must be one \
                     expression; its branches may hold statements only where it is the whole \
                     right side of an assignment or a `return`",
                    first.span,
                );
                expr(ExprKind::Nil, body.span)
            }
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
        let range = m == "each" && self.is_range(recv);
        let counting = range || matches!(m, "times" | "upto" | "step");
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
            let (start, end, step) = if range {
                let (start, end) = self.range(recv).expect("a range");
                (start, end, None)
            } else if m == "times" {
                let n = self.value(recv, out);
                let end = binary(BinOp::Sub, n, number(1.0, method.span), recv.span);
                (number(0.0, method.span), end, None)
            } else {
                let start = self.value(recv, out);
                let mut args = self.pos_args(args).into_iter();
                let end = args.next().unwrap_or_else(|| number(0.0, method.span));
                (start, end, args.next().map(Box::new))
            };
            let body = self.within(None, |t| t.body(&block.body, Some(block.span), Tail::None));
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

        let arg = self.value(recv, out);
        let mut body = self.within(None, |t| t.body(&block.body, Some(block.span), Tail::None));
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

    /// Whether `recv` is a range, `(a..b)`, which a loop counts through.
    fn is_range(&self, recv: &rb::Expr) -> bool {
        match &recv.kind {
            rb::ExprKind::Range { .. } => true,
            rb::ExprKind::Paren(inner) => self.is_range(inner),
            _ => false,
        }
    }

    /// The first and last number of the range `recv` is, if it is one: `a..b` counts to `b`,
    /// and `a...b` to `b - 1`.
    pub(super) fn range(&mut self, recv: &rb::Expr) -> Option<(Expr, Expr)> {
        match &recv.kind {
            rb::ExprKind::Range { lo, hi, exclusive } => {
                let (start, end) = (self.expr(lo), self.expr(hi));
                let end = if *exclusive {
                    binary(BinOp::Sub, end, number(1.0, hi.span), hi.span)
                } else {
                    end
                };
                Some((start, end))
            }
            rb::ExprKind::Paren(inner) => self.range(inner),
            _ => None,
        }
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
            // A collection method becoming a temporary (`collect.rs`) reads as one.
            rb::ExprKind::MethodCall { .. } => self.in_chain(e),
            rb::ExprKind::Field { recv, name } => {
                self.in_chain(e) || (!super::methods::is_method(&name.node) && self.pure(recv))
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
            rb::ExprKind::Ternary { cond, then, els } => {
                let cond = self.expr(cond);
                let then = self.expr(then);
                let els = self.expr(els);
                return cond_expr(cond, then, els, span);
            }
            rb::ExprKind::If(if_) => return self.if_value(if_, span),
            rb::ExprKind::Case(case) => return self.case_value(case, span, None),
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
        if let Some(msg) = self.misplaced_keyword(n) {
            self.error("E0612", msg, span);
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
            block: self.within(None, |t| {
                t.body(&block.body, Some(block.span), Tail::Return)
            }),
            span: block.span,
            fallthrough: Fallthrough::Nil,
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
        if !self.scopes.locals.contains(&callee.span)
            && let Some(msg) = self.misplaced_keyword(n)
        {
            self.error("E0612", msg, callee.span);
            return expr(ExprKind::Nil, span);
        }
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
