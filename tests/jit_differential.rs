//! Phase 7 exit criterion: the **JIT is a third semantics oracle**. For a corpus of
//! programs, run each through the AST interpreter, the IR `Vm`, *and* the cranelift
//! `JitModule`, and assert all three agree. Any divergence is a codegen bug.
//!
//! Values are compared by their `Display` form (deterministic: sorted table keys,
//! integer-formatted whole numbers), exactly as the IR differential test does.

#![cfg(all(feature = "interp", feature = "jit"))]

use std::collections::BTreeMap;

use grindlang::codegen::JitModule;
use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::{Type, TypeConfig, Value};

/// Build all three executors for a program. Artifacts are leaked for `'static` — fine for a
/// test harness.
fn triple(src: &str, cfg: &TypeConfig) -> (Interpreter<'static>, Vm<'static>, JitModule) {
    let (module, res, info) = grindlang::analyze(src, cfg).expect("analyze");
    let module = Box::leak(Box::new(module));
    let res = Box::leak(Box::new(res));
    let info = Box::leak(Box::new(info));
    let program = Box::leak(Box::new(
        grindlang::ir::lower(module, res, info, cfg).expect("lower"),
    ));
    grindlang::ir::verify(program).expect("verify");

    let interp = Interpreter::new(module, res).expect("interp");
    let vm = Vm::new(program);
    let jit = JitModule::compile(program).expect("jit compile");
    (interp, vm, jit)
}

/// Assert AST == IR == JIT for one call (default config, no host/memory).
fn assert_same(src: &str, func: &str, args: Vec<Value>) {
    let cfg = TypeConfig::default();
    let (mut interp, mut vm, mut jit) = triple(src, &cfg);
    let a = interp.call(func, args.clone());
    let b = vm.call(func, args.clone());
    let c = jit.call(func, args);
    match (a, b, c) {
        (Ok(av), Ok(bv), Ok(cv)) => {
            let (sa, sb, sc) = (format!("{av}"), format!("{bv}"), format!("{cv}"));
            assert_eq!(sa, sb, "AST vs IR mismatch for `{func}` in:\n{src}");
            assert_eq!(sb, sc, "IR vs JIT mismatch for `{func}` in:\n{src}");
        }
        (Err(_), Err(_), Err(_)) => {} // all three agree it fails
        (a, b, c) => panic!("oracle disagreement for `{func}`: {a:?} / {b:?} / {c:?}\n{src}"),
    }
}

#[test]
fn arithmetic_and_precedence() {
    assert_same(
        "function f(a, b) return a + b * 2 - 1 end",
        "f",
        vec![Value::Number(3.0), Value::Number(4.0)],
    );
}

#[test]
fn floor_div_mod_pow() {
    assert_same(
        "function f(a, b) return a // b + a % b end",
        "f",
        vec![Value::Number(17.0), Value::Number(5.0)],
    );
    assert_same(
        "function f(x) return x ^ 3 end",
        "f",
        vec![Value::Number(2.0)],
    );
    // Negative operands exercise floor-div / mod sign behavior.
    assert_same(
        "function f(a, b) return a // b end",
        "f",
        vec![Value::Number(-7.0), Value::Number(2.0)],
    );
    assert_same(
        "function f(a, b) return a % b end",
        "f",
        vec![Value::Number(-7.0), Value::Number(3.0)],
    );
}

#[test]
fn branches_and_comparison() {
    let src = "function classify(n)\n\
                 if n < 0 then return \"neg\" end\n\
                 if n == 0 then return \"zero\" end\n\
                 return \"pos\"\n\
               end";
    for n in [-5.0, 0.0, 9.0] {
        assert_same(src, "classify", vec![Value::Number(n)]);
    }
}

#[test]
fn while_loop_accumulation() {
    let src = "function fact(n)\n\
                 local acc = 1\n\
                 local i = 2\n\
                 while i <= n do\n\
                   acc = acc * i\n\
                   i = i + 1\n\
                 end\n\
                 return acc\n\
               end";
    for n in [0.0, 1.0, 5.0, 8.0] {
        assert_same(src, "fact", vec![Value::Number(n)]);
    }
}

#[test]
fn numeric_for_negative_step() {
    let src = "function countdown(n)\n\
                 local s = 0\n\
                 for i = n, 1, -1 do\n\
                   s = s + i\n\
                 end\n\
                 return s\n\
               end";
    for n in [0.0, 1.0, 6.0] {
        assert_same(src, "countdown", vec![Value::Number(n)]);
    }
}

