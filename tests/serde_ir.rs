//! IR serialization round-trip (`serde` + `jit`).
//!
//! The CLI runner's `--cache` persists the lowered [`grindlang::Program`] to disk and reloads
//! it instead of re-running the front end. These tests pin that contract at the library level:
//! a `Program` serialized through serde and deserialized back must (a) be byte-for-byte stable
//! across a second round-trip and (b) JIT-compile to code that produces the *same* result as the
//! original — i.e. nothing semantic is lost in serialization.
//!
//! Gated on `all(feature = "serde", feature = "jit")`; compiles to zero tests otherwise.
#![cfg(all(feature = "serde", feature = "jit"))]

use grindlang::codegen::JitModule;
use grindlang::{Program, TypeConfig};

/// Programs exercising scalars, control flow, arrays/tables, builtins, and closures —
/// each paired with its entry export and the expected `Display` of the result.
fn cases() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "recursion",
            "function fib(n)\n  if n < 2 then return n end\n  return fib(n-1) + fib(n-2)\nend\n\
             function main() return fib(12) end",
            "144",
        ),
        (
            "loop",
            "function main()\n  local s = 0\n  local i = 1\n  while i <= 5 do s = s + i*i i = i + 1 end\n  return s\nend",
            "55",
        ),
        (
            "builtins",
            "function main() return math.floor(3.7) + string.len(\"abcd\") end",
            "7",
        ),
        (
            "array",
            "function main()\n  local out = { 1, 2 }\n  out[#out + 1] = 3\n  return out\nend",
            "[1, 2, 3]",
        ),
        (
            "closure",
            "function make(n) return function(x) return x + n end end\n\
             function main() local add = make(10) return add(5) end",
            "15",
        ),
    ]
}

fn compile(src: &str) -> Program {
    grindlang::compile(src, &TypeConfig::default()).expect("front end should accept the program")
}

fn run_main(program: &Program) -> String {
    let mut jit = JitModule::compile(program).expect("jit compile");
    jit.call("main", Vec::new()).expect("call main").to_string()
}

#[test]
fn program_survives_serde_round_trip() {
    for (label, src, expected) in cases() {
        let program = compile(src);

        // The original compiles and runs to the expected value (sanity).
        assert_eq!(run_main(&program), expected, "[{label}] direct run");

        // Round-trip the IR through JSON (the cache's on-disk format).
        let json = serde_json::to_string(&program).expect("serialize Program");
        let restored: Program = serde_json::from_str(&json).expect("deserialize Program");

        // The reloaded IR re-verifies and JIT-runs to the *same* result.
        grindlang::ir::verify(&restored).expect("restored IR verifies");
        assert_eq!(
            run_main(&restored),
            expected,
            "[{label}] run after round-trip",
        );
    }
}

#[test]
fn serialization_is_stable() {
    // Serializing, deserializing, and reserializing yields identical bytes — the property a
    // source-hash-keyed cache relies on (no nondeterministic field ordering).
    for (label, src, _) in cases() {
        let program = compile(src);
        let once = serde_json::to_string(&program).expect("serialize");
        let restored: Program = serde_json::from_str(&once).expect("deserialize");
        let twice = serde_json::to_string(&restored).expect("reserialize");
        assert_eq!(once, twice, "[{label}] serialization is deterministic");
    }
}
