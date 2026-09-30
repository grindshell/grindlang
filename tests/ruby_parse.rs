//! The Ruby surface's lexer and parser (`PLAN.md` Phase 10, step R1): the fixtures in
//! `tests/fixtures/ruby` parse, and every construct `SPEC-RUBY.md` §6 rejects that shows in the
//! syntax fails with its diagnostic. The rendered diagnostics of each §6 group are pinned in one
//! snapshot, so a change to any message, span, or caret shows up as a reviewable diff.
//!
//! Rejections that need more than the syntax — a block passed to your own `def`, `@name` with no
//! memory designated for it, a `def` reading a top-level local, and the "Semantics" group — are
//! the translator's (R2 and later).

#![cfg(feature = "ruby")]

use grindlang::ruby::ast::{Def, ExprKind, Item, StmtKind, Visibility};
use grindlang::ruby::parse;

fn parse_fixture(name: &str) -> grindlang::ruby::ast::Program {
    let path = format!("{}/tests/fixtures/ruby/{name}", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    parse(&src).unwrap_or_else(|d| panic!("parse {name} failed:\n{}", d.render(&src)))
}

fn defs(items: &[Item]) -> Vec<&Def> {
    items
        .iter()
        .filter_map(|item| match item {
            Item::Def(d) => Some(d),
            _ => None,
        })
        .collect()
}

#[test]
fn the_worked_example_parses() {
    let program = parse_fixture("stat_calc.rb");
    let defs = defs(&program.items);
    let names: Vec<_> = defs.iter().map(|d| d.name.node.as_str()).collect();
    assert_eq!(names, ["curve", "xp_for", "best_name"]);
    assert_eq!(defs[0].visibility, Some(Visibility::Private));
    assert_eq!(defs[1].visibility, None);
    // The annotation sits directly above `best_name`.
    assert_eq!(defs[2].docs.len(), 1);
    assert!(defs[2].docs[0].text.contains("@param items"));
    // Its body ends in a ternary, which is what it returns.
    let last = defs[2].body.stmts.last().unwrap();
    assert!(matches!(&last.kind, StmtKind::Expr(e) if matches!(e.kind, ExprKind::Ternary { .. })));
}

#[test]
fn the_dsl_examples_parse() {
    for fixture in ["handlers.rb", "dialog.rb"] {
        let program = parse_fixture(fixture);
        for item in &program.items {
            // Every keyword is a call without parentheses with a `do` block.
            let Item::Stmt(s) = item else { continue };
            let StmtKind::Expr(e) = &s.kind else { continue };
            if let ExprKind::Call { block, .. } = &e.kind {
                assert!(block.is_some(), "{fixture}: a keyword without its block");
            }
        }
    }
}

#[test]
fn the_grammar_tour_parses() {
    let program = parse_fixture("tour.rb");
    let defs = defs(&program.items);
    assert_eq!(defs.len(), 8);
    assert!(
        program
            .items
            .iter()
            .any(|i| matches!(i, Item::Visibility(v) if v.node == Visibility::Private))
    );
    assert_eq!(defs[0].name.node, "damage");
    assert_eq!(defs[0].docs.len(), 3);
    assert_eq!(defs[1].visibility, Some(Visibility::Public));
}

/// The §6 groups, each with a source for every construct it lists and the code it gets.
const REJECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "object_model",
        &[
            ("class Hero\nend", "E0520"),
            ("module Stats\nend", "E0520"),
            ("x = self", "E0520"),
            ("def f\n  super\nend", "E0520"),
            ("Point = Struct.new(:x, :y)", "E0520"),
            ("h = Hero.new", "E0520"),
            ("attr_accessor :hp", "E0520"),
            ("define_method(:f) { 1 }", "E0520"),
            ("def method_missing(name)\nend", "E0520"),
            ("x.send(:hp)", "E0520"),
            ("x.public_send(:hp)", "E0520"),
            ("v = instance_variable_get(:@hp)", "E0520"),
            ("eval(\"1 + 1\")", "E0520"),
            ("x.instance_eval { 1 }", "E0520"),
        ],
    ),
    (
        "blocks_and_procs",
        &[
            ("def each\n  yield 1\nend", "E0521"),
            ("def each(&block)\nend", "E0521"),
            ("xs.each(&blk)", "E0521"),
            ("xs.map &:to_s", "E0521"),
            ("def f\n  block_given?\nend", "E0521"),
            ("p = proc { |x| x }", "E0521"),
            ("p = Proc.new { |x| x }", "E0521"),
            ("xs.each { |x; y| y = x }", "E0521"),
        ],
    ),
    (
        "exceptions",
        &[
            ("begin\n  f()\nrescue\n  g()\nend", "E0522"),
            ("def f\n  g()\nrescue\n  h()\nend", "E0522"),
            ("def f\n  g()\nensure\n  h()\nend", "E0522"),
            ("retry", "E0522"),
            ("raise \"boom\"", "E0522"),
            ("x = f() rescue 0", "E0522"),
        ],
    ),
    (
        "parameters",
        &[
            ("def f(a = 1)\nend", "E0523"),
            ("def f(*args)\nend", "E0523"),
            ("def f(**opts)\nend", "E0523"),
            ("def f(a:, b: 2)\nend", "E0523"),
            ("xs.each { |(a, b)| a }", "E0523"),
            ("f(*xs)", "E0523"),
        ],
    ),
    (
        "variables",
        &[("$score = 1", "E0524"), ("@@count = 0", "E0524")],
    ),
    (
        "literals",
        &[
            ("s = <<~EOS\n  text\nEOS\n", "E0525"),
            ("xs = %w[a b c]", "E0525"),
            ("xs = %i[a b]", "E0525"),
            ("s = %q(text)", "E0525"),
            ("s = %Q(text)", "E0525"),
            ("c = ?a", "E0525"),
            ("ok = /ab+/", "E0525"),
            ("out = `ls`", "E0525"),
            ("n = 0b1010", "E0525"),
            ("n = 0o17", "E0525"),
            ("n = 017", "E0525"),
            ("n = 3r", "E0525"),
            ("n = 2i", "E0525"),
            ("s = :\"hit points\"", "E0525"),
            ("h = { \"hp\": 1 }", "E0525"),
            ("s = \"hp: #@hp\"", "E0525"),
            ("x = xs[-1]", "E0525"),
            ("xs[-2] = 0", "E0525"),
        ],
    ),
    (
        "syntax",
        &[
            ("for x in xs do\nend", "E0526"),
            ("=begin\ncomment\n=end\nx = 1", "E0526"),
            ("x = 1\n__END__\ndata", "E0526"),
            ("BEGIN { x = 1 }", "E0526"),
            ("END { x = 1 }", "E0526"),
            ("alias hit strike", "E0526"),
            ("undef hit", "E0526"),
            ("ok = defined?(x)", "E0526"),
            ("n = user&.name", "E0526"),
            ("n = Math::PI", "E0526"),
            ("c = a <=> b", "E0526"),
            ("ok = a === b", "E0526"),
            ("ok = s =~ pattern", "E0526"),
            ("n = a & b", "E0526"),
            ("n = a | b", "E0526"),
            ("n = a ^ b", "E0526"),
            ("n = ~a", "E0526"),
            ("n = a >> 1", "E0526"),
            ("n <<= 1", "E0526"),
            ("begin\n  x += 1\nend while x < 3", "E0522"),
            ("case x\nin [a, b] then a\nend", "E0526"),
            ("redo", "E0526"),
        ],
    ),
    (
        "calls_without_parentheses",
        &[
            ("x = foo 1", "E0527"),
            ("say format \"%d\", 1", "E0527"),
            ("mem.add_item :sword", "E0527"),
            ("foo -1", "E0527"),
            ("foo [1, 2]", "E0527"),
            ("foo (1)", "E0527"),
            ("foo *xs", "E0527"),
            ("foo &blk", "E0521"),
            ("x = c ? a :b", "E0527"),
        ],
    ),
    (
        "structure",
        &[
            ("end", "E0513"),
            ("def f\n  def g\n  end\nend", "E0515"),
            ("if debug\n  private\nend", "E0515"),
            ("private :helper", "E0515"),
            ("def alive?\nend", "E0516"),
            ("def reset!\nend", "E0516"),
            ("xs.each do |x|\n  next x\nend", "E0517"),
            ("loop do\n  break 1\nend", "E0517"),
            ("a = b = 0", "E0512"),
            ("if (x = f()) then end", "E0512"),
            ("f() = 1", "E0512"),
            ("x = a or b", "E0528"),
            ("x = a and b", "E0528"),
            ("say a or b", "E0528"),
            ("x = not a", "E0511"),
            ("foo { hp: 1 }", "E0510"),
            ("def f x\nend", "E0510"),
            ("f = ->x { x }", "E0510"),
            ("unless a\nelsif b\nend", "E0510"),
        ],
    ),
];

#[test]
fn rejected_constructs_report_their_codes() {
    for (group, cases) in REJECTIONS {
        for (src, code) in *cases {
            let err = match parse(src) {
                Ok(_) => panic!("{group}: {src:?} parsed; expected {code}"),
                Err(d) => d,
            };
            assert_eq!(
                err.0[0].code,
                *code,
                "{group}: {src:?} gave\n{}",
                err.render(src)
            );
        }
    }
}

#[test]
fn snapshot_rejected_constructs() {
    for (group, cases) in REJECTIONS {
        let rendered = cases
            .iter()
            .map(|(src, _)| match parse(src) {
                Ok(_) => format!("{src}\n(parsed with no diagnostics)"),
                Err(d) => d.render(src),
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        insta::assert_snapshot!(format!("ruby_reject_{group}"), rendered);
    }
}
