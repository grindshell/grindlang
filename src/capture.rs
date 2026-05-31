//! Free-variable / closure-capture analysis, shared by the tree-walking interpreter
//! ([`crate::interp`]) and the IR lowering ([`crate::ir`]).
//!
//! The single source of truth for "which symbols does a function body reference, and which of
//! those are upvalues captured from an enclosing scope". The IR VM and the JIT build a
//! closure's environment and read it back *by index*, so they must agree on the **order** of
//! its captured upvalues; the ordering rule therefore lives here once.
//!
//! Terminology, for a function literal `B` at nesting depth `d` (the outermost top-level
//! function is depth 0, a `function … end` directly inside it is depth 1, …):
//!
//! * **referenced symbols** — every in-function [`SymbolId`] mentioned anywhere in `B`,
//!   *including* through nested function literals (a nested closure's upvalues must flow out
//!   through `B`'s own capture). Only ids resolving to a `Local`/`Upvalue` binding count;
//!   top-level names, builtins, host fns, and memory are not captured.
//! * **upvalues** — the referenced symbols declared in a *strictly shallower* function
//!   (`func_depth < d`); these are what `B` captures from its enclosing scopes.
//! * **boxed locals** of a function `F` — `F`'s own params/locals that are referenced inside
//!   some nested literal of `F` (hence captured). They must be allocated as shared cells so a
//!   closure and the enclosing frame observe each other's writes.

use std::collections::HashSet;

use crate::ast::*;
use crate::resolve::{Binding, Resolution, SymbolId};

/// Every in-function [`SymbolId`] referenced anywhere in `body`, including inside nested
/// function literals (their free variables flow through this body's capture).
pub fn referenced_symbols(res: &Resolution, body: &FuncBody) -> HashSet<SymbolId> {
    let mut out = HashSet::new();
    collect_block_ids(res, &body.block, &mut out);
    out
}

/// The ordered upvalues a function literal `body` at nesting `depth` captures: referenced
/// symbols whose declaring function is strictly shallower than `depth`. Sorted ascending by
/// [`SymbolId`] so the env layout is deterministic and identical across backends.
pub fn upvalues(res: &Resolution, body: &FuncBody, depth: usize) -> Vec<SymbolId> {
    let mut v: Vec<SymbolId> = referenced_symbols(res, body)
        .into_iter()
        .filter(|&id| (res.symbols[id as usize].func_depth) < depth)
        .collect();
    v.sort_unstable();
    v
}

/// The set of `body`'s own params/locals (declared at `depth`) that are captured by some
/// nested function literal — i.e. those that must be boxed as shared cells.
pub fn boxed_locals(res: &Resolution, body: &FuncBody, depth: usize) -> HashSet<SymbolId> {
    let mut captured = HashSet::new();
    each_child_literal(&body.block, &mut |child| {
        for id in referenced_symbols(res, child) {
            if res.symbols[id as usize].func_depth == depth {
                captured.insert(id);
            }
        }
    });
    captured
}

/// Invoke `f` on every *immediate* nested function literal of `block` — function expressions
/// and `local function` bodies that are not themselves nested inside a deeper literal. Does
/// not recurse into the literals it reports (their own children are their concern).
pub fn each_child_literal<F: FnMut(&FuncBody)>(block: &Block, f: &mut F) {
    visit_block_literals(block, f);
}

fn visit_block_literals<F: FnMut(&FuncBody)>(block: &Block, f: &mut F) {
    for stat in &block.stats {
        visit_stat_literals(stat, f);
    }
    if let Some(ret) = &block.ret {
        for e in &ret.exprs {
            visit_expr_literals(e, f);
        }
    }
}

