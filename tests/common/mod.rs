//! Test harness shared by the integration suites that hold all three oracles to one program.
//!
//! A test file uses it with `mod common;`. Each file is its own crate and uses only part of the
//! harness, hence the `dead_code` allowance.

#![allow(dead_code)]

use grindlang::codegen::JitModule;
use grindlang::interp::Interpreter;
use grindlang::ir::Vm;
use grindlang::{Module, RunError, TypeConfig, Value};

/// The four executors for one program: AST, IR, JIT, and a JIT that compacts its value table at
/// every loop back edge. Artifacts are leaked for `'static` — fine for a test harness.
pub struct Executors {
    pub interp: Interpreter<'static>,
    pub vm: Vm<'static>,
    pub jit: JitModule,
    pub eager: JitModule,
}

impl Executors {
    /// Build the executors for Lua source `src`.
    pub fn new(src: &str, cfg: &TypeConfig) -> Self {
        let module =
            grindlang::parse(src).unwrap_or_else(|d| panic!("parse failed:\n{}", d.render(src)));
        Self::from_module(module, cfg, src)
    }

    /// Build the executors for an already parsed `module`, from any surface syntax. `src` is the
    /// text it was parsed from, used only to render diagnostics if it fails to check.
    pub fn from_module(module: Module, cfg: &TypeConfig, src: &str) -> Self {
        let module = Box::leak(Box::new(module));
        let (res, info) = grindlang::analyze_module(module, cfg)
            .unwrap_or_else(|d| panic!("analyze failed:\n{}", d.render(src)));
        let res = Box::leak(Box::new(res));
        let info = Box::leak(Box::new(info));
        let program = Box::leak(Box::new(
            grindlang::ir::lower(module, res, info, cfg).expect("lower"),
        ));
        grindlang::ir::verify(program).expect("verify");
        let mut eager = JitModule::compile(program).expect("jit compile");
        eager.set_compact_slack(0);
        Executors {
            interp: Interpreter::new(module, res).expect("interp"),
            vm: Vm::new(program),
            jit: JitModule::compile(program).expect("jit compile"),
            eager,
        }
    }

    /// Bind host memory `name` on every executor, each to a table of its own from `make`, so
    /// each executor's writes can be compared ([`Self::assert_memory`]).
    pub fn set_memory(&mut self, name: &str, make: impl Fn() -> Value) {
        self.interp.set_memory(name, make());
        self.vm.set_memory(name, make());
        self.jit.set_memory(name, make());
        self.eager.set_memory(name, make());
    }

    /// Register the memory method `symbol` (`"mem:method"`) on every executor.
    pub fn set_method<F>(&mut self, symbol: &str, f: F)
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + Clone + 'static,
    {
        self.interp.set_method(symbol, f.clone());
        self.vm.set_method(symbol, f.clone());
        self.jit.set_method(symbol, f.clone());
        self.eager.set_method(symbol, f);
    }

    /// Register the host function `name` on every executor.
    pub fn set_host_function<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + Clone + 'static,
    {
        self.interp.set_host_function(name, f.clone());
        self.vm.set_host_function(name, f.clone());
        self.jit.set_host_function(name, f.clone());
        self.eager.set_host_function(name, f);
    }

    /// Assert every executor's memory `name` reads `want` (by `Display`).
    #[track_caller]
    pub fn assert_memory(&self, name: &str, want: &str) {
        for (oracle, got) in [
            ("AST", self.interp.memory(name)),
            ("IR", self.vm.memory(name)),
            ("JIT", self.jit.memory(name)),
            ("eager JIT", self.eager.memory(name)),
        ] {
            let got = got.unwrap_or_else(|| panic!("{oracle} has no memory `{name}`"));
            assert_eq!(got.to_string(), want, "{oracle} memory `{name}`");
        }
    }

    /// Call `func` on every executor, labelled.
    pub fn call(
        &mut self,
        func: &str,
        args: Vec<Value>,
    ) -> [(&'static str, Result<Value, RunError>); 4] {
        [
            ("AST", self.interp.call(func, args.clone())),
            ("IR", self.vm.call(func, args.clone())),
            ("JIT", self.jit.call(func, args.clone())),
            ("eager JIT", self.eager.call(func, args)),
        ]
    }

    /// Call, on every executor, the closure found at `path` in the value of the export `name`:
    /// each step is a table's key, or a 1-based position in an array.
    pub fn call_entry(
        &mut self,
        name: &str,
        path: &[&str],
        args: Vec<Value>,
    ) -> [(&'static str, Result<Value, RunError>); 4] {
        let find = |got: Result<Value, RunError>| -> Value {
            let root = got.unwrap_or_else(|e| panic!("reading `{name}`: {e}"));
            path.iter()
                .fold(root, |v, step| match step.parse::<usize>() {
                    Ok(i) => v.as_array().expect("an array")[i - 1].clone(),
                    Err(_) => v
                        .field(step)
                        .unwrap_or_else(|| panic!("no `{step}` in {v}")),
                })
        };
        let f = find(self.interp.call(name, vec![]));
        let interp = self.interp.call_value_public(f, args.clone());
        let f = find(self.vm.call(name, vec![]));
        let vm = self.vm.call_value(f, args.clone());
        let f = find(self.jit.call(name, vec![]));
        let jit = self.jit.call_value(f, args.clone());
        let f = find(self.eager.call(name, vec![]));
        let eager = self.eager.call_value(f, args);
        [
            ("AST", interp),
            ("IR", vm),
            ("JIT", jit),
            ("eager JIT", eager),
        ]
    }

    /// Assert every executor's closure at `path` in the export `name` returns `want`
    /// ([`Self::call_entry`]).
    #[track_caller]
    pub fn assert_entry_returns(
        &mut self,
        name: &str,
        path: &[&str],
        args: Vec<Value>,
        want: &str,
    ) {
        for (oracle, got) in self.call_entry(name, path, args) {
            match got {
                Ok(v) => assert_eq!(v.to_string(), want, "{oracle}: `{name}` at {path:?}"),
                Err(e) => panic!("{oracle} failed on `{name}` at {path:?}: {e}"),
            }
        }
    }

    /// Assert every executor returns `want` (by `Display`) from `func(args)`.
    #[track_caller]
    pub fn assert_returns(&mut self, func: &str, args: Vec<Value>, want: &str) {
        for (oracle, got) in self.call(func, args) {
            match got {
                Ok(v) => assert_eq!(v.to_string(), want, "{oracle} result of `{func}`"),
                Err(e) => panic!("{oracle} failed on `{func}`: {e}"),
            }
        }
    }

    /// Assert every executor fails `func(args)` with an error mentioning `needle`.
    #[track_caller]
    pub fn assert_fails(&mut self, func: &str, args: Vec<Value>, needle: &str) {
        for (oracle, got) in self.call(func, args) {
            match got {
                Ok(v) => panic!("{oracle}: `{func}` returned {v}, expected an error"),
                Err(e) => assert!(
                    e.to_string().contains(needle),
                    "{oracle}: `{func}` failed with {e:?}, expected one mentioning {needle:?}"
                ),
            }
        }
    }
}
