//! `grindlang` — a command-line runner for Grindlang scripts.
//!
//! A Grindlang script is a **module**, not a standalone program (see `SPEC.md`): it evaluates
//! to a table of exported functions and constants. This runner picks one export as an entry
//! point — `main` by default, or another name via `--call NAME` — compiles the script with the
//! cranelift JIT, runs its top-level code, invokes that export with no arguments, and prints
//! the value it returns.
//!
//! Because the runner registers no host functions and binds no memory, a script run this way
//! may use only the in-language builtins (`math.*` / `string.*`); anything needing host
//! capabilities must be driven through the embedding API (`grindlang::api`) instead.
//!
//! A script is Lua-syntax source (`SPEC.md`), or, in a `.rb` file, Ruby-syntax source
//! (`SPEC-RUBY.md`), which needs a `grindlang` built with the `ruby` feature; `--surface`
//! names the syntax whatever the extension. The runner declares no DSL keywords and designates
//! no memory for `@name`. `--print-lua` prints the module as Lua, which for a Ruby script is
//! what it translated into, without checking or running it.
//!
//! ```text
//! grindlang script.lua             # run; reuse a fresh IR cache or compile + write one
//! grindlang --call total sums.lua  # run the `total` export instead of `main`
//! grindlang --no-cache script.lua  # run without reading or writing the cache
//! grindlang --cache script.lua     # compile + write the IR cache, WITHOUT running
//! grindlang script.rb              # a Ruby-surface script
//! grindlang --print-lua script.rb  # print what script.rb translates into, as Lua
//! grindlang --help                 # usage
//! ```
//!
//! ## Disk cache (pyc-style)
//!
//! The cranelift JIT compiles into process memory and cannot be persisted to disk, so there is
//! no native-code cache. What *is* cached is the **lowered IR** (`ir::Program`, the front end's
//! output). By default — like Python's `*.pyc` — a normal run reads `<FILE>.glir` when it is
//! present and current (skipping lex/parse/resolve/type-check/lower, still JIT-compiling on
//! load), and otherwise compiles the source and writes the cache. The cache is keyed by a hash
//! of the source, the surface it was read in, and an identity of this executable's build (its
//! version, size, and modification time), so an edited script, a script read in the other
//! surface, or a rebuilt `grindlang` invalidates it automatically.
//!
//! Two flags adjust this:
//! * `--cache` writes the IR cache **without running** the script (a compile/pre-warm step).
//! * `--no-cache` runs the script but neither reads nor writes the cache.
//!
//! The cache requires the `serde` feature (it provides the IR's serialization and the on-disk
//! format), so this binary is built with `--features serde`.
//!
//! Exit status is `0` on success and `1` on any error (bad arguments, file I/O, a compile
//! diagnostic, a missing/unsuitable entry export, or a runtime error). Diagnostics and errors
//! are written to stderr; the entry export's return value is written to stdout.

use std::process::ExitCode;

use grindlang::codegen::JitModule;
use grindlang::ir::ExportTarget;
use grindlang::{CompileError, Program, Surface, TypeConfig, Value};

const USAGE: &str = "\
grindlang — run a Grindlang script by calling one of its exports.

USAGE:
    grindlang [OPTIONS] <FILE>

ARGS:
    <FILE>               Path to a Grindlang script: Lua-syntax source, or Ruby-syntax
                         source if its name ends in .rb.

OPTIONS:
    -c, --call NAME      Export to invoke as the entry point [default: main].
        --surface S      Read FILE as `lua` or `ruby` syntax, whatever its extension.
        --print-lua      Print the module as Lua (what a Ruby script translates into) and
                         exit, without checking or running it.
        --cache          Compile and write the IR cache (<FILE>.glir) WITHOUT running.
        --no-cache       Run the script but neither read nor write the IR cache.
        --cache-file P   Use path P for the IR cache instead of <FILE>.glir.
    -h, --help           Print this help and exit.

By default the runner caches the lowered IR like Python's *.pyc files: it reads <FILE>.glir
when it is present and current (skipping the front end), otherwise compiles and writes it,
then runs the chosen export with no arguments and prints its return value. The cache stores
IR, not native code, and still JIT-compiles on load.

Ruby-syntax scripts need a grindlang built with the `ruby` feature.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("grindlang: {err}");
            ExitCode::FAILURE
        }
    }
}

