//! Randomized differential testing for the JIT (`PLAN.md` Phase 7 exit criterion: "interp
//! result == jit result for a large corpus, incl. fuzzed inputs").
//!
//! A small deterministic LCG generates argument vectors; for each program in a corpus we run
//! the AST interpreter, the IR `Vm`, and the cranelift `JitModule` over many random inputs
//! and assert all three agree (by `Display`). Deterministic so failures reproduce exactly.

#![cfg(all(feature = "interp", feature = "jit"))]

use grindlang::codegen::JitModule;
use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::{TypeConfig, Value};

/// A tiny deterministic LCG (numerical-recipes constants) — no external rng, reproducible.
struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    /// A "nice" number in [-50, 50], sometimes whole — exercises both integer-ish and
    /// fractional paths.
    fn number(&mut self) -> f64 {
        let r = self.next_u64();
        let whole = (r % 101) as i64 - 50;
        if r & 0x100 == 0 {
            whole as f64
        } else {
            whole as f64 + ((r >> 16) % 1000) as f64 / 1000.0
        }
    }
}

fn triple(src: &str) -> (Interpreter<'static>, Vm<'static>, JitModule) {
    let cfg = TypeConfig::default();
    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let module = Box::leak(Box::new(module));
    let res = Box::leak(Box::new(res));
    let info = Box::leak(Box::new(info));
    let program = Box::leak(Box::new(
        grindlang::ir::lower(module, res, info, &cfg).expect("lower"),
    ));
    grindlang::ir::verify(program).expect("verify");
    let interp = Interpreter::new(module, res).expect("interp");
    let vm = Vm::new(program);
    let jit = JitModule::compile(program).expect("jit");
    (interp, vm, jit)
}

/// Fuzz one function over `iters` random numeric argument vectors of width `arity`.
fn fuzz_numeric(src: &str, func: &str, arity: usize, seed: u64, iters: usize) {
    let (mut interp, mut vm, mut jit) = triple(src);
    let mut rng = Lcg(seed);
    for _ in 0..iters {
        let args: Vec<Value> = (0..arity).map(|_| Value::Number(rng.number())).collect();
        let a = interp.call(func, args.clone());
        let b = vm.call(func, args.clone());
        let c = jit.call(func, args.clone());
        match (a, b, c) {
            (Ok(av), Ok(bv), Ok(cv)) => {
                let (sa, sb, sc) = (format!("{av}"), format!("{bv}"), format!("{cv}"));
                assert_eq!(sa, sb, "AST vs IR for `{func}{args:?}`\n{src}");
                assert_eq!(sb, sc, "IR vs JIT for `{func}{args:?}`\n{src}");
            }
            (Err(_), Err(_), Err(_)) => {}
            (a, b, c) => panic!("disagreement `{func}{args:?}`: {a:?}/{b:?}/{c:?}\n{src}"),
        }
    }
}

#[test]
fn fuzz_arithmetic_mix() {
    let src = "function f(a, b, c)\n\
                 return (a + b) * c - a / (b + 1.0) + a % 7.0 + b // 3.0\n\
               end";
    fuzz_numeric(src, "f", 3, 0xC0FFEE, 500);
}

#[test]
fn fuzz_branches_and_compare() {
    let src = "function f(a, b)\n\
                 if a < b then return a * 2.0 end\n\
                 if a == b then return 0.0 end\n\
                 return b - a\n\
               end";
    fuzz_numeric(src, "f", 2, 0x1234_5678, 500);
}

#[test]
fn fuzz_loop_accumulate() {
    // A bounded loop whose trip count depends on the input.
    let src = "function f(n)\n\
                 local s = 0.0\n\
                 local i = 1\n\
                 while i <= n do\n\
                   s = s + i * 1.5\n\
                   i = i + 1\n\
                 end\n\
                 return s\n\
               end";
    fuzz_numeric(src, "f", 1, 0xABCD, 300);
}

#[test]
fn fuzz_math_builtins() {
    let src = "function f(a, b)\n\
                 return math.floor(a) + math.max(a, b) - math.min(a, b) + math.abs(a)\n\
               end";
    fuzz_numeric(src, "f", 2, 0x5EED, 500);
}

#[test]
fn fuzz_recursion() {
    // Clamp via comparison so deep/negative inputs terminate identically everywhere.
    let src = "function fib(n)\n\
                 if n < 2.0 then return n end\n\
                 if n > 20.0 then return 0.0 end\n\
                 return fib(n - 1.0) + fib(n - 2.0)\n\
               end";
    fuzz_numeric(src, "fib", 1, 0xF1B0, 200);
}

#[test]
fn fuzz_short_circuit() {
    let src = "function f(a, b)\n\
                 return (a < b) and (b < 10.0) or (a == 0.0)\n\
               end";
    fuzz_numeric(src, "f", 2, 0x5C2C, 500);
}
