//! Embedding a Ruby-surface script that uses DSL keywords (`SPEC-RUBY.md` §5).
//!
//! Run with: `cargo run --example embed_ruby --features ruby`
//!
//! The host declares two keywords: `on`, whose blocks handle game events through a generated
//! `on_event` function, and `node`, which builds a small dialog tree. The host walks the tree,
//! running each choice's condition to decide what to offer, and its effect when it is picked.

#[cfg(all(feature = "jit", feature = "ruby"))]
fn main() {
    use std::collections::BTreeMap;

    use grindlang::api::{Engine, Module};
    use grindlang::ruby::{RubyConfig, parse_module};
    use grindlang::{Type, Value};

    let mut engine = Engine::new();
    let mut schema = BTreeMap::new();
    schema.insert("hp".to_string(), Type::Number);
    schema.insert("gold".to_string(), Type::Number);
    engine.declare_memory("mem", Type::Record(schema));

    // `@name` is a field of `mem`. Each `on :id do |n| … end` adds a handler to `ON`, and
    // `on_event(id, n)` runs the one for `id`. Each `node :id do … end` adds a record to `NODES`:
    // its `say` lines, and a record for each `choice`, whose `goto:`, `if:`, and block may be
    // left out, giving `nil`.
    let mut ruby = RubyConfig::new().ivar_memory("mem");
    ruby.handler("on", "ON")
        .block(&[Type::Number], Type::Unit)
        .dispatch("on_event");
    ruby.data("node", "NODES")
        .list("say", "lines", Type::String)
        .child("choice", "choices", |c| {
            c.positional("text", Type::String)
                .keyword("goto", "next", Type::String)
                .optional()
                .keyword("if", "cond", Type::function(vec![], Type::Bool))
                .optional()
                .block("effect", &[], Type::Unit)
                .optional()
        });

    let src = r#"
MAX_HP = 100

on :hit do |dmg|
  @hp -= dmg
  @hp = 0 if @hp < 0
end

on :heal do |amount|
  @hp = (@hp + amount).clamp(0, MAX_HP)
end

node :shop do
  say "Welcome! What'll it be?"
  choice "Buy a potion (5 gold)", if: -> { @gold >= 5 }, goto: "shop" do
    @gold -= 5
    @hp = (@hp + 25).clamp(0, MAX_HP)
  end
  choice "Ask for credit", if: -> { @gold < 5 }, goto: "credit"
  choice "Leave"
end

node :credit do
  say "We don't do that here."
  choice "Back", goto: "shop"
end
"#;
    let module = parse_module(src, &ruby, &engine.type_config())
        .unwrap_or_else(|d| panic!("{}", d.render(src)));
    let mut game = engine.compile_module(&module).expect("compile");
    let mut mem = BTreeMap::new();
    mem.insert("hp".to_string(), Value::Number(60.0));
    mem.insert("gold".to_string(), Value::Number(7.0));
    game.set_memory("mem", Value::table(mem));

    // --- 1. Events -----------------------------------------------------------------------
    for (event, n) in [("hit", 45.0), ("heal", 10.0), ("unknown", 99.0)] {
        game.call(
            "on_event",
            vec![Value::string(event.to_string()), Value::Number(n)],
        )
        .expect("on_event");
        println!("{event}({n}) -> {}", game.memory("mem").unwrap());
    }

    // --- 2. A dialog -----------------------------------------------------------------------
    //
    // The choices whose condition holds, as (text, record).
    fn offered(game: &mut Module, node: &Value) -> Vec<(String, Value)> {
        let choices = node.field("choices").and_then(|c| c.as_array()).unwrap();
        let mut out = Vec::new();
        for choice in choices {
            let open = match choice.field("cond") {
                Some(Value::Nil) | None => true,
                Some(cond) => game.call_value(cond, vec![]).unwrap().as_bool().unwrap(),
            };
            if open {
                out.push((choice.field("text").unwrap().as_string().unwrap(), choice));
            }
        }
        out
    }

    let nodes = game.call("NODES", vec![]).expect("NODES");
    let mut at = "shop".to_string();
    // The player picks the first choice offered, three times.
    let mut picked = Vec::new();
    for _ in 0..3 {
        let node = nodes.field(&at).unwrap();
        for line in node.field("lines").and_then(|l| l.as_array()).unwrap() {
            println!("[{at}] {line}");
        }
        let choices = offered(&mut game, &node);
        let (text, choice) = choices.into_iter().next().unwrap();
        println!("  > {text}");
        if let Some(effect @ Value::Closure(_)) = choice.field("effect") {
            game.call_value(effect, vec![]).unwrap();
        }
        picked.push(text);
        match choice.field("next").and_then(|n| n.as_string()) {
            Some(next) => at = next,
            None => break,
        }
    }
    println!("{}", game.memory("mem").unwrap());

    assert_eq!(picked, ["Buy a potion (5 gold)", "Ask for credit", "Back"]);
    assert_eq!(
        game.memory("mem").unwrap().to_string(),
        "{gold = 2, hp = 50}"
    );
    println!("\nkeywords work \u{2713}");
}

#[cfg(not(all(feature = "jit", feature = "ruby")))]
fn main() {
    eprintln!(
        "this example requires the `jit` and `ruby` features: \
         cargo run --example embed_ruby --features ruby"
    );
}
