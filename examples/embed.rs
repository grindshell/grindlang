//! A worked example of embedding Grindlang (`PLAN.md` Phase 8).
//!
//! Run with: `cargo run --example embed --features jit`
//!
//! Demonstrates the two things Grindlang exists for — a **stat calculation** and a
//! **dialog-tree decision** — driven from Rust through the embedding API: registering a host
//! function, declaring and binding host memory, and calling typed exports.

#[cfg(feature = "jit")]
fn main() {
    use std::collections::BTreeMap;

    use grindlang::Type;
    use grindlang::Value;
    use grindlang::api::Engine;

    // --- 1. A stat calculation -------------------------------------------------
    //
    // Mitigated damage with a host-provided difficulty multiplier. The host function's
    // signature is inferred from the closure.
    let mut engine = Engine::new();
    engine.register_fn("difficulty", || 1.25_f64);

    let stat_src = "\
        function mitigated(attack, defense)\n\
          local raw = attack * difficulty()\n\
          local dmg = raw - defense * 0.5\n\
          if dmg < 0 then return 0 end\n\
          return math.floor(dmg)\n\
        end";
    let mut stats = engine.compile(stat_src).expect("compile stat module");
    let dmg: f64 = stats.call_typed("mitigated", (120.0, 80.0)).unwrap();
    println!("mitigated(120, 80) = {dmg}"); // 120*1.25 - 40 = 110

    // --- 2. A dialog-tree decision against host memory -------------------------
    //
    // The script reads persistent reputation from host memory and returns the dialog node id
    // to show, plus the menu of available choices.
    let mut dialog_engine = Engine::new();
    let mut mem_schema = BTreeMap::new();
    mem_schema.insert("reputation".to_string(), Type::Number);
    dialog_engine.declare_memory("mem", Type::Record(mem_schema));

    let dialog_src = "\
        function greeting()\n\
          if mem.reputation >= 50 then return \"elder_warm\" end\n\
          if mem.reputation >= 0 then return \"elder_neutral\" end\n\
          return \"elder_cold\"\n\
        end\n\
        function choices()\n\
          local out = { \"ask_quest\", \"leave\" }\n\
          if mem.reputation >= 50 then\n\
            out[#out + 1] = \"ask_favor\"\n\
          end\n\
          return out\n\
        end";
    let mut dialog = dialog_engine
        .compile(dialog_src)
        .expect("compile dialog module");

    let mut mem = BTreeMap::new();
    mem.insert("reputation".to_string(), Value::Number(62.0));
    dialog.set_memory("mem", Value::table(mem));

    let node: String = dialog.call_typed("greeting", ()).unwrap();
    let choices: Vec<String> = dialog.call_typed("choices", ()).unwrap();
    println!("dialog node: {node}");
    println!("choices: {choices:?}");

    assert_eq!(node, "elder_warm");
    assert_eq!(choices, vec!["ask_quest", "leave", "ask_favor"]);
    println!("\nembedding works \u{2713}");
}

#[cfg(not(feature = "jit"))]
fn main() {
    eprintln!("this example requires the `jit` feature: cargo run --example embed --features jit");
}
