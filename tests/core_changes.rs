//! The core changes the Ruby surface needs (`PLAN.md` Phase 10, C2–C4 and C8–C10), held to all
//! three oracles. They are part of the core language, so these run without the `ruby` feature.

#![cfg(all(feature = "interp", feature = "jit"))]

mod common;

use common::Executors;
use grindlang::ast::{
    Block, Expr, ExprKind, Fallthrough, Field, FuncBody, IterExpr, Module, Stat, StatKind, TopDecl,
};
use grindlang::{Diagnostics, FnType, Type, TypeConfig, Value};

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

// ---- rewriting Lua into what only another surface writes ----------------------------------

/// Parse Lua `src`, then apply `on_expr` to every expression, innermost first, and `on_body` to
/// every function body: how these tests reach the tree the Ruby translator builds, which Lua
/// can't spell.
fn rewrite(src: &str, on_expr: fn(&mut Expr), on_body: fn(&mut FuncBody)) -> Module {
    let mut module =
        grindlang::parse(src).unwrap_or_else(|d| panic!("parse failed:\n{}", d.render(src)));
    let walk = Walk { on_expr, on_body };
    for decl in &mut module.decls {
        match decl {
            TopDecl::Function(f) => walk.body(&mut f.body),
            TopDecl::Const(c) => walk.expr(&mut c.value),
            TopDecl::Stat(s) => walk.stat(s),
        }
    }
    module
}

struct Walk {
    on_expr: fn(&mut Expr),
    on_body: fn(&mut FuncBody),
}

impl Walk {
    fn body(&self, body: &mut FuncBody) {
        (self.on_body)(body);
        self.block(&mut body.block);
    }

    fn block(&self, block: &mut Block) {
        block.stats.iter_mut().for_each(|s| self.stat(s));
        if let Some(ret) = &mut block.ret {
            ret.exprs.iter_mut().for_each(|e| self.expr(e));
        }
    }

    fn stat(&self, stat: &mut Stat) {
        match &mut stat.kind {
            StatKind::Local { exprs, .. } => exprs.iter_mut().for_each(|e| self.expr(e)),
            StatKind::LocalFunction { body, .. } => self.body(body),
            StatKind::Assign { targets, exprs } => {
                targets.iter_mut().chain(exprs).for_each(|e| self.expr(e))
            }
            StatKind::Call(e) => self.expr(e),
            StatKind::Do(b) => self.block(b),
            StatKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            StatKind::If { arms, else_block } => {
                for (cond, body) in arms {
                    self.expr(cond);
                    self.block(body);
                }
                if let Some(b) = else_block {
                    self.block(b);
                }
            }
            StatKind::NumericFor {
                start,
                end,
                step,
                body,
                ..
            } => {
                self.expr(start);
                self.expr(end);
                if let Some(step) = step {
                    self.expr(step);
                }
                self.block(body);
            }
            StatKind::GenericFor { iter, body, .. } => {
                match iter {
                    IterExpr::IPairs { arg, .. } | IterExpr::Pairs { arg, .. } => self.expr(arg),
                }
                self.block(body);
            }
            StatKind::Empty | StatKind::Break | StatKind::Continue => {}
        }
    }

    fn expr(&self, expr: &mut Expr) {
        match &mut expr.kind {
            ExprKind::Index { base, index, .. } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Function(body) => self.body(body),
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call { callee, args } => {
                self.expr(callee);
                args.iter_mut().for_each(|e| self.expr(e));
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.expr(receiver);
                args.iter_mut().for_each(|e| self.expr(e));
            }
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        Field::Positional(e) | Field::Named { value: e, .. } => self.expr(e),
                        Field::Keyed { key, value } => {
                            self.expr(key);
                            self.expr(value);
                        }
                    }
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { operand, .. } | ExprKind::Paren(operand) => self.expr(operand),
            ExprKind::Cond { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            ExprKind::Nil
            | ExprKind::Bool(_)
            | ExprKind::Number(_)
            | ExprKind::Str(_)
            | ExprKind::Name(_) => {}
        }
        (self.on_expr)(expr);
    }
}