fn visit_stat_literals<F: FnMut(&FuncBody)>(stat: &Stat, f: &mut F) {
    match &stat.kind {
        StatKind::Empty | StatKind::Break => {}
        StatKind::Local { exprs, .. } => {
            for e in exprs {
                visit_expr_literals(e, f);
            }
        }
        // A `local function` is itself an immediate child literal; report it, don't descend.
        StatKind::LocalFunction { body, .. } => f(body),
        StatKind::Assign { targets, exprs } => {
            for t in targets {
                visit_expr_literals(t, f);
            }
            for e in exprs {
                visit_expr_literals(e, f);
            }
        }
        StatKind::Call(e) => visit_expr_literals(e, f),
        StatKind::Do(block) => visit_block_literals(block, f),
        StatKind::While { cond, body } => {
            visit_expr_literals(cond, f);
            visit_block_literals(body, f);
        }
        StatKind::If { arms, else_block } => {
            for (cond, block) in arms {
                visit_expr_literals(cond, f);
                visit_block_literals(block, f);
            }
            if let Some(block) = else_block {
                visit_block_literals(block, f);
            }
        }
        StatKind::NumericFor {
            start,
            end,
            step,
            body,
            ..
        } => {
            visit_expr_literals(start, f);
            visit_expr_literals(end, f);
            if let Some(step) = step {
                visit_expr_literals(step, f);
            }
            visit_block_literals(body, f);
        }
        StatKind::GenericFor { iter, body, .. } => {
            match iter {
                IterExpr::IPairs { arg, .. } | IterExpr::Pairs { arg, .. } => {
                    visit_expr_literals(arg, f)
                }
            }
            visit_block_literals(body, f);
        }
    }
}