/// What to do with the on-disk IR cache for this run.
#[derive(Debug, PartialEq)]
enum CacheMode {
    /// Default (pyc-style): read a current cache if present, else compile and write one; run.
    ReadWrite,
    /// `--cache`: compile and write the cache, but do not run the script.
    WriteOnly,
    /// `--no-cache`: run the script without reading or writing the cache.
    Disabled,
}

/// Parsed command line.
#[derive(Debug)]
struct Args {
    file: String,
    entry: String,
    mode: CacheMode,
    /// Explicit cache path from `--cache-file`; `None` means the default `<file>.glir`.
    cache_path: Option<String>,
    /// The syntax `--surface` names; `None` means the one the file's extension implies.
    surface: Option<Surface>,
    /// `--print-lua`: print the module as Lua instead of running it.
    print_lua: bool,
}

impl Args {
    /// The cache file path to use for this run.
    fn cache_path(&self) -> String {
        self.cache_path
            .clone()
            .unwrap_or_else(|| format!("{}.glir", self.file))
    }

    /// The syntax the script is read in: the one `--surface` names, or Ruby's for a `.rb` file
    /// and Lua's for any other.
    fn surface(&self) -> Surface {
        self.surface.unwrap_or_else(|| {
            let ext = std::path::Path::new(&self.file).extension();
            if ext.is_some_and(|e| e.eq_ignore_ascii_case("rb")) {
                Surface::Ruby
            } else {
                Surface::Lua
            }
        })
    }
}

fn run() -> Result<(), String> {
    let args = match parse_args(std::env::args().skip(1))? {
        // `--help` was handled: print usage and stop, successfully.
        None => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(args) => args,
    };
    let surface = args.surface();

    let raw = std::fs::read_to_string(&args.file)
        .map_err(|e| format!("cannot read '{}': {e}", args.file))?;
    // Editors on Windows often save a UTF-8 BOM; strip a leading one so it doesn't reach the
    // lexer as stray characters.
    let src = raw.strip_prefix('\u{feff}').unwrap_or(&raw);

    if args.print_lua {
        let module = parse_source(src, &args.file, surface)?;
        print!("{}", grindlang::unparse::to_lua(&module));
        return Ok(());
    }

    match args.mode {
        // `--cache`: produce the cache artifact and stop — never run the script.
        CacheMode::WriteOnly => {
            let program = compile_source(src, &args.file, surface)?;
            let path = args.cache_path();
            cache::store(&path, src, surface, &program)
                .map_err(|e| format!("could not write cache '{path}': {e}"))?;
            eprintln!("grindlang: cached IR to '{path}'");
            Ok(())
        }
        // `--no-cache`: compile fresh and run, touching no cache file.
        CacheMode::Disabled => {
            let program = compile_source(src, &args.file, surface)?;
            execute(&program, &args.entry, &args.file)
        }
        // Default: reuse a current cache or compile + write one, then run.
        CacheMode::ReadWrite => {
            let path = args.cache_path();
            let program = match cache::load(&path, src, surface) {
                Some(program) => program,
                None => {
                    let program = compile_source(src, &args.file, surface)?;
                    // Writing the cache is best-effort: a failure (e.g. an unwritable directory)
                    // warns but must not stop the script from running.
                    if let Err(e) = cache::store(&path, src, surface, &program) {
                        eprintln!("grindlang: warning: could not write cache '{path}': {e}");
                    }
                    program
                }
            };
            execute(&program, &args.entry, &args.file)
        }
    }
}

/// Validate the entry export, JIT-compile, call it with no arguments, and print its result.
fn execute(program: &Program, entry: &str, file: &str) -> Result<(), String> {
    // The entry export must exist and be a function taking no arguments, since the runner calls
    // it with none. Report a precise error otherwise, listing what *is* runnable.
    validate_entry(program, entry)?;

    let mut jit =
        JitModule::compile(program).map_err(|e| format!("JIT-compiling '{file}': {e}"))?;
    // The call below would run the top-level code too; running it first reports its errors as
    // its own rather than as the entry's.
    jit.init()
        .map_err(|e| format!("top-level code in '{file}' raised: {e}"))?;
    let result = jit
        .call(entry, Vec::new())
        .map_err(|e| format!("'{entry}' raised: {e}"))?;

    // A `nil` return means the entry ran purely for effect — print nothing in that case so the
    // output is exactly the meaningful result.
    if !matches!(result, Value::Nil) {
        println!("{result}");
    }
    Ok(())
}

