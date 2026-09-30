//! The core changes the Ruby surface needs (`PLAN.md` Phase 10, C8 and C9), held to all three
//! oracles. They are part of the core language, so these run without the `ruby` feature.

#![cfg(all(feature = "interp", feature = "jit"))]

mod common;

use common::Executors;
use grindlang::ast::{Block, ExprKind, Field, IterExpr, Module, Stat, StatKind, TopDecl};
use grindlang::{TypeConfig, Value};

fn num(n: f64) -> Value {
    Value::Number(n)
}

fn string(s: &str) -> Value {
    Value::string(s.to_string())
}

// ---- C8: narrowing on `== nil` ------------------------------------------------------------

#[test]
fn eq_nil_narrows_the_else_branch_on_every_backend() {
    // The narrowed reads unwrap `number?` to a native number in the JIT.
    let src = "function f(s, c)\n\
                 local x = tonumber(s)\n\
                 if x == nil then\n\
                   return -1\n\
                 elseif x > 10 then\n\
                   return x - 10\n\
                 elseif c then\n\
                   return x * 2\n\
                 else\n\
                   return x\n\
                 end\n\
               end\n\
               function g(s)\n\
                 local x = tonumber(s)\n\
                 if not (x == nil) then return x + 1 end\n\
                 return 0\n\
               end";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("f", vec![string("nope"), Value::Bool(true)], "-1");
    ex.assert_returns("f", vec![string("15"), Value::Bool(true)], "5");
    ex.assert_returns("f", vec![string("4"), Value::Bool(true)], "8");
    ex.assert_returns("f", vec![string("4"), Value::Bool(false)], "4");
    ex.assert_returns("g", vec![string("4")], "5");
    ex.assert_returns("g", vec![string("x")], "0");
}

#[test]
fn a_nil_initialized_local_narrows_in_elseif() {
    // SPEC.md §5.3's example, which SPEC-RUBY.md §7 writes as `if best.nil? … elsif`.
    let src = "---@param items { name: string, score: number }[]\n\
               function best_name(items)\n\
                 local best = nil\n\
                 for _, it in ipairs(items) do\n\
                   if best == nil then\n\
                     best = it\n\
                   elseif it.score > best.score then\n\
                     best = it\n\
                   end\n\
                 end\n\
                 if best ~= nil then return best.name end\n\
                 return \"none\"\n\
               end\n\
               function run()\n\
                 return best_name({ { name = \"a\", score = 1 }, { name = \"b\", score = 3 },\n\
                                    { name = \"c\", score = 2 } })\n\
               end\n\
               function empty()\n\
                 return best_name({})\n\
               end";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("run", vec![], "b");
    ex.assert_returns("empty", vec![], "none");
}

// ---- C9: zero-based indexing --------------------------------------------------------------

/// Parse Lua `src` and mark every index zero-based, as the Ruby translator does.
fn zero_based(src: &str) -> Module {
    let mut module =
        grindlang::parse(src).unwrap_or_else(|d| panic!("parse failed:\n{}", d.render(src)));
    for decl in &mut module.decls {
        match decl {
            TopDecl::Function(f) => mark_block(&mut f.body.block),
            TopDecl::Const(c) => mark_expr(&mut c.value),
            TopDecl::Stat(s) => mark_stat(s),
        }
    }
    module
}

fn mark_block(block: &mut Block) {
    block.stats.iter_mut().for_each(mark_stat);
    if let Some(ret) = &mut block.ret {
        ret.exprs.iter_mut().for_each(mark_expr);
    }
}

fn mark_stat(stat: &mut Stat) {
    match &mut stat.kind {
        StatKind::Local { exprs, .. } => exprs.iter_mut().for_each(mark_expr),
        StatKind::LocalFunction { body, .. } => mark_block(&mut body.block),
        StatKind::Assign { targets, exprs } => targets.iter_mut().chain(exprs).for_each(mark_expr),
        StatKind::Call(e) => mark_expr(e),
        StatKind::Do(b) => mark_block(b),
        StatKind::While { cond, body } => {
            mark_expr(cond);
            mark_block(body);
        }
        StatKind::If { arms, else_block } => {
            for (cond, body) in arms {
                mark_expr(cond);
                mark_block(body);
            }
            if let Some(b) = else_block {
                mark_block(b);
            }
        }
        StatKind::NumericFor {
            start,
            end,
            step,
            body,
            ..
        } => {
            mark_expr(start);
            mark_expr(end);
            if let Some(step) = step {
                mark_expr(step);
            }
            mark_block(body);
        }
        StatKind::GenericFor { iter, body, .. } => {
            match iter {
                IterExpr::IPairs { arg, .. } | IterExpr::Pairs { arg, .. } => mark_expr(arg),
            }
            mark_block(body);
        }
        _ => {}
    }
}