#[test]
fn generic_for_ipairs() {
    let src = "function total(xs)\n\
                 local s = 0\n\
                 for _, v in ipairs(xs) do\n\
                   s = s + v\n\
                 end\n\
                 return s\n\
               end";
    let arr = Value::array(vec![
        Value::Number(2.0),
        Value::Number(4.0),
        Value::Number(6.0),
    ]);
    assert_same(src, "total", vec![arr]);
}

#[test]
fn short_circuit_and_or() {
    let and_src = "function f(a, b) return (a == true) and (b == true) end";
    let or_src = "function f(a, b) return (a == true) or (b == true) end";
    for a in [true, false] {
        for b in [true, false] {
            assert_same(and_src, "f", vec![Value::Bool(a), Value::Bool(b)]);
            assert_same(or_src, "f", vec![Value::Bool(a), Value::Bool(b)]);
        }
    }
}

#[test]
fn break_in_loop() {
    let src = "function firsthit(xs)\n\
                 local found = 0\n\
                 for _, v in ipairs(xs) do\n\
                   if v > 2 then\n\
                     found = v\n\
                     break\n\
                   end\n\
                 end\n\
                 return found\n\
               end";
    let arr = Value::array(vec![
        Value::Number(1.0),
        Value::Number(3.0),
        Value::Number(5.0),
    ]);
    assert_same(src, "firsthit", vec![arr]);
}

#[test]
fn string_builtins_and_concat() {
    assert_same(
        "function greet(name) return \"hi \" .. string.upper(name) end",
        "greet",
        vec![Value::string("bob")],
    );
    assert_same(
        "function f(s) return string.sub(s, 2, 4) end",
        "f",
        vec![Value::string("grindlang")],
    );
    assert_same(
        "function f(s, p) return string.find(s, p) end",
        "f",
        vec![Value::string("hello"), Value::string("ll")],
    );
}

#[test]
fn string_comparison() {
    // `string.upper` pins both params to `string`, so `<` is a string comparison (a bare
    // `a < b` would infer the params as numbers — numeric-by-default for relational ops).
    let src = "function f(a, b) return string.upper(a) < string.upper(b) end";
    assert_same(
        src,
        "f",
        vec![Value::string("apple"), Value::string("banana")],
    );
    assert_same(src, "f", vec![Value::string("zebra"), Value::string("ant")]);
    assert_same(src, "f", vec![Value::string("same"), Value::string("same")]);
}

#[test]
fn math_builtins_and_const() {
    let src = "K = 10\nfunction f(x) return math.floor(x) + K end";
    assert_same(src, "f", vec![Value::Number(3.7)]);
    assert_same(
        "function f(a, b) return math.max(a, b) - math.min(a, b) end",
        "f",
        vec![Value::Number(5.0), Value::Number(12.0)],
    );
}

#[test]
fn tostring_tonumber() {
    // `n + 0` pins `n` to `number` (a bare `tostring(n)` leaves it ambiguous → no inference).
    assert_same(
        "function f(n) return tostring(n + 0) end",
        "f",
        vec![Value::Number(42.0)],
    );
    // `tonumber` already requires a `string`, so `s` is pinned.
    assert_same(
        "function f(s) return tonumber(s) end",
        "f",
        vec![Value::string("3.5")],
    );
}

#[test]
fn array_build_and_append() {
    let src = "function grow(n)\n\
                 local out = { 1 }\n\
                 out[#out + 1] = n\n\
                 return out\n\
               end";
    assert_same(src, "grow", vec![Value::Number(9.0)]);
}

#[test]
fn record_literal_and_field() {
    let src = "function mk(h)\n\
                 local r = { hp = h, alive = h > 0 }\n\
                 return r.hp\n\
               end";
    assert_same(src, "mk", vec![Value::Number(7.0)]);
}

#[test]
fn recursion() {
    let src = "function fib(n)\n\
                 if n < 2 then return n end\n\
                 return fib(n - 1) + fib(n - 2)\n\
               end";
    for n in [0.0, 1.0, 7.0, 10.0] {
        assert_same(src, "fib", vec![Value::Number(n)]);
    }
}