/// Parse `src`, read in `surface`, into the shared tree: for a Ruby script, translate it, with
/// no keywords and no memory designated for `@name`. Diagnostics come back rendered against
/// the source.
fn parse_source(src: &str, file: &str, surface: Surface) -> Result<grindlang::ast::Module, String> {
    let parsed = match surface {
        Surface::Lua => grindlang::parse(src),
        #[cfg(feature = "ruby")]
        Surface::Ruby => grindlang::ruby::parse_module(
            src,
            &grindlang::ruby::RubyConfig::new(),
            &TypeConfig::default(),
        ),
        #[cfg(not(feature = "ruby"))]
        Surface::Ruby => {
            return Err(format!(
                "'{file}' is a Ruby-syntax script, and this grindlang was built without the \
                 `ruby` feature; build it with `--features serde,ruby`."
            ));
        }
    };
    parsed.map_err(|d| format!("compiling '{file}':\n{}", d.render(src)))
}

/// Run the full front end (parse → resolve → type-check → lower → verify) on `src`. No host
/// functions or memory are configured, so scripts may use only the in-language builtins.
fn compile_source(src: &str, file: &str, surface: Surface) -> Result<Program, String> {
    let module = parse_source(src, file, surface)?;
    grindlang::compile_module(&module, &TypeConfig::default()).map_err(|e| match e {
        CompileError::Check(d) => format!("compiling '{file}':\n{}", d.render(src)),
        CompileError::Lower(e) => format!("compiling '{file}':\n{e}"),
    })
}

/// Check that `entry` names a runnable export: a function taking no arguments.
fn validate_entry(program: &Program, entry: &str) -> Result<(), String> {
    match program.exports.get(entry) {
        None => Err(missing_entry(program, entry)),
        Some(ExportTarget::Global(_)) => Err(format!(
            "entry export '{entry}' is a constant, not a function; nothing to run.",
        )),
        Some(ExportTarget::Function(fname)) => {
            let arity = program.functions.get(fname).map_or(0, |f| f.params.len());
            if arity != 0 {
                Err(format!(
                    "entry export '{entry}' takes {arity} argument(s); the runner calls it with \
                     none. Drive parameterized exports through the embedding API instead.",
                ))
            } else {
                Ok(())
            }
        }
    }
}

/// Build the "no such entry export" error, listing the zero-argument functions to guide the user.
fn missing_entry(program: &Program, entry: &str) -> String {
    let mut callable: Vec<&str> = program
        .exports
        .iter()
        .filter(|(_, target)| match target {
            ExportTarget::Function(fname) => program
                .functions
                .get(fname)
                .is_some_and(|f| f.params.is_empty()),
            ExportTarget::Global(_) => false,
        })
        .map(|(name, _)| name.as_str())
        .collect();
    callable.sort_unstable();

    if callable.is_empty() {
        format!("no export named '{entry}', and the module has no zero-argument function to run.")
    } else {
        format!(
            "no export named '{entry}'. Runnable exports: {}.",
            callable.join(", "),
        )
    }
}

/// The surface `--surface` names.
fn parse_surface(name: &str) -> Result<Surface, String> {
    match name {
        "lua" => Ok(Surface::Lua),
        "ruby" => Ok(Surface::Ruby),
        other => Err(format!(
            "unknown surface '{other}': expected 'lua' or 'ruby'"
        )),
    }
}

