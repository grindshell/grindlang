//! # Host embedding API (`PLAN.md` Phase 8, `jit` feature)
//!
//! The clean, public surface an embedder uses to run Grindlang. It ties the front end, the
//! runtime, and the cranelift JIT together behind two types:
//!
//! * [`Engine`] — the configuration stage. Register host functions (typed Rust closures or
//!   a raw escape hatch) and declare the host **memory** schema, then [`Engine::compile`] a
//!   script source into a [`Module`].
//! * [`Module`] — a compiled script. Bind memory, [`call`](Module::call_typed) its exports
//!   with typed Rust arguments, and introspect its export signature.
//!
//! Backend: **JIT-only**. The whole module is gated behind the `jit` feature and always
//! compiles scripts to native code (the tree-walking interpreter remains available as
//! [`crate::interp`] for debugging / as the differential oracle, but the embedding surface is
//! the JIT).
//!
//! Marshaling: **typed closures with a raw escape hatch**. Host functions are ordinary Rust
//! closures whose parameter/return types are inferred into the script's type environment
//! ([`Engine::register_fn`]); dynamic or variadic cases drop to
//! [`Engine::register_fn_raw`]. Calls marshal Rust tuples in and a typed value out
//! ([`Module::call_typed`]), with [`Module::call`] as the raw `Value`-level path.
//!
//! ```
//! use grindlang::api::Engine;
//!
//! let mut engine = Engine::new();
//! engine.register_fn("double", |n: f64| n * 2.0);
//! let mut module = engine
//!     .compile("function calc(x) return double(x) + 1 end")
//!     .unwrap();
//! let out: f64 = module.call_typed("calc", (20.0,)).unwrap();
//! assert_eq!(out, 41.0);
//! ```
//!
//! ## Threading
//!
//! An [`Engine`] / [`Module`] is single-threaded (host closures are stored by `Rc`, matching
//! the trust model: scripts are trusted dev code, invoked synchronously). Compile separate
//! modules per thread if needed.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::codegen::{JitError, JitModule};
use crate::ir::LowerError;
use crate::types::{FnType, Type, TypeConfig};
use crate::value::{NativeFn, RunError, Value};

// ---- errors -----------------------------------------------------------------

/// A failure while marshaling a [`Value`] to or from a Rust type.
#[derive(Clone, Debug, thiserror::Error)]
pub enum MarshalError {
    /// A value's runtime shape didn't match the requested Rust type.
    #[error("expected a {expected} value, found {found}")]
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },
}