fn no_expr(_: &mut Expr) {}

fn no_body(_: &mut FuncBody) {}

// ---- C9: zero-based indexing --------------------------------------------------------------

/// Lua `src` with every index marked zero-based, as the Ruby translator marks them.
fn zero_based(src: &str) -> Module {
    let mark = |e: &mut Expr| {
        if let ExprKind::Index { zero_based, .. } = &mut e.kind {
            *zero_based = true;
        }
    };
    rewrite(src, mark, no_body)
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

// ---- C2: the conditional expression -------------------------------------------------------

/// Lua `src` with every call `cond(c, a, b)` made a conditional expression.
fn conditional(src: &str) -> Module {
    let make = |e: &mut Expr| {
        if let ExprKind::Call { callee, args } = &mut e.kind
            && matches!(&callee.kind, ExprKind::Name(n) if n == "cond")
            && args.len() == 3
        {
            let els = Box::new(args.pop().unwrap());
            let then = Box::new(args.pop().unwrap());
            let cond = Box::new(args.pop().unwrap());
            e.kind = ExprKind::Cond { cond, then, els };
        }
    };
    rewrite(src, make, no_body)
}

fn conditional_executors(src: &str) -> Executors {
    Executors::from_module(conditional(src), &TypeConfig::default(), src)
}

fn conditional_error(src: &str) -> Diagnostics {
    grindlang::analyze_module(&conditional(src), &TypeConfig::default())
        .err()
        .unwrap_or_else(|| panic!("expected an error:\n{src}"))
}

#[test]
fn a_conditional_expression_picks_one_branch() {
    let src = "LABEL = cond(1 < 2, \"low\", \"high\")\n\
               function pick(c) return cond(c, \"yes\", \"no\") end\n\
               function sign(x) return cond(x < 0, -1, cond(x == 0, 0, 1)) end\n\
               function flip(c) return cond(c, false, true) end\n\
               function label() return LABEL end\n\
               function only_the_chosen_branch_runs(c)\n\
                 local n = 0\n\
                 local bump = function() n = n + 1 return n end\n\
                 local v = cond(c, bump(), 100)\n\
                 return v + n * 1000\n\
               end\n\
               function closures(c)\n\
                 local k = 10\n\
                 local f = cond(c, function(x) return x + k end, function(x) return x - k end)\n\
                 return f(1)\n\
               end\n\
               function in_a_loop(n)\n\
                 local out = \"\"\n\
                 for i = 1, n do out = out .. cond(i % 2 == 0, \"e\", tostring(i)) end\n\
                 return out\n\
               end";
    let mut ex = conditional_executors(src);
    ex.assert_returns("pick", vec![Value::Bool(true)], "yes");
    ex.assert_returns("pick", vec![Value::Bool(false)], "no");
    for (x, want) in [(-5.0, "-1"), (0.0, "0"), (3.0, "1")] {
        ex.assert_returns("sign", vec![num(x)], want);
    }
    ex.assert_returns("flip", vec![Value::Bool(true)], "false");
    ex.assert_returns("label", vec![], "low");
    ex.assert_returns(
        "only_the_chosen_branch_runs",
        vec![Value::Bool(true)],
        "1001",
    );
    ex.assert_returns(
        "only_the_chosen_branch_runs",
        vec![Value::Bool(false)],
        "100",
    );
    ex.assert_returns("closures", vec![Value::Bool(true)], "11");
    ex.assert_returns("closures", vec![Value::Bool(false)], "-9");
    ex.assert_returns("in_a_loop", vec![num(5.0)], "1e3e5");
}

#[test]
fn a_conditional_expression_joins_and_narrows_like_or_and_if() {
    let src = "function maybe(c) return cond(c, 5, nil) end\n\
               function or_default(s)\n\
                 local x = tonumber(s)\n\
                 return cond(x == nil, 0, x + 1)\n\
               end\n\
               function doubled(s)\n\
                 local x = tonumber(s)\n\
                 return cond(x ~= nil, x * 2, -1)\n\
               end\n\
               function unwrapped(s)\n\
                 local v = maybe(s == \"y\")\n\
                 if v ~= nil then return v end\n\
                 return -1\n\
               end";
    let (_, info) =
        grindlang::analyze_module(&conditional(src), &TypeConfig::default()).expect("checks");
    assert_eq!(info.exports["maybe"].to_string(), "fn(bool) -> number?");
    assert_eq!(
        info.exports["or_default"].to_string(),
        "fn(string) -> number"
    );
    let mut ex = conditional_executors(src);
    ex.assert_returns("or_default", vec![string("x")], "0");
    ex.assert_returns("or_default", vec![string("4")], "5");
    ex.assert_returns("doubled", vec![string("4")], "8");
    ex.assert_returns("doubled", vec![string("x")], "-1");
    ex.assert_returns("unwrapped", vec![string("y")], "5");
    ex.assert_returns("unwrapped", vec![string("n")], "-1");
    ex.assert_returns("maybe", vec![Value::Bool(false)], "nil");
}

#[test]
fn a_conditional_expression_is_checked() {
    let code = |src: &str| conditional_error(src).iter().next().unwrap().code;
    // Branches with no one type, as for `or`.
    assert_eq!(code("function f(c) return cond(c, 1, \"x\") end"), "E0455");
    // A condition is a bool.
    assert_eq!(code("function f() return cond(1, 2, 3) end"), "E0400");
    // A branch is a value.
    assert_eq!(
        code("function g() end\nfunction f(c) return cond(c, g(), 1) end"),
        "E0435"
    );
    // `== nil` narrows the else branch only.
    assert_eq!(
        code("function f(s)\n  local x = tonumber(s)\n  return cond(x == nil, x + 1, 0)\nend"),
        "E0401"
    );
}

// ---- C3: `continue` -----------------------------------------------------------------------

#[test]
fn continue_goes_on_to_the_next_pass_of_every_loop() {
    let src = "local EVENS = {}\n\
               for i = 1, 6 do\n\
                 if i % 2 == 1 then continue end\n\
                 EVENS[#EVENS + 1] = i\n\
               end\n\
               function evens() return EVENS end\n\
               function odd_sum(xs)\n\
                 local t = 0\n\
                 for _, x in ipairs(xs) do\n\
                   if x % 2 == 0 then continue end\n\
                   t = t + x\n\
                 end\n\
                 return t\n\
               end\n\
               function skip_threes(n)\n\
                 local hits, i = 0, 0\n\
                 while i < n do\n\
                   i = i + 1\n\
                   if i % 3 == 0 then continue end\n\
                   hits = hits + 1\n\
                 end\n\
                 return hits\n\
               end\n\
               function positive(m)\n\
                 local n = 0\n\
                 for _, v in pairs(m) do\n\
                   if v < 0 then continue end\n\
                   n = n + v\n\
                 end\n\
                 return n\n\
               end\n\
               function triangle(n)\n\
                 local c = 0\n\
                 for i = 1, n do\n\
                   for j = 1, n do\n\
                     if j > i then continue end\n\
                     c = c + 1\n\
                   end\n\
                 end\n\
                 return c\n\
               end\n\
               function joined(n)\n\
                 local s, i = \"\", 0\n\
                 while i < n do\n\
                   i = i + 1\n\
                   if i % 2 == 0 then\n\
                     s = s .. \"x\"\n\
                     continue\n\
                   end\n\
                   s = s .. tostring(i)\n\
                 end\n\
                 return s\n\
               end\n\
               function backwards(n)\n\
                 local s = 0\n\
                 for i = n, 1, -1 do\n\
                   if i == 2 then continue end\n\
                   s = s * 10 + i\n\
                 end\n\
                 return s\n\
               end";
    let mut ex = Executors::new(src, &TypeConfig::default());
    ex.assert_returns("evens", vec![], "[2, 4, 6]");
    let xs = Value::array(vec![num(1.0), num(2.0), num(3.0), num(4.0), num(5.0)]);
    ex.assert_returns("odd_sum", vec![xs], "9");
    ex.assert_returns("skip_threes", vec![num(10.0)], "7");
    let mut m = std::collections::BTreeMap::new();
    m.insert("a".to_string(), num(4.0));
    m.insert("b".to_string(), num(-1.0));
    m.insert("c".to_string(), num(2.0));
    ex.assert_returns("positive", vec![Value::table(m)], "6");
    ex.assert_returns("triangle", vec![num(4.0)], "10");
    ex.assert_returns("joined", vec![num(5.0)], "1x3x5");
    ex.assert_returns("backwards", vec![num(4.0)], "431");
}

#[test]
fn continue_is_a_statement_only_where_luau_makes_it_one() {
    // The name stays usable: called, assigned, or read, it is an ordinary name.
    let src = "function f()\n\
                 local continue = 3\n\
                 continue = continue + 1\n\
                 return continue\n\
               end";
    Executors::new(src, &TypeConfig::default()).assert_returns("f", vec![], "4");
    let code = |src: &str| {
        grindlang::analyze(src, &TypeConfig::default())
            .err()
            .unwrap_or_else(|| panic!("expected an error:\n{src}"))
            .iter()
            .next()
            .unwrap()
            .code
    };
    assert_eq!(code("function f() continue end"), "E0311");
    assert_eq!(code("continue\n"), "E0311");
    // A function literal is a new function: the loop around it doesn't count.
    assert_eq!(
        code("function f()\n  for i = 1, 2 do\n    local g = function() continue end\n  end\nend"),
        "E0311"
    );
}

// ---- C4: returning `nil` at the end -------------------------------------------------------

/// Lua `src` with every function returning `nil` where a path gives no value, as the Ruby
/// surface's do.
fn nil_fallthrough(src: &str) -> Module {
    let mark = |b: &mut FuncBody| b.fallthrough = Fallthrough::Nil;
    rewrite(src, no_expr, mark)
}

#[test]
fn a_path_without_a_value_returns_nil() {
    let src = "function maybe(c) if c then return 1 end end\n\
               function first_big(xs)\n\
                 for _, x in ipairs(xs) do\n\
                   if x > 10 then return x end\n\
                 end\n\
               end\n\
               function bare(c) if c then return end return 2 end\n\
               function nothing() end\n\
               function act(c) if c then return nothing() end end\n\
               function mixed(c) if c then return 5 end return nothing() end\n\
               function chained() return act(true) end\n\
               function uses_chained(c) if c then return chained() end end\n\
               function closure(c)\n\
                 local f = function(x) if x > 0 then return x end end\n\
                 local v = f(c)\n\
                 if v ~= nil then return v end\n\
                 return -1\n\
               end";
    let module = nil_fallthrough(src);
    let (_, info) = grindlang::analyze_module(&module, &TypeConfig::default())
        .unwrap_or_else(|d| panic!("{}", d.render(src)));
    let sig = |name: &str| info.exports[name].to_string();
    assert_eq!(sig("maybe"), "fn(bool) -> number?");
    assert_eq!(sig("first_big"), "fn(array<number>) -> number?");
    assert_eq!(sig("bare"), "fn(bool) -> number?");
    // Returning a call that gives nothing gives nothing, unless another path gives a value.
    assert_eq!(sig("act"), "fn(bool) -> ()");
    assert_eq!(sig("chained"), "fn() -> ()");
    assert_eq!(sig("uses_chained"), "fn(bool) -> ()");
    assert_eq!(sig("mixed"), "fn(bool) -> number?");
    let mut ex = Executors::from_module(module, &TypeConfig::default(), src);
    ex.assert_returns("maybe", vec![Value::Bool(true)], "1");
    ex.assert_returns("maybe", vec![Value::Bool(false)], "nil");
    let xs = |v: &[f64]| Value::array(v.iter().map(|&x| num(x)).collect());
    ex.assert_returns("first_big", vec![xs(&[3.0, 12.0, 20.0])], "12");
    ex.assert_returns("first_big", vec![xs(&[3.0])], "nil");
    ex.assert_returns("bare", vec![Value::Bool(true)], "nil");
    ex.assert_returns("bare", vec![Value::Bool(false)], "2");
    ex.assert_returns("act", vec![Value::Bool(true)], "nil");
    ex.assert_returns("mixed", vec![Value::Bool(true)], "5");
    ex.assert_returns("mixed", vec![Value::Bool(false)], "nil");
    ex.assert_returns("closure", vec![num(3.0)], "3");
    ex.assert_returns("closure", vec![num(-3.0)], "-1");
}

// ---- C10: a function literal takes a declared parameter's type ---------------------------------

/// A host function `keep` of type `fn(ty) -> ty`, which gives back the function it is passed.
fn keeping(ty: Type) -> TypeConfig {
    let mut cfg = TypeConfig::default();
    cfg.host_functions.insert(
        "keep".to_string(),
        FnType {
            params: vec![ty.clone()],
            ret: Box::new(ty),
        },
    );
    cfg
}

#[test]
fn a_function_literal_takes_a_declared_parameters_type() {
    // `keep` takes a `fn() -> number?`, which a literal giving a `number` now is.
    let cfg = keeping(Type::function(vec![], Type::optional(Type::Number)));
    let src = "function one()\n  local f = keep(function() return 1 end)\n  return f()\nend\n\
               function none()\n  local f = keep(function() return nil end)\n  return f()\nend\n";
    let mut ex = Executors::new(src, &cfg);
    ex.set_host_function("keep", |args: &[Value]| Ok(args[0].clone()));
    ex.assert_returns("one", vec![], "1");
    ex.assert_returns("none", vec![], "nil");

    // Its parameter is typed before its body is checked, so a misuse is reported where it is.
    let cfg = keeping(Type::function(vec![Type::Number], Type::Number));
    let src = "function f()\n  return keep(function(n) return not n end)\nend\n";
    let d = grindlang::analyze(src, &cfg).unwrap_err();
    let first = d.iter().next().unwrap();
    assert_eq!((first.code, first.span.slice(src)), ("E0450", "n"));
    // And its returns are checked against the declared type.
    let src = "function f()\n  return keep(function(n) return n > 1 end)\nend\n";
    let d = grindlang::analyze(src, &cfg).unwrap_err();
    let first = d.iter().next().unwrap();
    assert_eq!((first.code, first.span.slice(src)), ("E0413", "n > 1"));
}

#[test]
fn nil_at_the_end_keeps_the_other_return_rules() {
    let code = |src: &str| {
        grindlang::analyze_module(&nil_fallthrough(src), &TypeConfig::default())
            .err()
            .unwrap_or_else(|| panic!("expected an error:\n{src}"))
            .iter()
            .next()
            .unwrap()
            .code
    };
    // A declared return type is the author's: `number` isn't optional.
    assert_eq!(
        code("---@return number\nfunction f(c) if c then return 1 end end"),
        "E0480"
    );
    // A tuple can't be `nil`.
    assert_eq!(code("function f(c) if c then return 1, 2 end end"), "E0480");
    // A function that returns nothing is still not a value.
    assert_eq!(
        code(
            "function g() end\nfunction f(c) if c then return g() end end\nfunction h() local x = f(true) end"
        ),
        "E0435"
    );
    // The Lua surface keeps its rule.
    let lua = "function f(c) if c then return 1 end end";
    let d = grindlang::analyze(lua, &TypeConfig::default()).unwrap_err();
    assert_eq!(d.iter().next().unwrap().code, "E0480");
}