/// Parse the runner's arguments. Returns `Ok(None)` when `--help` was requested (the caller
/// prints usage and exits successfully) and `Ok(Some(args))` otherwise.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut file: Option<String> = None;
    let mut entry = String::from("main");
    let mut cache_path: Option<String> = None;
    let mut surface: Option<Surface> = None;
    let mut saw_cache = false;
    let mut saw_no_cache = false;
    let mut print_lua = false;
    let mut args = args;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-c" | "--call" => {
                entry = args
                    .next()
                    .ok_or_else(|| format!("'{arg}' needs an export name"))?;
            }
            other if other.starts_with("--call=") => {
                entry = other["--call=".len()..].to_string();
            }
            "--surface" => {
                let name = args
                    .next()
                    .ok_or_else(|| "'--surface' needs 'lua' or 'ruby'".to_string())?;
                surface = Some(parse_surface(&name)?);
            }
            other if other.starts_with("--surface=") => {
                surface = Some(parse_surface(&other["--surface=".len()..])?);
            }
            "--print-lua" => print_lua = true,
            "--cache" => saw_cache = true,
            "--no-cache" => saw_no_cache = true,
            "--cache-file" => {
                cache_path = Some(
                    args.next()
                        .ok_or_else(|| "'--cache-file' needs a path".to_string())?,
                );
            }
            other if other.starts_with("--cache-file=") => {
                cache_path = Some(other["--cache-file=".len()..].to_string());
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option '{other}' (try --help)"));
            }
            other => {
                if file.replace(other.to_string()).is_some() {
                    return Err("more than one script file given".to_string());
                }
            }
        }
    }

    if saw_cache && saw_no_cache {
        return Err("--cache and --no-cache are mutually exclusive".to_string());
    }
    if saw_no_cache && cache_path.is_some() {
        return Err("--cache-file has no effect with --no-cache".to_string());
    }
    if print_lua && (saw_cache || cache_path.is_some()) {
        return Err(
            "--print-lua neither runs the script nor touches the cache, so it takes no \
             --cache or --cache-file"
                .to_string(),
        );
    }
    let mode = if saw_no_cache {
        CacheMode::Disabled
    } else if saw_cache {
        CacheMode::WriteOnly
    } else {
        CacheMode::ReadWrite
    };

    match file {
        Some(file) => Ok(Some(Args {
            file,
            entry,
            mode,
            cache_path,
            surface,
            print_lua,
        })),
        None => Err("no script file given (try --help)".to_string()),
    }
}

/// The compiled-IR disk cache. Persists `ir::Program` (not native code) so repeated runs of an
/// unchanged script can skip the front end.
mod cache {
    use std::hash::{Hash, Hasher};
    use std::time::UNIX_EPOCH;

    use grindlang::ir::verify;
    use grindlang::{Program, Surface};

    /// On-disk envelope version. Bump when [`Program`]'s serialized shape or this envelope
    /// changes, so a cache written by an older build is rejected rather than misread.
    const FORMAT: u32 = 5;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Envelope {
        format: u32,
        build: String,
        /// The syntax the source was read in: the same text can mean different programs in
        /// each.
        surface: String,
        source_hash: u64,
        program: Program,
    }