#[test]
fn mutual_recursion() {
    let src = "function even(n)\n\
                 if n == 0 then return true end\n\
                 return odd(n - 1)\n\
               end\n\
               function odd(n)\n\
                 if n == 0 then return false end\n\
                 return even(n - 1)\n\
               end";
    for n in [0.0, 1.0, 6.0, 9.0] {
        assert_same(src, "even", vec![Value::Number(n)]);
    }
}

#[test]
fn stat_calc_fixture() {
    let path = format!(
        "{}/tests/fixtures/stat_calc.lua",
        env!("CARGO_MANIFEST_DIR")
    );
    let src = std::fs::read_to_string(path).unwrap();
    assert_same(
        &src,
        "mitigated",
        vec![Value::Number(120.0), Value::Number(80.0)],
    );
    assert_same(
        &src,
        "lethal",
        vec![
            Value::Number(120.0),
            Value::Number(80.0),
            Value::Number(50.0),
        ],
    );
}

#[test]
fn curated_export() {
    let src = "function impl() return 7 end\nreturn { run = impl }";
    assert_same(src, "run", vec![]);
}

/// Memory read/write and `pairs` over a map, across all three oracles.
#[test]
fn memory_and_pairs() {
    let src = "function tally()\n\
                 local sum = 0\n\
                 for k, v in pairs(mem) do\n\
                   sum = sum + v\n\
                 end\n\
                 mem.total = sum\n\
                 return sum\n\
               end";

    let mut tc_mem = BTreeMap::new();
    tc_mem.insert("mem".to_string(), Type::Map(Box::new(Type::Number)));
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory: tc_mem,
    };

    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
    grindlang::ir::verify(&program).expect("verify");

    let make_mem = || {
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), Value::Number(10.0));
        m.insert("b".to_string(), Value::Number(20.0));
        Value::table(m)
    };

    let mut interp = Interpreter::new(&module, &res).unwrap();
    interp.set_memory("mem", make_mem());
    let ar = interp.call("tally", vec![]).unwrap();

    let mut vm = Vm::new(&program);
    vm.set_memory("mem", make_mem());
    let br = vm.call("tally", vec![]).unwrap();

    let mut jit = JitModule::compile(&program).expect("jit");
    jit.set_memory("mem", make_mem());
    let cr = jit.call("tally", vec![]).unwrap();

    assert_eq!(format!("{ar}"), format!("{br}"));
    assert_eq!(format!("{br}"), format!("{cr}"));
    // Memory writes agree across IR and JIT too.
    assert_eq!(
        format!("{}", vm.memory("mem").unwrap()),
        format!("{}", jit.memory("mem").unwrap())
    );
}

/// A host function called from JIT-compiled code.
#[test]
fn host_function() {
    let src = "function f(n) return roll(n) + 1 end";
    let mut host = BTreeMap::new();
    host.insert(
        "roll".to_string(),
        grindlang::FnType {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        },
    );
    let cfg = TypeConfig {
        host_functions: host,
        memory: BTreeMap::new(),
    };

    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");

    let mut interp = Interpreter::new(&module, &res).unwrap();
    interp.set_host_function("roll", |a| Ok(Value::Number(a[0].as_f64().unwrap() * 2.0)));
    let ar = interp.call("f", vec![Value::Number(3.0)]).unwrap();

    let mut jit = JitModule::compile(&program).expect("jit");
    jit.set_host_function("roll", |a| Ok(Value::Number(a[0].as_f64().unwrap() * 2.0)));
    let cr = jit.call("f", vec![Value::Number(3.0)]).unwrap();

    assert_eq!(format!("{ar}"), format!("{cr}"));
}

/// A host error propagates out of the JIT as `Err`.
#[test]
fn host_error_propagates() {
    let src = "function f() return boom() end";
    let mut host = BTreeMap::new();
    host.insert(
        "boom".to_string(),
        grindlang::FnType {
            params: vec![],
            ret: Box::new(Type::Number),
        },
    );
    let cfg = TypeConfig {
        host_functions: host,
        memory: BTreeMap::new(),
    };
    let (module, res, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let program = grindlang::ir::lower(&module, &res, &info, &cfg).expect("lower");
    let mut jit = JitModule::compile(&program).expect("jit");
    jit.set_host_function("boom", |_| Err(grindlang::RunError::Host("kaboom".into())));
    let err = jit.call("f", vec![]).unwrap_err();
    assert!(matches!(err, grindlang::RunError::Host(m) if m == "kaboom"));
}