/// A failure from [`Engine::compile`]: a front-end error, an IR lowering error, or a JIT
/// codegen error.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// Lexing / parsing / resolution / type-checking diagnostics.
    #[error("{0}")]
    Check(#[from] crate::diagnostics::Diagnostics),
    /// IR lowering failed (an unsupported construct or an internal invariant).
    #[error("{0}")]
    Lower(#[from] LowerError),
    /// Cranelift codegen failed.
    #[error("{0}")]
    Jit(#[from] JitError),
}

/// A failure from a typed call: the script raised an error, or its result couldn't be
/// marshaled into the requested Rust type.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The script (or a host function it called) raised a runtime error.
    #[error(transparent)]
    Run(#[from] RunError),
    /// The returned value couldn't be marshaled into the requested Rust type.
    #[error("result marshaling failed: {0}")]
    Result(#[source] MarshalError),
}

// ---- value marshaling traits ------------------------------------------------

/// A Rust type with a corresponding Grindlang [`Type`]. The supertrait of [`IntoValue`] /
/// [`FromValue`]; used to infer host-function signatures.
pub trait HostType {
    /// The Grindlang type this Rust type marshals as.
    fn grindlang_type() -> Type;
}

/// A Rust type that can be converted **into** a Grindlang [`Value`] (host-function arguments
/// flow this way into the script; host-function results flow this way out).
pub trait IntoValue: HostType {
    fn into_value(self) -> Value;
}

/// A Rust type that can be extracted **from** a Grindlang [`Value`] (call results, and
/// host-function arguments on the Rust side).
pub trait FromValue: HostType + Sized {
    fn from_value(v: Value) -> Result<Self, MarshalError>;
}

fn want<T>(expected: &'static str, v: &Value) -> Result<T, MarshalError>
where
    T: Sized,
{
    Err(MarshalError::TypeMismatch {
        expected,
        found: v.type_name(),
    })
}

macro_rules! number_marshal {
    ($($t:ty),*) => {$(
        impl HostType for $t {
            fn grindlang_type() -> Type { Type::Number }
        }
        impl IntoValue for $t {
            fn into_value(self) -> Value { Value::Number(self as f64) }
        }
        impl FromValue for $t {
            fn from_value(v: Value) -> Result<Self, MarshalError> {
                match v.as_f64() {
                    Some(n) => Ok(n as $t),
                    None => want("number", &v),
                }
            }
        }
    )*};
}
number_marshal!(f64, f32, i64, i32, u32, usize);

impl HostType for bool {
    fn grindlang_type() -> Type {
        Type::Bool
    }
}
impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Bool(self)
    }
}
impl FromValue for bool {
    fn from_value(v: Value) -> Result<Self, MarshalError> {
        match v.as_bool() {
            Some(b) => Ok(b),
            None => want("bool", &v),
        }
    }
}

impl HostType for String {
    fn grindlang_type() -> Type {
        Type::String
    }
}
impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::string(self)
    }
}
impl FromValue for String {
    fn from_value(v: Value) -> Result<Self, MarshalError> {
        match v.as_string() {
            Some(s) => Ok(s),
            None => want("string", &v),
        }
    }
}

impl HostType for () {
    fn grindlang_type() -> Type {
        Type::Unit
    }
}
impl IntoValue for () {
    fn into_value(self) -> Value {
        Value::Nil
    }
}
impl FromValue for () {
    fn from_value(_: Value) -> Result<Self, MarshalError> {
        Ok(())
    }
}

impl<T: HostType> HostType for Option<T> {
    fn grindlang_type() -> Type {
        Type::optional(T::grindlang_type())
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        match self {
            Some(v) => v.into_value(),
            None => Value::Nil,
        }
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value) -> Result<Self, MarshalError> {
        match v {
            Value::Nil => Ok(None),
            other => Ok(Some(T::from_value(other)?)),
        }
    }
}

impl<T: HostType> HostType for Vec<T> {
    fn grindlang_type() -> Type {
        Type::array(T::grindlang_type())
    }
}
impl<T: IntoValue> IntoValue for Vec<T> {
    fn into_value(self) -> Value {
        Value::array(self.into_iter().map(IntoValue::into_value).collect())
    }
}
impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(v: Value) -> Result<Self, MarshalError> {
        match v.as_array() {
            Some(items) => items.into_iter().map(T::from_value).collect(),
            None => want("array", &v),
        }
    }
}

// ---- host function traits ---------------------------------------------------

/// An **infallible** Rust closure usable as a host function: each parameter is a
/// [`FromValue`] and the return is an [`IntoValue`]. Implemented for `Fn`s of arity 0–6; the
/// `Args` type parameter lets the compiler pick the arity from the closure's signature.
///
/// For host functions that can fail, use [`HostTryFn`] (via [`Engine::register_try_fn`]) or
/// the raw [`Engine::register_fn_raw`]. Infallible and fallible live in separate traits
/// because Rust's coherence rules forbid one trait covering both `T` and `Result<T, E>`.
pub trait HostFn<Args> {
    /// The inferred Grindlang signature (what the type checker sees).
    fn signature(&self) -> FnType;
    /// Type-erase into a [`NativeFn`] the runtime can call.
    fn into_native(self) -> NativeFn;
}

