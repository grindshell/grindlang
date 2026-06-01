//! Phase 9 benchmarks (`PLAN.md` §4): parse / compile / call latency, the cranelift **JIT
//! vs. the interpreters**, and a rough **Luau baseline** for a representative calculation and
//! a loop/recursion workload.
//!
//! Run:
//! * `cargo bench` — front end, grindlang JIT compile/call, and the Luau baseline.
//! * `cargo bench --features interp` — additionally times the AST `Interpreter` and IR `Vm`,
//!   so the JIT-vs-interpreter comparison is included.
//!
//! Programs are kept self-contained (default [`TypeConfig`], no host functions or memory) so
//! each grindlang executor and its Luau twin run the identical computation.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use grindlang::codegen::JitModule;
use grindlang::{Program, TypeConfig, Value};

// ---- workloads (grindlang) --------------------------------------------------

const FIB: &str = "\
function fib(n)
  if n < 2 then return n end
  return fib(n - 1) + fib(n - 2)
end";

const LOOPSUM: &str = "\
function loopsum(n)
  local s = 0
  local i = 1
  while i <= n do
    s = s + i
    i = i + 1
  end
  return s
end";

const MITIGATED: &str = "\
function mitigated(attack, defense)
  local raw = attack * 1.25
  local dmg = raw - defense * 0.5
  if dmg < 0 then return 0 end
  return math.floor(dmg)
end";

// ---- workloads (Luau twins) -------------------------------------------------

const FIB_LUAU: &str = "\
function fib(n)
  if n < 2 then return n end
  return fib(n - 1) + fib(n - 2)
end";

const LOOPSUM_LUAU: &str = "\
function loopsum(n)
  local s = 0
  local i = 1
  while i <= n do
    s = s + i
    i = i + 1
  end
  return s
end";

const MITIGATED_LUAU: &str = "\
function mitigated(attack, defense)
  local raw = attack * 1.25
  local dmg = raw - defense * 0.5
  if dmg < 0 then return 0 end
  return math.floor(dmg)
end";

/// The three workloads as `(label, grindlang src, luau src, export name, args)`.
fn workloads() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Vec<f64>,
)> {
    vec![
        ("fib", FIB, FIB_LUAU, "fib", vec![20.0]),
        ("loopsum", LOOPSUM, LOOPSUM_LUAU, "loopsum", vec![1000.0]),
        (
            "mitigated",
            MITIGATED,
            MITIGATED_LUAU,
            "mitigated",
            vec![120.0, 80.0],
        ),
    ]
}

fn lower(src: &str) -> Program {
    let cfg = TypeConfig::default();
    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
    grindlang::ir::verify(&program).expect("verify");
    program
}

fn num_args(args: &[f64]) -> Vec<Value> {
    args.iter().copied().map(Value::Number).collect()
}

// ---- front end: parse / analyze / compile-to-IR -----------------------------

fn bench_frontend(c: &mut Criterion) {
    let cfg = TypeConfig::default();
    let mut g = c.benchmark_group("frontend");
    for (label, src, ..) in workloads() {
        g.bench_function(format!("parse/{label}"), |b| {
            b.iter(|| grindlang::parse(black_box(src)).expect("parse"))
        });
        g.bench_function(format!("analyze/{label}"), |b| {
            b.iter(|| grindlang::analyze(black_box(src), &cfg).expect("analyze"))
        });
        g.bench_function(format!("compile_ir/{label}"), |b| {
            b.iter(|| grindlang::compile(black_box(src), &cfg).expect("compile"))
        });
    }
    g.finish();
}

// ---- backend compile: grindlang JIT vs. Luau --------------------------------

fn bench_compile(c: &mut Criterion) {
    let mut g = c.benchmark_group("backend_compile");
    for (label, src, luau_src, ..) in workloads() {
        let program = lower(src);
        g.bench_function(format!("jit/{label}"), |b| {
            b.iter(|| JitModule::compile(black_box(&program)).expect("jit compile"))
        });
        g.bench_function(format!("luau/{label}"), |b| {
            let lua = mlua::Lua::new();
            b.iter(|| {
                lua.load(black_box(luau_src))
                    .into_function()
                    .expect("luau compile")
            })
        });
    }
    g.finish();
}

// ---- call latency: JIT vs. interpreters vs. Luau ----------------------------

fn bench_call(c: &mut Criterion) {
    let mut g = c.benchmark_group("call");
    for (label, src, luau_src, func, args) in workloads() {
        let program = lower(src);

        // grindlang JIT (the production backend).
        {
            let mut jit = JitModule::compile(&program).expect("jit");
            let args = num_args(&args);
            g.bench_function(format!("jit/{label}"), |b| {
                b.iter(|| jit.call(func, black_box(args.clone())).expect("jit call"))
            });
        }

        // grindlang interpreters (oracles) — only with the `interp` feature.
        #[cfg(feature = "interp")]
        {
            let cfg = TypeConfig::default();
            let (module, res, _info) = grindlang::analyze(src, &cfg).expect("analyze");
            let mut interp = grindlang::interp::Interpreter::new(&module, &res).expect("interp");
            let iargs = num_args(&args);
            g.bench_function(format!("interp/{label}"), |b| {
                b.iter(|| {
                    interp
                        .call(func, black_box(iargs.clone()))
                        .expect("interp call")
                })
            });

            let mut vm = grindlang::ir::Vm::new(&program);
            let vargs = num_args(&args);
            g.bench_function(format!("vm/{label}"), |b| {
                b.iter(|| vm.call(func, black_box(vargs.clone())).expect("vm call"))
            });
        }

        // Luau baseline (mlua, luau-jit).
        {
            let lua = mlua::Lua::new();
            lua.load(luau_src).exec().expect("luau load");
            let f: mlua::Function = lua.globals().get(func).expect("luau fn");
            g.bench_function(format!("luau/{label}"), |b| {
                b.iter(|| {
                    let r: f64 = match args.len() {
                        1 => f.call(black_box(args[0])).expect("luau call"),
                        2 => f.call(black_box((args[0], args[1]))).expect("luau call"),
                        _ => unreachable!("benchmark workloads take 1 or 2 args"),
                    };
                    r
                })
            });
        }
    }
    g.finish();
}

criterion_group!(benches, bench_frontend, bench_compile, bench_call);
criterion_main!(benches);