    fn hash_source(src: &str) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        src.hash(&mut hasher);
        hasher.finish()
    }

    fn surface_name(surface: Surface) -> &'static str {
        match surface {
            Surface::Lua => "lua",
            Surface::Ruby => "ruby",
        }
    }

    /// Identifies the build of this executable: the crate version plus the executable file's size
    /// and modification time.
    ///
    /// The crate version alone stays the same across rebuilds, so it can't tell a cache written
    /// before a lowering fix from one written after. Rebuilding rewrites the executable, which
    /// changes its modification time. Reading metadata instead of hashing the executable keeps a
    /// cache hit cheap; a false miss (a copy of the same binary, say) only costs a recompile.
    ///
    /// `None` when the executable can't be inspected, in which case the cache is not used.
    fn build_id() -> Option<String> {
        let meta = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
        let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(format!(
            "{}+{}+{}",
            env!("CARGO_PKG_VERSION"),
            meta.len(),
            modified.as_nanos()
        ))
    }

    /// Load a cached program if the file exists, deserializes, matches this build, surface, and
    /// source, and re-verifies. Any mismatch or error is a cache miss (`None`) — the caller
    /// recompiles.
    pub fn load(path: &str, src: &str, surface: Surface) -> Option<Program> {
        load_for(&build_id()?, path, src, surface)
    }

    fn load_for(build: &str, path: &str, src: &str, surface: Surface) -> Option<Program> {
        let bytes = std::fs::read(path).ok()?;
        let envelope: Envelope = serde_json::from_slice(&bytes).ok()?;
        if envelope.format != FORMAT
            || envelope.build != build
            || envelope.surface != surface_name(surface)
            || envelope.source_hash != hash_source(src)
        {
            return None;
        }
        // Never trust on-disk IR blindly: re-verify before handing it to codegen.
        verify(&envelope.program).ok()?;
        Some(envelope.program)
    }

    /// Write `program` to `path`. Returns the error as a string on failure so the caller can
    /// decide whether it is fatal (`--cache`) or a warning (the default read-write mode).
    pub fn store(path: &str, src: &str, surface: Surface, program: &Program) -> Result<(), String> {
        let build = build_id().ok_or("cannot identify this executable's build")?;
        store_for(&build, path, src, surface, program)
    }

    fn store_for(
        build: &str,
        path: &str,
        src: &str,
        surface: Surface,
        program: &Program,
    ) -> Result<(), String> {
        let envelope = Envelope {
            format: FORMAT,
            build: build.to_string(),
            surface: surface_name(surface).to_string(),
            source_hash: hash_source(src),
            program: program.clone(),
        };
        let bytes = serde_json::to_vec(&envelope).map_err(|e| e.to_string())?;
        std::fs::write(path, bytes).map_err(|e| e.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_cache_is_only_reused_by_the_build_surface_and_source_that_wrote_it() {
            let src = "function main() return 1 end";
            let program = grindlang::compile(src, &Default::default()).expect("compile");
            let path =
                std::env::temp_dir().join(format!("grindlang-cache-{}.glir", std::process::id()));
            let path = path.to_str().expect("utf-8 temp path");

            store_for("build-a", path, src, Surface::Lua, &program).expect("store");
            let hit = load_for("build-a", path, src, Surface::Lua);
            let rebuilt = load_for("build-b", path, src, Surface::Lua);
            let edited = load_for(
                "build-a",
                path,
                "function main() return 2 end",
                Surface::Lua,
            );
            let other_surface = load_for("build-a", path, src, Surface::Ruby);
            let _ = std::fs::remove_file(path);

            assert!(hit.is_some(), "same build and source must reuse the cache");
            assert!(rebuilt.is_none(), "a different build must recompile");
            assert!(edited.is_none(), "an edited source must recompile");
            assert!(
                other_surface.is_none(),
                "the source read in another surface must recompile"
            );
        }

        #[test]
        fn this_executable_has_a_build_id() {
            assert!(build_id().is_some());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &[&str]) -> Result<Args, String> {
        parse_args(line.iter().map(|s| s.to_string())).map(|a| a.expect("not --help"))
    }

    #[test]
    fn the_extension_picks_the_surface_unless_one_is_named() {
        assert_eq!(args(&["a.lua"]).unwrap().surface(), Surface::Lua);
        assert_eq!(args(&["dir/a.rb"]).unwrap().surface(), Surface::Ruby);
        assert_eq!(args(&["A.RB"]).unwrap().surface(), Surface::Ruby);
        assert_eq!(args(&["a.txt"]).unwrap().surface(), Surface::Lua);
        assert_eq!(
            args(&["--surface", "ruby", "a.lua"]).unwrap().surface(),
            Surface::Ruby
        );
        assert_eq!(
            args(&["--surface=lua", "a.rb"]).unwrap().surface(),
            Surface::Lua
        );
        assert!(args(&["--surface", "python", "a.py"]).is_err());
        assert!(args(&["a.rb", "--surface"]).is_err());
    }

    #[test]
    fn print_lua_takes_no_cache_option() {
        let a = args(&["--print-lua", "a.rb"]).unwrap();
        assert!(a.print_lua);
        assert_eq!(a.mode, CacheMode::ReadWrite);
        assert!(args(&["--print-lua", "--no-cache", "a.rb"]).is_ok());
        assert!(args(&["--print-lua", "--cache", "a.rb"]).is_err());
        assert!(args(&["--print-lua", "--cache-file", "x", "a.rb"]).is_err());
    }

    #[cfg(feature = "ruby")]
    #[test]
    fn a_ruby_script_compiles_and_prints_as_lua() {
        let src = "def main\n  [1, 2, 3].sum { |x| x * 2 }\nend\n";
        let program = compile_source(src, "a.rb", Surface::Ruby).expect("compile");
        assert!(validate_entry(&program, "main").is_ok());
        let module = parse_source(src, "a.rb", Surface::Ruby).expect("translate");
        let lua = grindlang::unparse::to_lua(&module);
        assert!(lua.starts_with("-- Translated from Ruby"), "{lua}");
        assert!(lua.contains("function main()"), "{lua}");

        let err = compile_source("def main\n  1 + \"a\"\nend\n", "a.rb", Surface::Ruby)
            .expect_err("a string operand of +");
        assert!(err.contains("E0600") && err.contains("--> 2:"), "{err}");
    }
}