/// A **fallible** Rust closure usable as a host function: like [`HostFn`] but returning
/// `Result<R, E>` where `R: IntoValue` and `E: Display`. An `Err` surfaces in the script as a
/// [`RunError::Host`].
pub trait HostTryFn<Args> {
    fn signature(&self) -> FnType;
    fn into_native(self) -> NativeFn;
}

macro_rules! impl_host_fn {
    ($($T:ident),*) => {
        #[allow(non_snake_case)]
        impl<Func, Ret, $($T),*> HostFn<($($T,)*)> for Func
        where
            Func: Fn($($T),*) -> Ret + 'static,
            Ret: IntoValue,
            $($T: FromValue,)*
        {
            fn signature(&self) -> FnType {
                FnType {
                    params: vec![$(<$T as HostType>::grindlang_type()),*],
                    ret: Box::new(Ret::grindlang_type()),
                }
            }
            fn into_native(self) -> NativeFn {
                Rc::new(move |args: &[Value]| {
                    #[allow(unused_mut, unused_variables)]
                    let mut it = args.iter().cloned();
                    $(
                        let $T = <$T as FromValue>::from_value(it.next().unwrap_or(Value::Nil))
                            .map_err(|e| RunError::Host(e.to_string()))?;
                    )*
                    Ok((self)($($T),*).into_value())
                })
            }
        }

        #[allow(non_snake_case)]
        impl<Func, Ret, Err, $($T),*> HostTryFn<($($T,)*)> for Func
        where
            Func: Fn($($T),*) -> Result<Ret, Err> + 'static,
            Ret: IntoValue,
            Err: std::fmt::Display,
            $($T: FromValue,)*
        {
            fn signature(&self) -> FnType {
                FnType {
                    params: vec![$(<$T as HostType>::grindlang_type()),*],
                    ret: Box::new(Ret::grindlang_type()),
                }
            }
            fn into_native(self) -> NativeFn {
                Rc::new(move |args: &[Value]| {
                    #[allow(unused_mut, unused_variables)]
                    let mut it = args.iter().cloned();
                    $(
                        let $T = <$T as FromValue>::from_value(it.next().unwrap_or(Value::Nil))
                            .map_err(|e| RunError::Host(e.to_string()))?;
                    )*
                    match (self)($($T),*) {
                        Ok(v) => Ok(v.into_value()),
                        Err(e) => Err(RunError::Host(e.to_string())),
                    }
                })
            }
        }
    };
}

impl_host_fn!();
impl_host_fn!(A);
impl_host_fn!(A, B);
impl_host_fn!(A, B, C);
impl_host_fn!(A, B, C, D);
impl_host_fn!(A, B, C, D, E);
impl_host_fn!(A, B, C, D, E, F);

// ---- call arguments ---------------------------------------------------------

/// A bundle of Rust values usable as the argument list of [`Module::call_typed`]. Implemented
/// for tuples of arity 0–6 (each element an [`IntoValue`]) and for a raw `Vec<Value>`.
pub trait IntoArgs {
    fn into_args(self) -> Vec<Value>;
}

impl IntoArgs for () {
    fn into_args(self) -> Vec<Value> {
        Vec::new()
    }
}
impl IntoArgs for Vec<Value> {
    fn into_args(self) -> Vec<Value> {
        self
    }
}

macro_rules! impl_into_args {
    ($($T:ident $n:tt),+) => {
        impl<$($T: IntoValue),+> IntoArgs for ($($T,)+) {
            fn into_args(self) -> Vec<Value> {
                vec![$(self.$n.into_value()),+]
            }
        }
    };
}
impl_into_args!(A 0);
impl_into_args!(A 0, B 1);
impl_into_args!(A 0, B 1, C 2);
impl_into_args!(A 0, B 1, C 2, D 3);
impl_into_args!(A 0, B 1, C 2, D 3, E 4);
impl_into_args!(A 0, B 1, C 2, D 3, E 4, F 5);

// ---- engine -----------------------------------------------------------------

