# grindlang

> The calc-script language: the green bar, the feature checks, the benches, and the CLI

Cargo commands that build the crate take `-f --forge` to run on the forge's `dev` account against this working tree. Set `DEV_SLOT` to a slot of your own when another session may be building there.

## check

> The green bar: build, every feature's tests, clippy with warnings denied

`cargo test` with only the default `jit` feature compiles the differential, fuzz and property suites to zero tests, which is why this runs `--all-features`.

**FLAGS**
- FORGE
  - flags: `-f --forge`
  - type: boolean
  - description: Run on the forge instead of this machine

```bash
set -e
bar='cargo build && timeout 300 cargo test --all-features && cargo clippy --all-targets --all-features -- -D warnings'
if [ "$FORGE" = true ]; then
  exec ../server-configs/build/devbuild.sh grindlang -- bash -c "$bar"
fi
cargo build
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

## features

> Check each feature combination compiles on its own; run before committing

**FLAGS**
- FORGE
  - flags: `-f --forge`
  - type: boolean
  - description: Run on the forge instead of this machine

```bash
set -e
checks='cargo check --no-default-features && cargo check --no-default-features --features interp && cargo check --no-default-features --features ruby && cargo check --features serde'
if [ "$FORGE" = true ]; then
  exec ../server-configs/build/devbuild.sh grindlang -- bash -c "$checks"
fi
bash -c "$checks"
```

## bench [FEATURE]

> Run the benches, with one extra feature if named: interp or ruby

```bash
if [ -n "$FEATURE" ]; then cargo bench --features "$FEATURE"; else cargo bench; fi
```

## run (FILE) [...ARGS]

> Run a script through the CLI; its flags (--call, --cache, --no-cache) go after `--`

```bash
cargo run --features serde --bin grindlang -- "$FILE" "${ARGS[@]}"
```

## fmt

> Format the crate

```bash
cargo fmt
```
