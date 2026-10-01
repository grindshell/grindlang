//! Property-based differential testing (`PLAN.md` Phase 9). Where `tests/jit_fuzz.rs` drives
//! a fixed corpus with a deterministic LCG, this generates **random programs** (arithmetic
//! expression trees over the parameters `a`, `b`, `c` and integer literals) *and* random
//! argument vectors with `proptest`, then asserts the three semantics oracles agree:
//! AST `Interpreter` == IR `Vm` == cranelift `JitModule`.
//!
//! [`interp_vm_jit_agree`] covers single-value functions; [`multi_return_oracles_agree`] covers
//! Tier-A multi-return (`SPEC.md` §5.5) — a generated tuple return consumed both as a direct
//! pass-through (exercising `MakeTuple` + tuple `Display` across the host boundary) and via a
//! parallel-binding unpack (exercising `TupleGet`); [`top_level_code_oracles_agree`] computes the
//! expression in top-level code instead of a function (`SPEC.md` §3.1).
//!
//! proptest **shrinks** any failure to a minimal expression + inputs, which is the point:
//! a codegen divergence is reported as the smallest program that triggers it.
//!
//! Requires both backends, like the other differential suites.

#![cfg(all(feature = "interp", feature = "jit"))]

use grindlang::codegen::JitModule;
use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::{TypeConfig, Value};
use proptest::prelude::*;

/// A generated numeric expression over `a`/`b`/`c` and non-negative integer literals, which
/// prints in either surface.
#[derive(Clone, Debug)]
enum Gen {
    Num(i64),
    Var(&'static str),
    Bin(Box<Gen>, &'static str, Box<Gen>),
    Neg(Box<Gen>),
}

impl Gen {
    /// The expression in Lua. Every node is fully parenthesized so the rendered source is
    /// unambiguous regardless of precedence. Literals are non-negative; negative values arise
    /// only via the unary-minus branch (so we never emit `a + -5`-style adjacency).
    fn lua(&self) -> String {
        match self {
            Gen::Num(n) => n.to_string(),
            Gen::Var(v) => v.to_string(),
            Gen::Bin(l, op, r) => format!("({} {op} {})", l.lua(), r.lua()),
            Gen::Neg(e) => format!("(-{})", e.lua()),
        }
    }

    /// The expression in the Ruby surface (`SPEC-RUBY.md` §4.7), where `^` is `**` and `//` is
    /// `.div`, and a negated literal is a negative literal.
    fn ruby(&self) -> String {
        match self {
            Gen::Num(n) => n.to_string(),
            Gen::Var(v) => v.to_string(),
            Gen::Bin(l, "//", r) => format!("({}.div({}))", l.ruby(), r.ruby()),
            Gen::Bin(l, "^", r) => format!("({} ** {})", l.ruby(), r.ruby()),
            Gen::Bin(l, op, r) => format!("({} {op} {})", l.ruby(), r.ruby()),
            Gen::Neg(e) => format!("(-{})", e.ruby()),
        }
    }
}

fn arb_gen() -> impl Strategy<Value = Gen> {
    let leaf = prop_oneof![
        (0i64..=20i64).prop_map(Gen::Num),
        prop::sample::select(vec!["a", "b", "c"]).prop_map(Gen::Var),
    ];
    // depth 6, max 48 nodes, ~2 children per branch.
    leaf.prop_recursive(6, 48, 2, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop::sample::select(vec!["+", "-", "*", "/", "//", "%", "^"]),
                inner.clone(),
            )
                .prop_map(|(l, op, r)| Gen::Bin(Box::new(l), op, Box::new(r))),
            inner.prop_map(|e| Gen::Neg(Box::new(e))),
        ]
    })
}

/// A generated numeric expression, in Lua.
fn arb_expr() -> impl Strategy<Value = String> {
    arb_gen().prop_map(|g| g.lua())
}

