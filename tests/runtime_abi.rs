//! Integration test for the Phase 6 runtime / host ABI: it composes with the real front end.
//!
//! These assert that the ABI layer (`runtime`) derives correct representations and calling
//! conventions from *inferred* types produced by `analyze`, i.e. that the runtime contract
//! lines up with what the checker actually infers — not hand-written types.

use std::collections::BTreeMap;

use grindlang::{Arena, FnAbi, MemorySchema, Repr, Slot, Type, TypeConfig};

/// `FnAbi` of an inferred function signature matches its representation classes.
#[test]
fn fn_abi_from_inferred_signature() {
    // `hits` returns a bool; params are a number and a string (pinned by usage).
    let src = "function hits(roll, label)\n\
                 return roll > 10 and string.len(label) > 0\n\
               end";
    let (_m, _r, info) = grindlang::analyze(src, &TypeConfig::default()).expect("analyze");
    let sig = &info.functions["hits"];
    let abi = FnAbi::of(sig);
    assert_eq!(abi.params, vec![Repr::Number, Repr::Ptr]); // number, string
    assert_eq!(abi.ret, Repr::Bool);
    assert_eq!(abi.cranelift_signature(), "(f64, i64) -> i8");
    // sanity: the inferred surface type agrees.
    assert_eq!(
        info.exports["hits"].to_string(),
        "fn(number, string) -> bool"
    );
}

/// A `()`-returning function lowers to a no-return ABI.
#[test]
fn fn_abi_unit_return() {
    // `touch` writes memory and returns nothing.
    let mut mem = BTreeMap::new();
    let mut rec = BTreeMap::new();
    rec.insert("n".to_string(), Type::Number);
    mem.insert("mem".to_string(), Type::Record(rec));
    let cfg = TypeConfig {
        host_functions: BTreeMap::new(),
        memory: mem,
    };
    let src = "function touch(x)\n  mem.n = x\nend";
    let (_m, _r, info) = grindlang::analyze(src, &cfg).expect("analyze");
    let abi = FnAbi::of(&info.functions["touch"]);
    assert_eq!(abi.ret, Repr::Unit);
    assert_eq!(abi.cranelift_signature(), "(f64)");
}

/// A record-typed memory binding becomes a typed accessor schema.
#[test]
fn memory_schema_from_record_binding() {
    let mut rec = BTreeMap::new();
    rec.insert("gold".to_string(), Type::Number);
    rec.insert("alive".to_string(), Type::Bool);
    rec.insert("name".to_string(), Type::String);
    let schema = MemorySchema::of(&Type::Record(rec)).expect("record has a schema");

    assert_eq!(schema.field("gold").unwrap().repr, Repr::Number);
    assert_eq!(schema.field("alive").unwrap().repr, Repr::Bool);
    assert_eq!(schema.field("name").unwrap().repr, Repr::Ptr);
    assert!(schema.field("nope").is_none());
}

/// The arena allocates and resets across "invocations" while retaining capacity.
#[test]
fn arena_round_trips_across_invocations() {
    let mut arena = Arena::with_capacity(128);
    let cap = arena.capacity();

    // Invocation 1: stash a string's bytes.
    let r = arena.alloc_slice(b"grind", 1);
    assert_eq!(arena.bytes(r), b"grind");
    assert!(arena.bytes_used() >= 5);

    // End of invocation: reset.
    arena.reset();
    assert_eq!(arena.bytes_used(), 0);
    assert_eq!(arena.capacity(), cap);

    // Invocation 2 reuses the buffer from offset 0.
    let r2 = arena.alloc_slice(b"shell", 1);
    assert_eq!(r2.offset, 0);
    assert_eq!(arena.bytes(r2), b"shell");
}

/// Slots round-trip every scalar representation the JIT will pass in registers.
#[test]
fn slots_carry_each_scalar_repr() {
    assert_eq!(Slot::from_number(12.5).as_number(), 12.5);
    assert!(Slot::from_bool(true).as_bool());
    assert!(Slot::NULL.is_null());
    assert_eq!(Slot::from_handle(7).handle(), 7);
}
