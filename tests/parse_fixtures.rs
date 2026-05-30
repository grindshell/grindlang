//! Integration test: the spec's worked-example modules parse, and a representative set
//! of rejected constructs fail with the expected diagnostic codes.

use grindlang::ast::TopDecl;

fn parse_fixture(name: &str) -> grindlang::Module {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    grindlang::parse(&src).unwrap_or_else(|d| panic!("parse {name} failed:\n{}", d.render(&src)))
}

#[test]
fn stat_calc_fixture_parses() {
    let m = parse_fixture("stat_calc.lua");
    // ARMOR_K const + mitigated + lethal.
    assert_eq!(m.decls.len(), 3);
    assert!(matches!(m.decls[0], TopDecl::Const(_)));
    assert!(matches!(m.decls[1], TopDecl::Function(_)));
    assert!(matches!(m.decls[2], TopDecl::Function(_)));
    assert!(m.export.is_none());
}

#[test]
fn dialog_decision_fixture_parses() {
    let m = parse_fixture("dialog_decision.lua");
    assert_eq!(m.decls.len(), 2);
    let export = m.export.expect("curated export table");
    assert_eq!(export.node.len(), 2);
}

#[test]
fn rejected_constructs_report_expected_codes() {
    let cases: &[(&str, &str)] = &[
        ("local g = 1", "E0103"),                               // top-level local
        ("doThing()", "E0104"),                                 // top-level statement
        ("function f() repeat until true end", "E0201"),        // repeat/until
        ("function f(...) end", "E0200"),                       // varargs
        ("function f() ::lbl:: end", "E0202"),                  // labels/goto
        ("function f(t) for x in each(t) do end end", "E0203"), // bad iterator
    ];
    for (src, code) in cases {
        let err = grindlang::parse(src).unwrap_err();
        assert!(
            err.0.iter().any(|d| d.code == *code),
            "src {src:?} expected code {code}, got {:?}",
            err.0.iter().map(|d| d.code).collect::<Vec<_>>()
        );
    }
}