/// Render the deterministic comparison string for a call result (Ok value by `Display`, or a
/// stable `err` marker). Non-finite results (`inf`/`NaN` from `/0`, `%0`, overflow) compare
/// fine because all three backends share the same `f64` formatting.
fn render(r: &Result<Value, grindlang::RunError>) -> String {
    match r {
        Ok(v) => format!("ok:{v}"),
        Err(_) => "err".to_string(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// AST == IR == JIT for a generated expression over generated inputs.
    #[test]
    fn interp_vm_jit_agree(
        expr in arb_expr(),
        a in -50.0f64..50.0,
        b in -50.0f64..50.0,
        c in -50.0f64..50.0,
    ) {
        // Declare only the parameters the expression actually uses: an *unused* parameter has
        // an un-inferrable type and the checker rejects the program. (a/b/c are the only
        // letters the generator emits, so a substring test reliably identifies used vars.)
        let used: Vec<(&str, f64)> = [("a", a), ("b", b), ("c", c)]
            .into_iter()
            .filter(|(name, _)| expr.contains(name))
            .collect();
        let names = used.iter().map(|(n, _)| *n).collect::<Vec<_>>();
        let params = names.join(", ");
        // A param's type must be *determined* by use, not merely referenced (a bare
        // `return a` leaves `a` ambiguous → E0410). Pin every used param to `number` with a
        // throwaway arithmetic local; it has no effect on the returned value.
        let pin = if names.is_empty() {
            String::new()
        } else {
            format!("  local _pin = ({}) * 0\n", names.join(" + "))
        };
        let src = format!("function f({params})\n{pin}  return {expr}\nend");

        let cfg = TypeConfig::default();
        let (module, res, info) = grindlang::analyze(&src, &cfg).expect("analyze numeric expr");
        let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
        grindlang::ir::verify(&program).expect("verify");

        let mut interp = Interpreter::new(&module, &res).expect("interp");
        let mut vm = Vm::new(&program);
        let mut jit = JitModule::compile(&program).expect("jit compile");

        let args: Vec<Value> = used.iter().map(|(_, v)| Value::Number(*v)).collect();
        let si = render(&interp.call("f", args.clone()));
        let sv = render(&vm.call("f", args.clone()));
        let sj = render(&jit.call("f", args));

        prop_assert_eq!(&si, &sv, "AST vs IR mismatch:\n{}", src);
        prop_assert_eq!(&sv, &sj, "IR vs JIT mismatch:\n{}", src);
    }
}

#[cfg(feature = "ruby")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The same generated expression written in Lua and in the Ruby surface (`SPEC-RUBY.md`)
    /// gives the same result on every executor of both: AST == IR == JIT, and Lua == Ruby.
    #[test]
    fn surfaces_agree(
        expr in arb_gen(),
        a in -50.0f64..50.0,
        b in -50.0f64..50.0,
        c in -50.0f64..50.0,
    ) {
        use grindlang::ruby::{RubyConfig, parse_module};

        // Annotated, so that a parameter the expression doesn't use still has a type.
        let lua = format!(
            "---@param a number\n---@param b number\n---@param c number\n\
             function f(a, b, c)\n  return {}\nend\n",
            expr.lua()
        );
        let rb = format!(
            "# @param a number\n# @param b number\n# @param c number\n\
             def f(a, b, c)\n  {}\nend\n",
            expr.ruby()
        );
        let cfg = TypeConfig::default();
        let from_lua = grindlang::parse(&lua).expect("parse Lua");
        let from_ruby = parse_module(&rb, &RubyConfig::new(), &cfg)
            .unwrap_or_else(|d| panic!("translate:\n{}", d.render(&rb)));

        let args = vec![Value::Number(a), Value::Number(b), Value::Number(c)];
        let mut results = Vec::new();
        for (surface, module, src) in [("Lua", from_lua, &lua), ("Ruby", from_ruby, &rb)] {
            let (res, info) = grindlang::analyze_module(&module, &cfg)
                .unwrap_or_else(|d| panic!("analyze:\n{}", d.render(src)));
            let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
            grindlang::ir::verify(&program).expect("verify");
            let mut interp = Interpreter::new(&module, &res).expect("interp");
            let mut vm = Vm::new(&program);
            let mut jit = JitModule::compile(&program).expect("jit compile");
            results.push((format!("{surface} AST"), render(&interp.call("f", args.clone()))));
            results.push((format!("{surface} IR"), render(&vm.call("f", args.clone()))));
            results.push((format!("{surface} JIT"), render(&jit.call("f", args.clone()))));
        }
        let (first, want) = &results[0];
        for (oracle, got) in &results[1..] {
            prop_assert_eq!(got, want, "{} vs {}:\n{}\n{}", oracle, first, lua, rb);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AST == IR == JIT for a generated expression computed by **top-level code** (`SPEC.md`
    /// §3.1): the inputs are module variables, the expression initializes a constant and — built
    /// up in a loop — a second module variable, and exported functions read both back. Exercises
    /// the init function and the module slots rather than parameters and locals.
    #[test]
    fn top_level_code_oracles_agree(
        expr in arb_expr(),
        a in -50.0f64..50.0,
        b in -50.0f64..50.0,
        c in -50.0f64..50.0,
        reps in 0u32..4,
    ) {
        let src = format!(
            "local a, b, c = {a:?}, {b:?}, {c:?}\n\
             K = {expr}\n\
             local acc = 0\n\
             for i = 1, {reps} do acc = acc + K * i end\n\
             function k() return K end\n\
             function total() return acc end"
        );

        let cfg = TypeConfig::default();
        let (module, res, info) = grindlang::analyze(&src, &cfg).expect("analyze top-level code");
        let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
        grindlang::ir::verify(&program).expect("verify");

        let mut interp = Interpreter::new(&module, &res).expect("interp");
        let mut vm = Vm::new(&program);
        let mut jit = JitModule::compile(&program).expect("jit compile");

        for func in ["k", "total", "K"] {
            let si = render(&interp.call(func, vec![]));
            let sv = render(&vm.call(func, vec![]));
            let sj = render(&jit.call(func, vec![]));
            prop_assert_eq!(&si, &sv, "AST vs IR mismatch in `{}`:\n{}", func, src);
            prop_assert_eq!(&sv, &sj, "IR vs JIT mismatch in `{}`:\n{}", func, src);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AST == IR == JIT for a *multi-return* function (Tier A, `SPEC.md` §5.5). A helper `mr`
    /// returns a tuple of 2–4 generated expressions; we then check two consumers over the same
    /// inputs: `direct` returns the tuple unchanged (stressing `MakeTuple` and tuple `Display`
    /// as it crosses the host boundary), and `sum` unpacks it with a parallel `local` binding
    /// and adds the elements (stressing `TupleGet` and per-element box/unbox).
    #[test]
    fn multi_return_oracles_agree(
        exprs in prop::collection::vec(arb_expr(), 2..=4),
        a in -50.0f64..50.0,
        b in -50.0f64..50.0,
        c in -50.0f64..50.0,
    ) {
        // Union of the params actually used across *any* element expression (an unused param is
        // un-inferrable and rejected). `direct`/`sum` pass every param straight to `mr`, so each
        // is "used" there too.
        let joined = exprs.join(" ");
        let used: Vec<(&str, f64)> = [("a", a), ("b", b), ("c", c)]
            .into_iter()
            .filter(|(name, _)| joined.contains(name))
            .collect();
        let names = used.iter().map(|(n, _)| *n).collect::<Vec<_>>();
        let params = names.join(", ");
        // Pin every used param to `number` (a bare pass-through leaves it ambiguous → E0410).
        let pin = if names.is_empty() {
            String::new()
        } else {
            format!("  local _pin = ({}) * 0\n", names.join(" + "))
        };
        let ret_list = exprs.join(", ");
        let binders = (0..exprs.len())
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sum = (0..exprs.len())
            .map(|i| format!("v{i}"))
            .collect::<Vec<_>>()
            .join(" + ");
        let src = format!(
            "function mr({params})\n{pin}  return {ret_list}\nend\n\
             function direct({params})\n  return mr({params})\nend\n\
             function sum({params})\n  local {binders} = mr({params})\n  return {sum}\nend"
        );

        let cfg = TypeConfig::default();
        let (module, res, info) = grindlang::analyze(&src, &cfg).expect("analyze multi-return");
        let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
        grindlang::ir::verify(&program).expect("verify");

        let mut interp = Interpreter::new(&module, &res).expect("interp");
        let mut vm = Vm::new(&program);
        let mut jit = JitModule::compile(&program).expect("jit compile");

        let args: Vec<Value> = used.iter().map(|(_, v)| Value::Number(*v)).collect();
        for func in ["direct", "sum"] {
            let si = render(&interp.call(func, args.clone()));
            let sv = render(&vm.call(func, args.clone()));
            let sj = render(&jit.call(func, args.clone()));
            prop_assert_eq!(&si, &sv, "AST vs IR mismatch in `{}`:\n{}", func, src);
            prop_assert_eq!(&sv, &sj, "IR vs JIT mismatch in `{}`:\n{}", func, src);
        }
    }
}