fn mark_expr(expr: &mut grindlang::ast::Expr) {
    match &mut expr.kind {
        ExprKind::Index {
            base,
            index,
            zero_based,
        } => {
            *zero_based = true;
            mark_expr(base);
            mark_expr(index);
        }
        ExprKind::Function(body) => mark_block(&mut body.block),
        ExprKind::Field { base, .. } => mark_expr(base),
        ExprKind::Call { callee, args } => {
            mark_expr(callee);
            args.iter_mut().for_each(mark_expr);
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            mark_expr(receiver);
            args.iter_mut().for_each(mark_expr);
        }
        ExprKind::Table(fields) => {
            for field in fields {
                match field {
                    Field::Positional(e) | Field::Named { value: e, .. } => mark_expr(e),
                    Field::Keyed { key, value } => {
                        mark_expr(key);
                        mark_expr(value);
                    }
                }
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            mark_expr(lhs);
            mark_expr(rhs);
        }
        ExprKind::Unary { operand, .. } | ExprKind::Paren(operand) => mark_expr(operand),
        _ => {}
    }
}

fn zero_based_executors(src: &str) -> Executors {
    Executors::from_module(zero_based(src), &TypeConfig::default(), src)
}

#[test]
fn zero_based_array_reads_and_writes() {
    let src = "function first(xs)\n\
                 local v = xs[0]\n\
                 if v ~= nil then return v end\n\
                 return -1\n\
               end\n\
               function sum(xs)\n\
                 local n, i = 0, 0\n\
                 while i < #xs do\n\
                   local v = xs[i]\n\
                   if v ~= nil then n = n + v end\n\
                   i = i + 1\n\
                 end\n\
                 return n\n\
               end\n\
               function build()\n\
                 local xs = { 10, 20 }\n\
                 xs[0] = 11\n\
                 xs[#xs] = 30\n\
                 return xs\n\
               end\n\
               function past_the_end()\n\
                 local xs = { 10, 20 }\n\
                 return xs[2] == nil and xs[-1] == nil\n\
               end\n\
               function bad_write()\n\
                 local xs = { 10, 20 }\n\
                 xs[5] = 1\n\
               end";
    let mut ex = zero_based_executors(src);
    let xs = || Value::array(vec![num(7.0), num(8.0), num(9.0)]);
    ex.assert_returns("first", vec![xs()], "7");
    ex.assert_returns("sum", vec![xs()], "24");
    ex.assert_returns("build", vec![], "[11, 20, 30]");
    ex.assert_returns("past_the_end", vec![], "true");
    // The runtime reports the 1-based position (SPEC-RUBY.md, open items).
    ex.assert_fails("bad_write", vec![], "6");
}

#[test]
fn zero_based_leaves_map_and_record_keys_alone() {
    let src = "function f()\n\
                 local m = { [\"b\"] = 2 }\n\
                 m[\"a\"] = 1\n\
                 local r = { name = \"x\", score = 3 }\n\
                 local v = m[\"a\"]\n\
                 if v ~= nil then return r[\"score\"] + v end\n\
                 return 0\n\
               end";
    zero_based_executors(src).assert_returns("f", vec![], "4");
}

#[test]
fn zero_based_index_of_a_value_typed_later() {
    // The base's type is unknown where the index is checked; lowering reads it once inferred.
    let src = "function f()\n\
                 local xs = nil\n\
                 xs = { 5, 6 }\n\
                 local ys = xs\n\
                 if ys ~= nil then\n\
                   local v = ys[1]\n\
                   if v ~= nil then return v end\n\
                 end\n\
                 return 0\n\
               end";
    zero_based_executors(src).assert_returns("f", vec![], "6");
}