/// The configuration + compilation entry point. Register host functions and declare the
/// memory schema, then [`compile`](Engine::compile) scripts into [`Module`]s.
///
/// Host functions and memory are declared **before** compilation because the type checker
/// needs their signatures. The same `Engine` can compile many modules; its registered
/// closures are shared (by `Rc`) into each.
#[derive(Default)]
pub struct Engine {
    /// name → (signature for the checker, type-erased implementation)
    host_fns: BTreeMap<String, (FnType, NativeFn)>,
    /// name → declared type of a host memory binding (commonly a record or map)
    memory: BTreeMap<String, Type>,
}

impl Engine {
    /// A new engine with no host functions or memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a typed host function. The Grindlang signature is inferred from the closure's
    /// parameter and return types (annotate the parameters, e.g. `|n: f64| ...`). Fallible
    /// host functions return `Result<T, E>` — the `Err` surfaces in the script as a runtime
    /// error.
    ///
    /// ```
    /// use grindlang::api::Engine;
    /// let mut engine = Engine::new();
    /// engine.register_fn("clamp01", |x: f64| x.clamp(0.0, 1.0));
    /// ```
    pub fn register_fn<Args, F>(&mut self, name: impl Into<String>, f: F) -> &mut Self
    where
        F: HostFn<Args>,
    {
        let sig = f.signature();
        let native = f.into_native();
        self.host_fns.insert(name.into(), (sig, native));
        self
    }

    /// Register a **fallible** typed host function returning `Result<R, E>`. The `Err`
    /// surfaces in the script as a runtime error.
    ///
    /// ```
    /// use grindlang::api::Engine;
    /// let mut engine = Engine::new();
    /// engine.register_try_fn("checked_sqrt", |x: f64| {
    ///     if x < 0.0 { Err("negative") } else { Ok(x.sqrt()) }
    /// });
    /// ```
    pub fn register_try_fn<Args, F>(&mut self, name: impl Into<String>, f: F) -> &mut Self
    where
        F: HostTryFn<Args>,
    {
        let sig = HostTryFn::signature(&f);
        let native = HostTryFn::into_native(f);
        self.host_fns.insert(name.into(), (sig, native));
        self
    }

    /// Register a host function with an explicit signature and a raw `Value`-level
    /// implementation — the escape hatch for dynamic or variadic functions the typed
    /// [`register_fn`](Engine::register_fn) can't express.
    pub fn register_fn_raw<F>(&mut self, name: impl Into<String>, sig: FnType, f: F) -> &mut Self
    where
        F: Fn(&[Value]) -> Result<Value, RunError> + 'static,
    {
        self.host_fns.insert(name.into(), (sig, Rc::new(f)));
        self
    }

    /// Declare a host memory binding's type so scripts may read/write it. The runtime value
    /// is bound per-module via [`Module::set_memory`]. Memory is typically a
    /// [`Type::Record`] (fixed fields) or [`Type::Map`] (homogeneous string keys).
    pub fn declare_memory(&mut self, name: impl Into<String>, ty: Type) -> &mut Self {
        self.memory.insert(name.into(), ty);
        self
    }

    /// The [`TypeConfig`] implied by the registered host functions and memory.
    fn type_config(&self) -> TypeConfig {
        TypeConfig {
            host_functions: self
                .host_fns
                .iter()
                .map(|(k, (sig, _))| (k.clone(), sig.clone()))
                .collect(),
            memory: self.memory.clone(),
        }
    }

    /// Compile a script source into a runnable [`Module`], type-checked against this engine's
    /// host functions and memory schema, and JIT-compiled to native code. The engine's host
    /// closures are installed into the module.
    ///
    /// To recompile on change, simply call `compile` again — each call yields an independent
    /// `Module` owning its own native code; hosts can cache `Module`s by source as they see
    /// fit.
    pub fn compile(&self, src: &str) -> Result<Module, BuildError> {
        let cfg = self.type_config();
        let (module, res, info) = crate::analyze(src, &cfg)?;
        let program = crate::ir::lower(&module, &res, &info, &cfg)?;
        crate::ir::verify(&program)?;
        let mut jit = JitModule::compile(&program)?;
        for (name, (_sig, native)) in &self.host_fns {
            jit.set_host_fn(name.clone(), native.clone());
        }
        Ok(Module {
            jit,
            exports: info.exports,
        })
    }
}

