//! Golden snapshots (`insta`, `PLAN.md` Phase 9) for the front end's user-facing output:
//! rendered diagnostics for rejected constructs and type errors, and inferred module export
//! signatures. These pin the *exact* text the author sees, so a regression in a message,
//! span, or caret placement shows up as a reviewable diff.
//!
//! Front-end only (parse/analyze) — no backend feature required, so they run under the
//! default `cargo test`. Update intentionally-changed snapshots with `cargo insta review`
//! (or `INSTA_UPDATE=always cargo test --test snapshots`).

use std::collections::BTreeMap;

use grindlang::{Type, TypeConfig};

fn read_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// Rendered parser diagnostics (or a marker if the source unexpectedly parsed).
fn parse_diag(src: &str) -> String {
    match grindlang::parse(src) {
        Ok(_) => "(parsed with no diagnostics)".to_string(),
        Err(d) => d.render(src),
    }
}

/// Rendered front-end diagnostics through type-checking.
fn analyze_diag(src: &str) -> String {
    match grindlang::analyze(src, &TypeConfig::default()) {
        Ok(_) => "(type-checked with no diagnostics)".to_string(),
        Err(d) => d.render(src),
    }
}

/// The inferred export signature, one `name: type` per line (BTreeMap → stable order).
fn signature(src: &str, cfg: &TypeConfig) -> String {
    let (_m, _r, info) = grindlang::analyze(src, cfg).expect("analyze");
    info.exports
        .iter()
        .map(|(name, ty)| format!("{name}: {ty}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- rejected constructs (parser diagnostics) -------------------------------

#[test]
fn snapshot_rejected_constructs() {
    insta::assert_snapshot!("reject_top_level_local", parse_diag("local g = 1"));
    insta::assert_snapshot!("reject_top_level_statement", parse_diag("doThing()"));
    insta::assert_snapshot!(
        "reject_repeat_until",
        parse_diag("function f() repeat until true end")
    );
    insta::assert_snapshot!("reject_varargs", parse_diag("function f(...) end"));
    insta::assert_snapshot!("reject_labels", parse_diag("function f() ::lbl:: end"));
    insta::assert_snapshot!(
        "reject_bad_iterator",
        parse_diag("function f(t) for x in each(t) do end end")
    );
}

// ---- type errors (checker diagnostics) --------------------------------------

#[test]
fn snapshot_type_errors() {
    insta::assert_snapshot!(
        "type_error_inconsistent_returns",
        analyze_diag("function f(b)\n  if b then return 1 end\n  return \"x\"\nend")
    );
    insta::assert_snapshot!(
        "type_error_arith_on_string",
        analyze_diag("function f()\n  return 1 + \"a\"\nend")
    );
    insta::assert_snapshot!(
        "type_error_nil_arithmetic",
        analyze_diag("function f()\n  local x = nil\n  return x + 1\nend")
    );
}

// ---- inferred export signatures ---------------------------------------------

#[test]
fn snapshot_export_signatures() {
    insta::assert_snapshot!(
        "signature_stat_calc",
        signature(&read_fixture("stat_calc.lua"), &TypeConfig::default())
    );

    insta::assert_snapshot!(
        "signature_inline",
        signature(
            "K = 7\nfunction double(x) return x * 2 end\nfunction is_pos(n) return n > 0 end",
            &TypeConfig::default()
        )
    );

    // A module that reads typed host memory (record), to pin how memory-shaped inference and
    // a curated export table render.
    let mut rec = BTreeMap::new();
    rec.insert("reputation".to_string(), Type::Number);
    rec.insert("met_elder".to_string(), Type::Bool);
    let mut memory = BTreeMap::new();
    memory.insert("mem".to_string(), Type::Record(rec));
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory,
    };
    insta::assert_snapshot!(
        "signature_dialog_decision",
        signature(&read_fixture("dialog_decision.lua"), &cfg)
    );
}
