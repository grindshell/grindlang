//! Property-based differential testing (`PLAN.md` Phase 9). Where `tests/jit_fuzz.rs` drives
//! a fixed corpus with a deterministic LCG, this generates **random programs** (arithmetic
//! expression trees over the parameters `a`, `b`, `c` and integer literals) *and* random
//! argument vectors with `proptest`, then asserts the three semantics oracles agree:
//! AST `Interpreter` == IR `Vm` == cranelift `JitModule`.
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

/// A numeric expression over `a`/`b`/`c` and non-negative integer literals. Every node is
/// fully parenthesized so the rendered source is unambiguous regardless of precedence.
/// Literals are non-negative; negative values arise only via the unary-minus branch (so we
/// never emit `a + -5`-style adjacency).
fn arb_expr() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        (0i64..=20i64).prop_map(|n| n.to_string()),
        prop::sample::select(vec!["a", "b", "c"]).prop_map(|s| s.to_string()),
    ];
    // depth 6, max 48 nodes, ~2 children per branch.
    leaf.prop_recursive(6, 48, 2, |inner| {
        prop_oneof![
            (
                inner.clone(),
                prop::sample::select(vec!["+", "-", "*", "/", "//", "%", "^"]),
                inner.clone(),
            )
                .prop_map(|(l, op, r)| format!("({l} {op} {r})")),
            inner.prop_map(|e| format!("(-{e})")),
        ]
    })
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