// ---- module -----------------------------------------------------------------

/// A compiled script: native code plus its export signature. Bind memory, then call exports.
pub struct Module {
    jit: JitModule,
    exports: BTreeMap<String, Type>,
}

impl Module {
    /// Bind a host memory value (typically built with [`Value::table`] / [`Value::array`]).
    /// The module shares it by `Rc`, so script mutations are observable via
    /// [`memory`](Module::memory) after a call returns. Persists across calls until rebound.
    pub fn set_memory(&mut self, name: impl Into<String>, value: Value) -> &mut Self {
        self.jit.set_memory(name, value);
        self
    }

    /// Read back a bound memory value (a clone of the shared `Rc`), e.g. to observe mutations.
    pub fn memory(&self, name: &str) -> Option<Value> {
        self.jit.memory(name)
    }

    /// Call an exported function with typed Rust arguments and marshal its typed result.
    ///
    /// Arguments are a tuple (`(a, b)`, `(x,)`, or `()` for none) or a raw `Vec<Value>`.
    ///
    /// ```
    /// use grindlang::api::Engine;
    /// let mut m = Engine::new()
    ///     .compile("function add(a, b) return a + b end")
    ///     .unwrap();
    /// let sum: f64 = m.call_typed("add", (2.0, 3.0)).unwrap();
    /// assert_eq!(sum, 5.0);
    /// ```
    pub fn call_typed<Args, R>(&mut self, name: &str, args: Args) -> Result<R, CallError>
    where
        Args: IntoArgs,
        R: FromValue,
    {
        let v = self.jit.call(name, args.into_args())?;
        R::from_value(v).map_err(CallError::Result)
    }

    /// Call an exported function at the raw [`Value`] level (no marshaling). Useful for
    /// dynamic argument lists or when a result's shape isn't statically known.
    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        self.jit.call(name, args)
    }

    /// Invoke a closure value previously returned by [`call`](Self::call_typed) (or by another
    /// `call_value`), at the raw [`Value`] level. The closure carries its captured upvalues,
    /// so mutations persist across host calls, and it keeps its backing code alive — so it
    /// remains callable even after the originating [`Module`] is dropped.
    ///
    /// `callee` must be a function value (`Type::Function(..)`); anything else is a runtime
    /// error. Closures cannot be persisted into host memory or serialized.
    pub fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
        self.jit.call_value(callee, args)
    }

    /// Invoke a returned closure with typed Rust arguments and marshal its typed result —
    /// the [`call_typed`](Self::call_typed) analog for a first-class function value.
    ///
    /// ```
    /// use grindlang::api::Engine;
    /// let mut m = Engine::new()
    ///     .compile("function make_adder(n) return function(x) return x + n end end")
    ///     .unwrap();
    /// // The closure is an opaque function value; obtain it via the raw `call`.
    /// let adder = m.call("make_adder", vec![grindlang::Value::Number(10.0)]).unwrap();
    /// let sum: f64 = m.call_value_typed(adder, (5.0,)).unwrap();
    /// assert_eq!(sum, 15.0);
    /// ```
    pub fn call_value_typed<Args, R>(&mut self, callee: Value, args: Args) -> Result<R, CallError>
    where
        Args: IntoArgs,
        R: FromValue,
    {
        let v = self.jit.call_value(callee, args.into_args())?;
        R::from_value(v).map_err(CallError::Result)
    }

    /// The module's export signature: each exported name → its Grindlang [`Type`].
    pub fn exports(&self) -> &BTreeMap<String, Type> {
        &self.exports
    }

    /// The declared type of a single export, if present.
    pub fn export_type(&self, name: &str) -> Option<&Type> {
        self.exports.get(name)
    }
}