fn visit_expr_literals<F: FnMut(&FuncBody)>(expr: &Expr, f: &mut F) {
    match &expr.kind {
        ExprKind::Nil
        | ExprKind::Bool(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::Name(_) => {}
        // An immediate function literal: report it, don't descend into it.
        ExprKind::Function(body) => f(body),
        ExprKind::Index { base, index } => {
            visit_expr_literals(base, f);
            visit_expr_literals(index, f);
        }
        ExprKind::Field { base, .. } => visit_expr_literals(base, f),
        ExprKind::Call { callee, args } => {
            visit_expr_literals(callee, f);
            for a in args {
                visit_expr_literals(a, f);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            visit_expr_literals(receiver, f);
            for a in args {
                visit_expr_literals(a, f);
            }
        }
        ExprKind::Table(fields) => {
            for field in fields {
                match field {
                    Field::Positional(e) => visit_expr_literals(e, f),
                    Field::Named { value, .. } => visit_expr_literals(value, f),
                    Field::Keyed { key, value } => {
                        visit_expr_literals(key, f);
                        visit_expr_literals(value, f);
                    }
                }
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            visit_expr_literals(lhs, f);
            visit_expr_literals(rhs, f);
        }
        ExprKind::Unary { operand, .. } => visit_expr_literals(operand, f),
        ExprKind::Paren(inner) => visit_expr_literals(inner, f),
    }
}

// ---- referenced-symbol collection -------------------------------------------
//
// Walk a body and gather every in-function [`SymbolId`] it references — its own
// locals/params and its upvalues, *including* those reached only through nested function
// literals (a nested closure's upvalues must flow through the enclosing closure's capture).

fn collect_block_ids(res: &Resolution, block: &Block, out: &mut HashSet<SymbolId>) {
    for stat in &block.stats {
        collect_stat_ids(res, stat, out);
    }
    if let Some(ret) = &block.ret {
        for e in &ret.exprs {
            collect_expr_ids(res, e, out);
        }
    }
}

fn collect_stat_ids(res: &Resolution, stat: &Stat, out: &mut HashSet<SymbolId>) {
    match &stat.kind {
        StatKind::Empty | StatKind::Break => {}
        StatKind::Local { exprs, .. } => {
            for e in exprs {
                collect_expr_ids(res, e, out);
            }
        }
        StatKind::LocalFunction { body, .. } => collect_block_ids(res, &body.block, out),
        StatKind::Assign { targets, exprs } => {
            for t in targets {
                collect_target_ids(res, t, out);
            }
            for e in exprs {
                collect_expr_ids(res, e, out);
            }
        }
        StatKind::Call(e) => collect_expr_ids(res, e, out),
        StatKind::Do(block) => collect_block_ids(res, block, out),
        StatKind::While { cond, body } => {
            collect_expr_ids(res, cond, out);
            collect_block_ids(res, body, out);
        }
        StatKind::If { arms, else_block } => {
            for (cond, block) in arms {
                collect_expr_ids(res, cond, out);
                collect_block_ids(res, block, out);
            }
            if let Some(block) = else_block {
                collect_block_ids(res, block, out);
            }
        }
        StatKind::NumericFor {
            start,
            end,
            step,
            body,
            ..
        } => {
            collect_expr_ids(res, start, out);
            collect_expr_ids(res, end, out);
            if let Some(step) = step {
                collect_expr_ids(res, step, out);
            }
            collect_block_ids(res, body, out);
        }
        StatKind::GenericFor { iter, body, .. } => {
            match iter {
                IterExpr::IPairs { arg, .. } | IterExpr::Pairs { arg, .. } => {
                    collect_expr_ids(res, arg, out)
                }
            }
            collect_block_ids(res, body, out);
        }
    }
}

/// An assignment target: a name write (whose symbol must be captured so the write reaches the
/// shared slot) or a field/index whose base is an ordinary expression.
fn collect_target_ids(res: &Resolution, expr: &Expr, out: &mut HashSet<SymbolId>) {
    match &expr.kind {
        ExprKind::Name(_) => collect_name_id(res, expr, out),
        ExprKind::Field { base, .. } => collect_expr_ids(res, base, out),
        ExprKind::Index { base, index } => {
            collect_expr_ids(res, base, out);
            collect_expr_ids(res, index, out);
        }
        _ => collect_expr_ids(res, expr, out),
    }
}

fn collect_expr_ids(res: &Resolution, expr: &Expr, out: &mut HashSet<SymbolId>) {
    match &expr.kind {
        ExprKind::Nil | ExprKind::Bool(_) | ExprKind::Number(_) | ExprKind::Str(_) => {}
        ExprKind::Name(_) => collect_name_id(res, expr, out),
        // Recurse into nested functions: their upvalues are this closure's responsibility to
        // carry, so they must be captured here too.
        ExprKind::Function(body) => collect_block_ids(res, &body.block, out),
        ExprKind::Index { base, index } => {
            collect_expr_ids(res, base, out);
            collect_expr_ids(res, index, out);
        }
        ExprKind::Field { base, .. } => collect_expr_ids(res, base, out),
        ExprKind::Call { callee, args } => {
            collect_expr_ids(res, callee, out);
            for a in args {
                collect_expr_ids(res, a, out);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            collect_expr_ids(res, receiver, out);
            for a in args {
                collect_expr_ids(res, a, out);
            }
        }
        ExprKind::Table(fields) => {
            for field in fields {
                match field {
                    Field::Positional(e) => collect_expr_ids(res, e, out),
                    Field::Named { value, .. } => collect_expr_ids(res, value, out),
                    Field::Keyed { key, value } => {
                        collect_expr_ids(res, key, out);
                        collect_expr_ids(res, value, out);
                    }
                }
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_ids(res, lhs, out);
            collect_expr_ids(res, rhs, out);
        }
        ExprKind::Unary { operand, .. } => collect_expr_ids(res, operand, out),
        ExprKind::Paren(inner) => collect_expr_ids(res, inner, out),
    }
}

/// Record the symbol id of a name use if it resolves to an in-function binding.
fn collect_name_id(res: &Resolution, expr: &Expr, out: &mut HashSet<SymbolId>) {
    if let Some(Binding::Local(id) | Binding::Upvalue(id)) = res.binding(expr.span) {
        out.insert(*id);
    }
}
