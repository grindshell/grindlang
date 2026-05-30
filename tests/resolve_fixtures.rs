//! Integration test: the spec's worked-example modules resolve cleanly under a realistic
//! host configuration, and representative constraint violations report the expected codes.

use grindlang::ResolveConfig;

fn read_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

#[test]
fn stat_calc_fixture_resolves() {
    let src = read_fixture("stat_calc.lua");
    let (_m, res) = grindlang::check(&src, &ResolveConfig::default())
        .unwrap_or_else(|d| panic!("resolve failed:\n{}", d.render(&src)));
    // `attack`, `armor`, `hp` params all show up as symbols.
    assert!(res.symbols.iter().any(|s| s.name == "attack"));
    assert!(res.symbols.iter().any(|s| s.name == "hp"));
}

#[test]
fn dialog_decision_fixture_resolves_with_memory() {
    let src = read_fixture("dialog_decision.lua");
    let cfg = ResolveConfig::with_memory("mem");
    grindlang::check(&src, &cfg).unwrap_or_else(|d| panic!("resolve failed:\n{}", d.render(&src)));
}

#[test]
fn dialog_decision_fixture_fails_without_memory_binding() {
    let src = read_fixture("dialog_decision.lua");
    // Without `mem` configured, every `mem` reference is an unknown name.
    let err = grindlang::check(&src, &ResolveConfig::default()).unwrap_err();
    assert!(err.0.iter().any(|d| d.code == "E0300"));
}

#[test]
fn constraint_violations_report_expected_codes() {
    let cfg = ResolveConfig::default();
    let cases: &[(&str, &str)] = &[
        ("function f() return ghost end", "E0300"), // free global
        ("K = 1\nfunction f() K = 2 end", "E0302"), // assign to const
        ("function f() break end", "E0306"),        // break outside loop
        ("X = f()", "E0303"),                       // non-constant const RHS
        ("function f() end\nfunction f() end", "E0305"), // duplicate decl
        ("function math() end", "E0304"),           // shadow builtin
        ("function f() return ipairs end", "E0301"), // ipairs as value
    ];
    for (src, code) in cases {
        let err = grindlang::check(src, &cfg).unwrap_err();
        assert!(
            err.0.iter().any(|d| d.code == *code),
            "src {src:?} expected {code}, got {:?}",
            err.0.iter().map(|d| d.code).collect::<Vec<_>>()
        );
    }
}
