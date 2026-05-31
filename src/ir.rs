//! Mid-level typed IR and lowering from the typed AST (`PLAN.md` Phase 5).
//!
//! The IR is a control-flow graph of basic blocks. It sits between the AST and the future
//! cranelift backend (Phase 7) and is deliberately backend-agnostic:
//!
//!   * **Locals are mutable variables**, not SSA values. Reads/writes go through
//!     [`Op::LocalGet`] / [`Op::LocalSet`]. This matches `cranelift-frontend`'s `Variable`
//!     abstraction (it performs SSA construction for us), so lowering emits no phi nodes.
//!   * **Expression results are SSA temporaries** ([`ValueId`]) — each is assigned exactly
//!     once, by the instruction that produces it.
//!   * Every basic block ends in exactly one [`Terminator`].
//!   * Every [`ValueId`] and [`LocalId`] carries a [`Type`], so the IR is fully typed.
//!
//! Lowering consumes the [`Module`], its [`Resolution`], its [`TypeInfo`], and a
//! [`TypeConfig`] (for host function / memory types). It produces a [`Program`].
//!
//! ## The IR interpreter (`interp` feature)
//!
//! Behind the `interp` feature, [`Vm`] executes the IR directly. It exists so the IR can be
//! validated as a *second* semantics oracle: lowering correctness is checked by running the
//! same program through both the AST interpreter ([`crate::interp`]) and the IR interpreter
//! and asserting equal results (see the differential tests).
//!
//! ## Closures
//!
//! Anonymous `function … end` and `local function` literals are lowered by **closure
//! conversion**: each is lifted into its own top-level [`Function`] taking the closure value
//! as a hidden first (env) parameter, and the literal site emits an [`Op::MakeClosure`].
//! Captured variables (upvalues) become shared **cells** ([`Op::MakeCell`] / [`Op::CellGet`]
//! / [`Op::CellSet`]) so writes are observed across the enclosing scope and sibling closures,
//! matching the interpreter. A first-class function value is called with [`Op::CallValue`].
//!
//! ## Deliberately deferred (documented gaps)
//!
//! * **Named functions as first-class values.** Using a *named* top-level or host function as
//!   a value (rather than calling it directly) raises [`LowerError::Unsupported`].
//! * **Method calls** (`recv:m(...)`).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ast::{
    self, BinOp, Expr, ExprKind, Field, FuncBody, Ident, IterExpr, Module, Stat, StatKind, TopDecl,
    UnOp,
};
use crate::resolve::{Binding, Resolution, SymbolId};
use crate::types::{Type, TypeConfig, TypeInfo};

/// An SSA temporary holding the result of one instruction.
pub type ValueId = u32;
/// A mutable local variable (a parameter, `local`, or loop variable). Mirrors a
/// [`SymbolId`] for source-level locals; synthetic temporaries get fresh ids past the end
/// of the symbol table.
pub type LocalId = u32;
/// A basic block index into a [`Function`]'s `blocks`.
pub type BlockId = u32;

/// An error raised while lowering the AST to IR.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum LowerError {
    /// A construct the IR doesn't lower yet (see the module's deferred-gaps list).
    #[error("unsupported in IR lowering: {0}")]
    Unsupported(String),
    /// An invariant the resolver/checker should have guaranteed was violated.
    #[error("internal lowering error: {0}")]
    Internal(String),
}

/// A whole lowered module.
#[derive(Clone, Debug)]
pub struct Program {
    /// Top-level functions, by declared name.
    pub functions: BTreeMap<String, Function>,
    /// Top-level constants as zero-argument initializer functions, by declared name.
    pub constants: BTreeMap<String, Function>,
    /// Public surface: export name → what it refers to.
    pub exports: BTreeMap<String, ExportTarget>,
}

/// What an exported name refers to within a [`Program`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportTarget {
    Function(String),
    Const(String),
}

/// A lowered function: a typed CFG.
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    /// Parameter locals, in order. For a lifted closure (`is_closure`), the first entry is the
    /// hidden environment parameter (the closure value carrying the captured cells); the rest
    /// are the source-level parameters.
    pub params: Vec<LocalId>,
    pub ret: Type,
    /// Type of every local (parameters, source locals, synthetic temporaries).
    pub locals: BTreeMap<LocalId, Type>,
    /// Type of every SSA value, indexed by [`ValueId`].
    pub values: Vec<Type>,
    pub blocks: Vec<Block>,
    pub entry: BlockId,
    /// Whether this is a lifted closure body (its first parameter is the env). Top-level
    /// functions and constant initializers are `false`.
    pub is_closure: bool,
}

impl Function {
    pub fn value_type(&self, v: ValueId) -> &Type {
        &self.values[v as usize]
    }
}

/// A basic block: a straight-line sequence of instructions ending in a terminator.
#[derive(Clone, Debug)]
pub struct Block {
    pub id: BlockId,
    pub instrs: Vec<Instr>,
    pub term: Terminator,
}

/// A single instruction. `dest` is `Some` for value-producing ops and `None` for
/// effects (stores).
#[derive(Clone, Debug)]
pub struct Instr {
    pub dest: Option<ValueId>,
    pub op: Op,
}

/// An IR operation.
#[derive(Clone, Debug)]
pub enum Op {
    ConstNil,
    ConstBool(bool),
    ConstNumber(f64),
    ConstString(String),

    /// Read a local variable.
    LocalGet(LocalId),
    /// Write a local variable (no result).
    LocalSet(LocalId, ValueId),

    Unary(UnOp, ValueId),
    /// Arithmetic, concat, comparison, and equality. Short-circuiting `and`/`or` are
    /// lowered to control flow, not this op.
    Binary(BinOp, ValueId, ValueId),
    /// Lua truthiness of a value → bool (only `nil`/`false` are falsy). Used to branch on
    /// the operands of short-circuiting `and`/`or`.
    Truthy(ValueId),

    /// Build an array from element values.
    MakeArray(Vec<ValueId>),
    /// Build a table from (key, value) pairs.
    MakeTable(Vec<(String, ValueId)>),

    /// `base[index]` on an array → element (assumed in range at runtime).
    ArrayGet(ValueId, ValueId),
    /// `base[index] = value` on an array (no result).
    ArraySet(ValueId, ValueId, ValueId),
    /// `base[key]` on a map → value or nil (no result type wrapping; see `interp`).
    MapGet(ValueId, ValueId),
    /// `base[key] = value` on a map (no result).
    MapSet(ValueId, ValueId, ValueId),
    /// `base.field` / `base["field"]` table read.
    FieldGet(ValueId, String),
    /// `base.field = value` table write (no result).
    FieldSet(ValueId, String, ValueId),
    /// Snapshot the keys of a map into an array of strings (drives `pairs`).
    MapKeys(ValueId),

    /// Call a top-level script function by name.
    CallScript(String, Vec<ValueId>),
    /// Call a host-registered function by name.
    CallHost(String, Vec<ValueId>),
    /// A plain builtin call (`tostring`, `tonumber`).
    CallBuiltinValue(String, Vec<ValueId>),
    /// A builtin namespace member call (`math.floor`, `string.sub`).
    CallBuiltinMember(String, String, Vec<ValueId>),

    /// Read a top-level constant.
    ConstRef(String),
    /// Read a host memory handle.
    MemoryRef(String),
    /// A builtin namespace value field (`math.pi`, `math.huge`).
    NamespaceField(String, String),

    // ---- closures (upvalues) ----
    /// Box a value into a fresh shared upvalue cell. Result is the cell (a `Ptr`).
    MakeCell(ValueId),
    /// Read the value currently held in a cell.
    CellGet(ValueId),
    /// Write a value into a cell (no result).
    CellSet(ValueId, ValueId),
    /// Read the i-th captured cell from a closure's environment. The operand is the closure
    /// value passed as the lifted function's hidden first parameter; the result is the cell.
    ClosureEnvGet(ValueId, u32),
    /// Build a closure value: a lifted function (by its synthetic name) plus its captured
    /// cells, in env order.
    MakeClosure(String, Vec<ValueId>),
    /// Call a closure value with `args`.
    CallValue(ValueId, Vec<ValueId>),
}

/// How a basic block ends.
#[derive(Clone, Debug)]
pub enum Terminator {
    /// Return `Some(value)` or no value.
    Return(Option<ValueId>),
    /// Unconditional jump.
    Jump(BlockId),
    /// Conditional branch on a `bool` value.
    Branch {
        cond: ValueId,
        then_blk: BlockId,
        else_blk: BlockId,
    },
    /// Statically unreachable (e.g. a merge block with no predecessors).
    Unreachable,
}

/// A nested function literal discovered during lowering, queued to be lifted into its own
/// top-level [`Function`]. The body is cloned (cheap, compile-time only) so no AST lifetime
/// threads through the lowerer; spans are preserved, so resolver/type lookups still work.
struct Pending {
    name: String,
    body: FuncBody,
    depth: usize,
    upvalues: Vec<SymbolId>,
}

/// The IR type used for an upvalue cell / closure handle: any `Repr::Ptr` type works, since
/// the handle is opaque (its element type is never inspected).
fn cell_type() -> Type {
    Type::array(Type::Nil)
}

/// Lower a checked module into IR.
pub fn lower(
    module: &Module,
    res: &Resolution,
    types: &TypeInfo,
    cfg: &TypeConfig,
) -> Result<Program, LowerError> {
    let mut functions = BTreeMap::new();
    let mut constants = BTreeMap::new();
    // Nested closures discovered while lowering are lifted into their own functions; lifting
    // one may discover more, so drain a work queue until it is empty.
    let mut queue: Vec<Pending> = Vec::new();

    for decl in &module.decls {
        match decl {
            TopDecl::Function(f) => {
                let (lowered, discovered) =
                    lower_function(&f.name.node, &f.body, res, types, cfg)?;
                functions.insert(f.name.node.clone(), lowered);
                queue.extend(discovered);
            }
            TopDecl::Const(c) => {
                let lowered = lower_const(&c.name.node, &c.value, res, types, cfg)?;
                constants.insert(c.name.node.clone(), lowered);
            }
        }
    }

    while let Some(p) = queue.pop() {
        let (lowered, discovered) = lower_closure(&p, res, types, cfg)?;
        functions.insert(p.name.clone(), lowered);
        queue.extend(discovered);
    }

    let exports = lower_exports(module, res)?;
    Ok(Program {
        functions,
        constants,
        exports,
    })
}

fn lower_exports(
    module: &Module,
    res: &Resolution,
) -> Result<BTreeMap<String, ExportTarget>, LowerError> {
    let mut exports = BTreeMap::new();
    if let Some(export) = &module.export {
        for field in &export.node {
            let Field::Named { name, value } = field else {
                return Err(LowerError::Internal(
                    "export field is not `name = value`".into(),
                ));
            };
            // The export value must be a bare name referring to a top-level decl.
            let ExprKind::Name(_) = &value.kind else {
                return Err(LowerError::Unsupported(
                    "module exports must be bare references to top-level functions or \
                     constants"
                        .into(),
                ));
            };
            let target = match res.binding(value.span) {
                Some(Binding::TopFunction(n)) => ExportTarget::Function(n.clone()),
                Some(Binding::TopConst(n)) => ExportTarget::Const(n.clone()),
                _ => {
                    return Err(LowerError::Unsupported(
                        "module exports must reference a top-level function or constant".into(),
                    ));
                }
            };
            exports.insert(name.node.clone(), target);
        }
    } else {
        for decl in &module.decls {
            match decl {
                TopDecl::Function(f) => {
                    exports.insert(
                        f.name.node.clone(),
                        ExportTarget::Function(f.name.node.clone()),
                    );
                }
                TopDecl::Const(c) => {
                    exports.insert(
                        c.name.node.clone(),
                        ExportTarget::Const(c.name.node.clone()),
                    );
                }
            }
        }
    }
    Ok(exports)
}

fn lower_function(
    name: &str,
    body: &FuncBody,
    res: &Resolution,
    types: &TypeInfo,
    cfg: &TypeConfig,
) -> Result<(Function, Vec<Pending>), LowerError> {
    let ret = types
        .functions
        .get(name)
        .map(|ft| (*ft.ret).clone())
        .unwrap_or(Type::Unit);
    let mut fl = FnLowerer::new(name.to_string(), ret, res, types, cfg);
    fl.set_boxed(crate::capture::boxed_locals(res, body, 0));
    fl.lower_params(&body.params)?;
    let terminated = fl.lower_block(&body.block)?;
    if !terminated {
        fl.terminate(Terminator::Return(None));
    }
    let discovered = std::mem::take(&mut fl.discovered);
    Ok((fl.finish(), discovered))
}

/// Lower a lifted closure: a function literal hoisted to top level with an explicit env
/// parameter (the closure value, carrying the captured cells) as its hidden first parameter.
fn lower_closure(
    p: &Pending,
    res: &Resolution,
    types: &TypeInfo,
    cfg: &TypeConfig,
) -> Result<(Function, Vec<Pending>), LowerError> {
    let ret = types
        .function_literals
        .get(&p.body.span)
        .map(|ft| (*ft.ret).clone())
        .unwrap_or(Type::Unit);
    let mut fl = FnLowerer::new(p.name.clone(), ret, res, types, cfg);
    fl.depth = p.depth;
    fl.set_boxed(crate::capture::boxed_locals(res, &p.body, p.depth));
    fl.upvalue_index = p
        .upvalues
        .iter()
        .enumerate()
        .map(|(i, &s)| (s, i as u32))
        .collect();
    // The hidden env parameter (params[0]) carries the closure value.
    let env_l = fl.fresh_temp(cell_type());
    fl.env_local = Some(env_l);
    fl.params.push(env_l);
    fl.lower_params(&p.body.params)?;
    let terminated = fl.lower_block(&p.body.block)?;
    if !terminated {
        fl.terminate(Terminator::Return(None));
    }
    let discovered = std::mem::take(&mut fl.discovered);
    Ok((fl.finish(), discovered))
}

fn lower_const(
    name: &str,
    value: &Expr,
    res: &Resolution,
    types: &TypeInfo,
    cfg: &TypeConfig,
) -> Result<Function, LowerError> {
    let ret = types.constants.get(name).cloned().unwrap_or(Type::Error);
    let mut fl = FnLowerer::new(format!("const::{name}"), ret, res, types, cfg);
    let v = fl.lower_expr(value)?;
    fl.terminate(Terminator::Return(Some(v)));
    Ok(fl.finish())
}

struct FnLowerer<'a> {
    name: String,
    ret: Type,
    res: &'a Resolution,
    types: &'a TypeInfo,
    cfg: &'a TypeConfig,

    params: Vec<LocalId>,
    locals: BTreeMap<LocalId, Type>,
    values: Vec<Type>,
    blocks: Vec<Block>,
    current: usize,
    next_temp: LocalId,
    /// Stack of loop-exit blocks for `break`.
    loop_exits: Vec<BlockId>,

    // ---- closure conversion ----
    /// Nesting depth of the function being lowered (0 = top-level).
    depth: usize,
    /// This function's own params/locals that are captured by a nested closure, so they are
    /// stored as shared cells rather than plain values.
    boxed: HashSet<SymbolId>,
    /// For a lifted closure, each captured upvalue symbol → its index in the env.
    upvalue_index: HashMap<SymbolId, u32>,
    /// For a lifted closure, the local holding the env (closure value) — params[0].
    env_local: Option<LocalId>,
    /// Nested function literals discovered here, to be lifted into their own functions.
    discovered: Vec<Pending>,
    /// Per-function counter for minting unique lifted-closure names.
    closure_counter: u32,
}

impl<'a> FnLowerer<'a> {
    fn new(
        name: String,
        ret: Type,
        res: &'a Resolution,
        types: &'a TypeInfo,
        cfg: &'a TypeConfig,
    ) -> Self {
        let next_temp = res.symbols.len() as LocalId;
        let mut fl = FnLowerer {
            name,
            ret,
            res,
            types,
            cfg,
            params: Vec::new(),
            locals: BTreeMap::new(),
            values: Vec::new(),
            blocks: Vec::new(),
            current: 0,
            next_temp,
            loop_exits: Vec::new(),
            depth: 0,
            boxed: HashSet::new(),
            upvalue_index: HashMap::new(),
            env_local: None,
            discovered: Vec::new(),
            closure_counter: 0,
        };
        let entry = fl.new_block();
        fl.current = entry as usize;
        fl
    }

    /// Record which of this function's own symbols are captured (boxed). Pre-types their
    /// locals as cell handles so later `ensure_local` calls don't overwrite the type.
    fn set_boxed(&mut self, boxed: HashSet<SymbolId>) {
        for &id in &boxed {
            self.locals.insert(id, cell_type());
        }
        self.boxed = boxed;
    }

    fn finish(self) -> Function {
        Function {
            name: self.name,
            params: self.params,
            ret: self.ret,
            locals: self.locals,
            values: self.values,
            blocks: self.blocks,
            entry: 0,
            is_closure: self.env_local.is_some(),
        }
    }

    // ---- block / value plumbing ---------------------------------------------

    fn new_block(&mut self) -> BlockId {
        let id = self.blocks.len() as BlockId;
        self.blocks.push(Block {
            id,
            instrs: Vec::new(),
            term: Terminator::Unreachable,
        });
        id
    }

    fn switch(&mut self, b: BlockId) {
        self.current = b as usize;
    }

    fn terminate(&mut self, term: Terminator) {
        self.blocks[self.current].term = term;
    }

    fn set_term(&mut self, b: BlockId, term: Terminator) {
        self.blocks[b as usize].term = term;
    }

    fn emit_value(&mut self, op: Op, ty: Type) -> ValueId {
        let id = self.values.len() as ValueId;
        self.values.push(ty);
        self.blocks[self.current]
            .instrs
            .push(Instr { dest: Some(id), op });
        id
    }

    fn emit_stmt(&mut self, op: Op) {
        self.blocks[self.current]
            .instrs
            .push(Instr { dest: None, op });
    }

    fn fresh_temp(&mut self, ty: Type) -> LocalId {
        let id = self.next_temp;
        self.next_temp += 1;
        self.locals.insert(id, ty);
        id
    }

    fn ensure_local(&mut self, id: LocalId) {
        if !self.locals.contains_key(&id) {
            let ty = self
                .types
                .symbol_types
                .get(id as usize)
                .cloned()
                .unwrap_or(Type::Error);
            self.locals.insert(id, ty);
        }
    }

    fn local_id(&self, span: crate::diagnostics::Span) -> Result<LocalId, LowerError> {
        self.res
            .def(span)
            .ok_or_else(|| LowerError::Internal("missing symbol id for local".into()))
    }

    // ---- closure conversion helpers ------------------------------------------

    /// The inferred type of an in-function symbol.
    fn symbol_type(&self, id: SymbolId) -> Type {
        self.types
            .symbol_types
            .get(id as usize)
            .cloned()
            .unwrap_or(Type::Error)
    }

    /// Whether `id` is accessed through a cell (it is one of this function's boxed own locals,
    /// or an upvalue captured from an enclosing scope).
    fn is_captured(&self, id: SymbolId) -> bool {
        self.boxed.contains(&id) || self.upvalue_index.contains_key(&id)
    }

    /// Produce the cell holding captured symbol `id`, from this function's perspective — from
    /// the env if it's an upvalue, or from the symbol's own (cell-holding) local otherwise.
    fn cell_of(&mut self, id: SymbolId) -> ValueId {
        if let Some(&idx) = self.upvalue_index.get(&id) {
            let env_l = self.env_local.expect("upvalue access in a function without an env");
            let env = self.emit_value(Op::LocalGet(env_l), cell_type());
            self.emit_value(Op::ClosureEnvGet(env, idx), cell_type())
        } else {
            self.ensure_local(id);
            self.emit_value(Op::LocalGet(id), cell_type())
        }
    }

    /// Read symbol `id`, routing through its cell when captured.
    fn read_symbol(&mut self, id: SymbolId) -> ValueId {
        if self.is_captured(id) {
            let cell = self.cell_of(id);
            let ty = self.symbol_type(id);
            self.emit_value(Op::CellGet(cell), ty)
        } else {
            self.ensure_local(id);
            let ty = self.locals[&id].clone();
            self.emit_value(Op::LocalGet(id), ty)
        }
    }

    /// Write `v` to an *existing* symbol `id`, routing through its cell when captured.
    fn write_symbol(&mut self, id: SymbolId, v: ValueId) {
        if self.is_captured(id) {
            let cell = self.cell_of(id);
            self.emit_stmt(Op::CellSet(cell, v));
        } else {
            self.ensure_local(id);
            self.emit_stmt(Op::LocalSet(id, v));
        }
    }

    /// Bind `v` to symbol `id` at its declaration, creating a fresh cell if `id` is a boxed
    /// own local.
    fn bind_local(&mut self, id: SymbolId, v: ValueId) {
        if self.boxed.contains(&id) {
            let cell = self.emit_value(Op::MakeCell(v), cell_type());
            self.ensure_local(id);
            self.emit_stmt(Op::LocalSet(id, cell));
        } else {
            self.ensure_local(id);
            self.emit_stmt(Op::LocalSet(id, v));
        }
    }

    /// Lower a nested function literal: gather its upvalue cells, queue it for lifting under a
    /// fresh unique name, and emit a `MakeClosure` producing the closure value.
    fn lower_closure_literal(&mut self, body: &FuncBody) -> Result<ValueId, LowerError> {
        let child_depth = self.depth + 1;
        let ups = crate::capture::upvalues(self.res, body, child_depth);
        let cells: Vec<ValueId> = ups.iter().map(|&u| self.cell_of(u)).collect();
        let n = self.closure_counter;
        self.closure_counter += 1;
        let name = format!("{}$c{}", self.name, n);
        let fty = self
            .types
            .function_literals
            .get(&body.span)
            .cloned()
            .map(Type::Function)
            .unwrap_or(Type::Error);
        self.discovered.push(Pending {
            name: name.clone(),
            body: body.clone(),
            depth: child_depth,
            upvalues: ups,
        });
        Ok(self.emit_value(Op::MakeClosure(name, cells), fty))
    }

    // ---- declarations --------------------------------------------------------

    fn lower_params(&mut self, params: &[Ident]) -> Result<(), LowerError> {
        for p in params {
            let id = self.local_id(p.span)?;
            if self.boxed.contains(&id) {
                // A captured parameter: keep the raw incoming value in a fresh param local,
                // then box it into the symbol's cell at function entry.
                let ty = self.symbol_type(id);
                let raw = self.fresh_temp(ty.clone());
                self.params.push(raw);
                self.ensure_local(id);
                let rawv = self.emit_value(Op::LocalGet(raw), ty);
                let cell = self.emit_value(Op::MakeCell(rawv), cell_type());
                self.emit_stmt(Op::LocalSet(id, cell));
            } else {
                self.ensure_local(id);
                self.params.push(id);
            }
        }
        Ok(())
    }

    /// Lower a block's statements into the current CFG position. Returns whether control
    /// flow was terminated (a `return` was reached on every path out of this block).
    fn lower_block(&mut self, block: &ast::Block) -> Result<bool, LowerError> {
        for stat in &block.stats {
            if self.lower_stat(stat)? {
                return Ok(true);
            }
        }
        if let Some(ret) = &block.ret {
            let v = match ret.exprs.first() {
                Some(e) => Some(self.lower_expr(e)?),
                None => None,
            };
            self.terminate(Terminator::Return(v));
            return Ok(true);
        }
        Ok(false)
    }

    /// Returns whether the statement terminated control flow.
    fn lower_stat(&mut self, stat: &Stat) -> Result<bool, LowerError> {
        match &stat.kind {
            StatKind::Empty => Ok(false),
            StatKind::Break => {
                let exit = *self
                    .loop_exits
                    .last()
                    .ok_or_else(|| LowerError::Internal("`break` outside a loop".into()))?;
                self.terminate(Terminator::Jump(exit));
                Ok(true)
            }
            StatKind::Local { names, exprs } => {
                let vals: Vec<Option<ValueId>> = exprs
                    .iter()
                    .map(|e| self.lower_expr(e))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(Some)
                    .collect();
                for (i, name) in names.iter().enumerate() {
                    let id = self.local_id(name.span)?;
                    let v = match vals.get(i).copied().flatten() {
                        Some(v) => v,
                        None => self.emit_value(Op::ConstNil, Type::Nil),
                    };
                    self.bind_local(id, v);
                }
                Ok(false)
            }
            StatKind::LocalFunction { name, body } => {
                let id = self.local_id(name.span)?;
                if self.boxed.contains(&id) {
                    // Recursion-safe: create the cell first so the closure can capture it,
                    // build the closure, then store it into that same cell.
                    let nil = self.emit_value(Op::ConstNil, Type::Nil);
                    let cell = self.emit_value(Op::MakeCell(nil), cell_type());
                    self.ensure_local(id);
                    self.emit_stmt(Op::LocalSet(id, cell));
                    let clo = self.lower_closure_literal(body)?;
                    self.write_symbol(id, clo);
                } else {
                    let clo = self.lower_closure_literal(body)?;
                    self.ensure_local(id);
                    self.emit_stmt(Op::LocalSet(id, clo));
                }
                Ok(false)
            }
            StatKind::Assign { targets, exprs } => {
                let vals = exprs
                    .iter()
                    .map(|e| self.lower_expr(e))
                    .collect::<Result<Vec<_>, _>>()?;
                for (i, target) in targets.iter().enumerate() {
                    let v = vals[i];
                    self.lower_assign(target, v)?;
                }
                Ok(false)
            }
            StatKind::Call(e) => {
                self.lower_expr(e)?;
                Ok(false)
            }
            StatKind::Do(block) => self.lower_block(block),
            StatKind::While { cond, body } => self.lower_while(cond, body),
            StatKind::If { arms, else_block } => self.lower_if(arms, else_block),
            StatKind::NumericFor {
                var,
                start,
                end,
                step,
                body,
            } => self.lower_numeric_for(var, start, end, step.as_ref(), body),
            StatKind::GenericFor { names, iter, body } => self.lower_generic_for(names, iter, body),
        }
    }

    fn lower_assign(&mut self, target: &Expr, v: ValueId) -> Result<(), LowerError> {
        match &target.kind {
            ExprKind::Name(_) => {
                let id = match self.res.binding(target.span) {
                    Some(Binding::Local(id) | Binding::Upvalue(id)) => *id,
                    _ => {
                        return Err(LowerError::Internal(
                            "assignment target is not a mutable local".into(),
                        ));
                    }
                };
                self.write_symbol(id, v);
                Ok(())
            }
            ExprKind::Field { base, name } => {
                let b = self.lower_expr(base)?;
                self.emit_stmt(Op::FieldSet(b, name.node.clone(), v));
                Ok(())
            }
            ExprKind::Index { base, index } => {
                let b = self.lower_expr(base)?;
                let idx = self.lower_expr(index)?;
                match self.values[b as usize].clone() {
                    Type::Array(_) => self.emit_stmt(Op::ArraySet(b, idx, v)),
                    Type::Map(_) => self.emit_stmt(Op::MapSet(b, idx, v)),
                    other => {
                        return Err(LowerError::Internal(format!(
                            "index-assign to a non-array/map type {other}"
                        )));
                    }
                }
                Ok(())
            }
            _ => Err(LowerError::Internal("invalid assignment target".into())),
        }
    }

    // ---- control flow --------------------------------------------------------

    fn lower_while(&mut self, cond: &Expr, body: &ast::Block) -> Result<bool, LowerError> {
        let header = self.new_block();
        let body_b = self.new_block();
        let exit = self.new_block();

        self.terminate(Terminator::Jump(header));
        self.switch(header);
        let c = self.lower_expr(cond)?;
        self.terminate(Terminator::Branch {
            cond: c,
            then_blk: body_b,
            else_blk: exit,
        });

        self.switch(body_b);
        self.loop_exits.push(exit);
        let terminated = self.lower_block(body)?;
        self.loop_exits.pop();
        if !terminated {
            self.terminate(Terminator::Jump(header));
        }

        self.switch(exit);
        Ok(false)
    }

    fn lower_if(
        &mut self,
        arms: &[(Expr, ast::Block)],
        else_block: &Option<ast::Block>,
    ) -> Result<bool, LowerError> {
        // Blocks that fall through and need to jump to a shared merge block.
        let mut fallthroughs: Vec<BlockId> = Vec::new();

        for (cond, body) in arms {
            let c = self.lower_expr(cond)?;
            let body_b = self.new_block();
            let next_b = self.new_block();
            self.terminate(Terminator::Branch {
                cond: c,
                then_blk: body_b,
                else_blk: next_b,
            });
            self.switch(body_b);
            let terminated = self.lower_block(body)?;
            if !terminated {
                fallthroughs.push(self.current as BlockId);
            }
            self.switch(next_b);
        }

        // After the arm chain, `current` is the final else position.
        match else_block {
            Some(eb) => {
                let terminated = self.lower_block(eb)?;
                if !terminated {
                    fallthroughs.push(self.current as BlockId);
                }
            }
            None => fallthroughs.push(self.current as BlockId),
        }

        if fallthroughs.is_empty() {
            // Every path returned.
            return Ok(true);
        }

        let merge = self.new_block();
        for fb in fallthroughs {
            self.set_term(fb, Terminator::Jump(merge));
        }
        self.switch(merge);
        Ok(false)
    }

    fn lower_numeric_for(
        &mut self,
        var: &Ident,
        start: &Expr,
        end: &Expr,
        step: Option<&Expr>,
        body: &ast::Block,
    ) -> Result<bool, LowerError> {
        let var_id = self.local_id(var.span)?;
        self.ensure_local(var_id);

        let start_v = self.lower_expr(start)?;
        let end_l = self.fresh_temp(Type::Number);
        let end_v = self.lower_expr(end)?;
        self.emit_stmt(Op::LocalSet(end_l, end_v));
        let step_l = self.fresh_temp(Type::Number);
        let step_v = match step {
            Some(e) => self.lower_expr(e)?,
            None => self.emit_value(Op::ConstNumber(1.0), Type::Number),
        };
        self.emit_stmt(Op::LocalSet(step_l, step_v));
        // Create the loop variable's cell once (if captured), matching the interpreter's
        // single-slot reuse across iterations.
        self.bind_local(var_id, start_v);

        let header = self.new_block();
        let pos_check = self.new_block();
        let neg_check = self.new_block();
        let body_b = self.new_block();
        let latch = self.new_block();
        let exit = self.new_block();

        self.terminate(Terminator::Jump(header));

        // header: branch on sign of step.
        self.switch(header);
        let step_get = self.emit_value(Op::LocalGet(step_l), Type::Number);
        let zero = self.emit_value(Op::ConstNumber(0.0), Type::Number);
        let nonneg = self.emit_value(Op::Binary(BinOp::Ge, step_get, zero), Type::Bool);
        self.terminate(Terminator::Branch {
            cond: nonneg,
            then_blk: pos_check,
            else_blk: neg_check,
        });

        // pos_check: i <= end ?
        self.switch(pos_check);
        let i1 = self.read_symbol(var_id);
        let e1 = self.emit_value(Op::LocalGet(end_l), Type::Number);
        let le = self.emit_value(Op::Binary(BinOp::Le, i1, e1), Type::Bool);
        self.terminate(Terminator::Branch {
            cond: le,
            then_blk: body_b,
            else_blk: exit,
        });

        // neg_check: i >= end ?
        self.switch(neg_check);
        let i2 = self.read_symbol(var_id);
        let e2 = self.emit_value(Op::LocalGet(end_l), Type::Number);
        let ge = self.emit_value(Op::Binary(BinOp::Ge, i2, e2), Type::Bool);
        self.terminate(Terminator::Branch {
            cond: ge,
            then_blk: body_b,
            else_blk: exit,
        });

        // body
        self.switch(body_b);
        self.loop_exits.push(exit);
        let terminated = self.lower_block(body)?;
        self.loop_exits.pop();
        if !terminated {
            self.terminate(Terminator::Jump(latch));
        }

        // latch: i = i + step; back to header.
        self.switch(latch);
        let i3 = self.read_symbol(var_id);
        let s3 = self.emit_value(Op::LocalGet(step_l), Type::Number);
        let next = self.emit_value(Op::Binary(BinOp::Add, i3, s3), Type::Number);
        self.write_symbol(var_id, next);
        self.terminate(Terminator::Jump(header));

        self.switch(exit);
        Ok(false)
    }

    fn lower_generic_for(
        &mut self,
        names: &[Ident],
        iter: &IterExpr,
        body: &ast::Block,
    ) -> Result<bool, LowerError> {
        // Both ipairs and pairs are lowered to a counted loop over an array: ipairs over
        // the array itself, pairs over a snapshot of the map's keys.
        let (arg, is_pairs) = match iter {
            IterExpr::IPairs { arg, .. } => (arg, false),
            IterExpr::Pairs { arg, .. } => (arg, true),
        };
        let arg_v = self.lower_expr(arg)?;
        let (val_ty, key_ty) = match self.values[arg_v as usize].clone() {
            Type::Array(e) => (*e, Type::Number),
            Type::Map(v) => (*v, Type::String),
            other => {
                return Err(LowerError::Internal(format!(
                    "generic for over a non-iterable type {other}"
                )));
            }
        };

        // The thing we index: the array, or the map's key snapshot.
        let seq_ty = if is_pairs {
            Type::array(Type::String)
        } else {
            self.values[arg_v as usize].clone()
        };
        let seq_l = self.fresh_temp(seq_ty.clone());
        let seq_v = if is_pairs {
            self.emit_value(Op::MapKeys(arg_v), Type::array(Type::String))
        } else {
            arg_v
        };
        self.emit_stmt(Op::LocalSet(seq_l, seq_v));
        let map_l = if is_pairs {
            let l = self.fresh_temp(self.values[arg_v as usize].clone());
            self.emit_stmt(Op::LocalSet(l, arg_v));
            Some(l)
        } else {
            None
        };

        let idx_l = self.fresh_temp(Type::Number);
        let one = self.emit_value(Op::ConstNumber(1.0), Type::Number);
        self.emit_stmt(Op::LocalSet(idx_l, one));

        // Captured loop variables share a single cell, created once before the loop (matching
        // the interpreter's slot reuse), then written each iteration.
        for name in names {
            let id = self.local_id(name.span)?;
            if self.boxed.contains(&id) {
                let nil = self.emit_value(Op::ConstNil, Type::Nil);
                self.bind_local(id, nil);
            }
        }

        let header = self.new_block();
        let body_b = self.new_block();
        let latch = self.new_block();
        let exit = self.new_block();

        self.terminate(Terminator::Jump(header));

        // header: idx <= #seq ?
        self.switch(header);
        let seq_get = self.emit_value(Op::LocalGet(seq_l), seq_ty.clone());
        let len = self.emit_value(Op::Unary(UnOp::Len, seq_get), Type::Number);
        let idx_get = self.emit_value(Op::LocalGet(idx_l), Type::Number);
        let in_range = self.emit_value(Op::Binary(BinOp::Le, idx_get, len), Type::Bool);
        self.terminate(Terminator::Branch {
            cond: in_range,
            then_blk: body_b,
            else_blk: exit,
        });

        // body: bind key/value, run body.
        self.switch(body_b);
        let idx_now = self.emit_value(Op::LocalGet(idx_l), Type::Number);
        let seq_now = self.emit_value(Op::LocalGet(seq_l), seq_ty.clone());
        let elem = self.emit_value(Op::ArrayGet(seq_now, idx_now), {
            if is_pairs {
                Type::String
            } else {
                val_ty.clone()
            }
        });
        // key binding
        if let Some(kname) = names.first() {
            let kid = self.local_id(kname.span)?;
            self.ensure_local(kid);
            let key_v = if is_pairs {
                elem
            } else {
                self.emit_value(Op::LocalGet(idx_l), Type::Number)
            };
            let _ = &key_ty;
            self.write_symbol(kid, key_v);
        }
        // value binding
        if let Some(vname) = names.get(1) {
            let vid = self.local_id(vname.span)?;
            self.ensure_local(vid);
            let value_v = if is_pairs {
                let map_get = self.emit_value(
                    Op::LocalGet(map_l.unwrap()),
                    self.locals[&map_l.unwrap()].clone(),
                );
                self.emit_value(Op::MapGet(map_get, elem), val_ty.clone())
            } else {
                elem
            };
            self.write_symbol(vid, value_v);
        }

        self.loop_exits.push(exit);
        let terminated = self.lower_block(body)?;
        self.loop_exits.pop();
        if !terminated {
            self.terminate(Terminator::Jump(latch));
        }

        // latch: idx += 1
        self.switch(latch);
        let idx2 = self.emit_value(Op::LocalGet(idx_l), Type::Number);
        let one2 = self.emit_value(Op::ConstNumber(1.0), Type::Number);
        let nxt = self.emit_value(Op::Binary(BinOp::Add, idx2, one2), Type::Number);
        self.emit_stmt(Op::LocalSet(idx_l, nxt));
        self.terminate(Terminator::Jump(header));

        self.switch(exit);
        Ok(false)
    }

    // ---- expressions ---------------------------------------------------------

    fn lower_expr(&mut self, expr: &Expr) -> Result<ValueId, LowerError> {
        match &expr.kind {
            ExprKind::Nil => Ok(self.emit_value(Op::ConstNil, Type::Nil)),
            ExprKind::Bool(b) => Ok(self.emit_value(Op::ConstBool(*b), Type::Bool)),
            ExprKind::Number(n) => Ok(self.emit_value(Op::ConstNumber(*n), Type::Number)),
            ExprKind::Str(s) => Ok(self.emit_value(Op::ConstString(s.clone()), Type::String)),
            ExprKind::Paren(inner) => self.lower_expr(inner),
            ExprKind::Name(_) => self.lower_name(expr),
            ExprKind::Function(body) => self.lower_closure_literal(body),
            ExprKind::Field { base, name } => self.lower_field(base, name),
            ExprKind::Index { base, index } => self.lower_index(base, index),
            ExprKind::Call { callee, args } => self.lower_call(callee, args),
            ExprKind::MethodCall { method, .. } => Err(LowerError::Unsupported(format!(
                "method call `:{}` is not lowered to IR yet",
                method.node
            ))),
            ExprKind::Table(fields) => self.lower_table(fields),
            ExprKind::Unary { op, operand } => {
                let v = self.lower_expr(operand)?;
                let ty = match op {
                    UnOp::Neg => Type::Number,
                    UnOp::Not => Type::Bool,
                    UnOp::Len => Type::Number,
                };
                Ok(self.emit_value(Op::Unary(*op, v), ty))
            }
            ExprKind::Binary { op, lhs, rhs } => self.lower_binary(*op, lhs, rhs),
        }
    }

    fn lower_name(&mut self, expr: &Expr) -> Result<ValueId, LowerError> {
        let binding = self
            .res
            .binding(expr.span)
            .ok_or_else(|| LowerError::Internal("unresolved name".into()))?;
        match binding.clone() {
            Binding::Local(id) | Binding::Upvalue(id) => Ok(self.read_symbol(id)),
            Binding::TopConst(n) => {
                let ty = self.types.constants.get(&n).cloned().unwrap_or(Type::Error);
                Ok(self.emit_value(Op::ConstRef(n), ty))
            }
            Binding::Memory(n) => {
                let ty = self.cfg.memory.get(&n).cloned().unwrap_or(Type::Error);
                Ok(self.emit_value(Op::MemoryRef(n), ty))
            }
            Binding::TopFunction(_) | Binding::HostFunction(_) => Err(LowerError::Unsupported(
                "using a named top-level or host function as a first-class value is not \
                 supported; only anonymous and `local function` closures are first-class"
                    .into(),
            )),
            Binding::Builtin(ns) => Err(LowerError::Internal(format!(
                "builtin namespace `{ns}` used as a value"
            ))),
        }
    }

    fn lower_field(&mut self, base: &Expr, name: &Ident) -> Result<ValueId, LowerError> {
        if let Some(ns) = self.builtin_namespace(base) {
            let ty = crate::runtime::builtins::namespace_field_type(ns, &name.node)
                .unwrap_or(Type::Error);
            return Ok(self.emit_value(Op::NamespaceField(ns.to_string(), name.node.clone()), ty));
        }
        let b = self.lower_expr(base)?;
        let ty = match self.values[b as usize].clone() {
            Type::Record(fields) => fields.get(&name.node).cloned().unwrap_or(Type::Error),
            Type::Map(v) => Type::optional(*v),
            _ => Type::Error,
        };
        Ok(self.emit_value(Op::FieldGet(b, name.node.clone()), ty))
    }

    fn lower_index(&mut self, base: &Expr, index: &Expr) -> Result<ValueId, LowerError> {
        let b = self.lower_expr(base)?;
        let idx = self.lower_expr(index)?;
        match self.values[b as usize].clone() {
            Type::Array(e) => Ok(self.emit_value(Op::ArrayGet(b, idx), Type::optional(*e))),
            Type::Map(v) => Ok(self.emit_value(Op::MapGet(b, idx), Type::optional(*v))),
            Type::Record(fields) => {
                // A record indexed by a string literal — read the field.
                if let ExprKind::Str(key) = &index.kind {
                    let ty = fields.get(key).cloned().unwrap_or(Type::Error);
                    Ok(self.emit_value(Op::FieldGet(b, key.clone()), ty))
                } else {
                    Err(LowerError::Internal(
                        "record indexed by a non-literal key".into(),
                    ))
                }
            }
            other => Err(LowerError::Internal(format!("indexing a {other}"))),
        }
    }

    fn lower_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<ValueId, LowerError> {
        // Plain builtin: tostring / tonumber.
        if let ExprKind::Name(_) = &callee.kind
            && let Some(Binding::Builtin(ns)) = self.res.binding(callee.span)
        {
            let argv = self.lower_args(args)?;
            let ty = crate::runtime::builtins::value_sig(ns)
                .map(|s| s.ret)
                .unwrap_or(Type::Error);
            return Ok(self.emit_value(Op::CallBuiltinValue(ns.to_string(), argv), ty));
        }
        // Builtin namespace member: math.* / string.*.
        if let ExprKind::Field { base, name } = &callee.kind
            && let Some(ns) = self.builtin_namespace(base)
        {
            let argv = self.lower_args(args)?;
            let ty = crate::runtime::builtins::member_sig(ns, &name.node)
                .map(|s| s.ret)
                .unwrap_or(Type::Error);
            return Ok(self.emit_value(
                Op::CallBuiltinMember(ns.to_string(), name.node.clone(), argv),
                ty,
            ));
        }
        // Direct call of a named top-level / host function keeps the fast (typed) path.
        if let ExprKind::Name(_) = &callee.kind {
            match self.res.binding(callee.span).cloned() {
                Some(Binding::TopFunction(n)) => {
                    let argv = self.lower_args(args)?;
                    let ty = self
                        .types
                        .functions
                        .get(&n)
                        .map(|ft| (*ft.ret).clone())
                        .unwrap_or(Type::Error);
                    return Ok(self.emit_value(Op::CallScript(n, argv), ty));
                }
                Some(Binding::HostFunction(n)) => {
                    let argv = self.lower_args(args)?;
                    let ty = self
                        .cfg
                        .host_functions
                        .get(&n)
                        .map(|ft| (*ft.ret).clone())
                        .unwrap_or(Type::Error);
                    return Ok(self.emit_value(Op::CallHost(n, argv), ty));
                }
                _ => {}
            }
        }
        // Otherwise the callee is a first-class function value (a closure, or a fn-typed
        // local/param/upvalue): evaluate it and call it indirectly.
        let cv = self.lower_expr(callee)?;
        let ret = match self.values[cv as usize].clone() {
            Type::Function(ft) => (*ft.ret).clone(),
            _ => Type::Error,
        };
        let argv = self.lower_args(args)?;
        Ok(self.emit_value(Op::CallValue(cv, argv), ret))
    }

    fn lower_args(&mut self, args: &[Expr]) -> Result<Vec<ValueId>, LowerError> {
        args.iter().map(|a| self.lower_expr(a)).collect()
    }

    fn lower_table(&mut self, fields: &[Field]) -> Result<ValueId, LowerError> {
        let all_positional =
            !fields.is_empty() && fields.iter().all(|f| matches!(f, Field::Positional(_)));
        if all_positional || fields.is_empty() {
            let mut elems = Vec::with_capacity(fields.len());
            let mut elem_ty = Type::Error;
            for f in fields {
                if let Field::Positional(e) = f {
                    let v = self.lower_expr(e)?;
                    elem_ty = self.values[v as usize].clone();
                    elems.push(v);
                }
            }
            return Ok(self.emit_value(Op::MakeArray(elems), Type::array(elem_ty)));
        }
        let mut pairs = Vec::with_capacity(fields.len());
        let mut rec = BTreeMap::new();
        for f in fields {
            match f {
                Field::Named { name, value } => {
                    let v = self.lower_expr(value)?;
                    rec.insert(name.node.clone(), self.values[v as usize].clone());
                    pairs.push((name.node.clone(), v));
                }
                Field::Keyed { key, value } => {
                    let ExprKind::Str(k) = &key.kind else {
                        return Err(LowerError::Internal(
                            "non-literal map key reached lowering".into(),
                        ));
                    };
                    let v = self.lower_expr(value)?;
                    pairs.push((k.clone(), v));
                }
                Field::Positional(_) => {
                    return Err(LowerError::Internal("mixed table shape".into()));
                }
            }
        }
        // Record literals get a record type; keyed literals are maps. The checker keeps
        // these from mixing, so all-named ⇒ record.
        let ty = if rec.len() == pairs.len() {
            Type::Record(rec)
        } else {
            Type::Error
        };
        Ok(self.emit_value(Op::MakeTable(pairs), ty))
    }

    fn lower_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<ValueId, LowerError> {
        use BinOp::*;
        match op {
            And | Or => self.lower_short_circuit(op, lhs, rhs),
            _ => {
                let l = self.lower_expr(lhs)?;
                let r = self.lower_expr(rhs)?;
                let ty = match op {
                    Add | Sub | Mul | Div | FloorDiv | Mod | Pow => Type::Number,
                    Concat => Type::String,
                    Lt | Le | Gt | Ge | Eq | Ne => Type::Bool,
                    And | Or => unreachable!(),
                };
                Ok(self.emit_value(Op::Binary(op, l, r), ty))
            }
        }
    }

    /// Short-circuiting `and`/`or` lowered to a branch and a result temporary.
    fn lower_short_circuit(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Result<ValueId, LowerError> {
        let l = self.lower_expr(lhs)?;
        let result_ty = self.values[l as usize].clone();
        let result_l = self.fresh_temp(result_ty.clone());
        self.emit_stmt(Op::LocalSet(result_l, l));
        let cond = self.emit_value(Op::Truthy(l), Type::Bool);

        let rhs_b = self.new_block();
        let merge = self.new_block();

        // `and`: evaluate rhs when lhs is truthy. `or`: when lhs is falsy.
        let (then_blk, else_blk) = match op {
            BinOp::And => (rhs_b, merge),
            BinOp::Or => (merge, rhs_b),
            _ => unreachable!(),
        };
        self.terminate(Terminator::Branch {
            cond,
            then_blk,
            else_blk,
        });

        self.switch(rhs_b);
        let r = self.lower_expr(rhs)?;
        self.emit_stmt(Op::LocalSet(result_l, r));
        self.terminate(Terminator::Jump(merge));

        self.switch(merge);
        Ok(self.emit_value(Op::LocalGet(result_l), result_ty))
    }

    /// If `expr` is a bare name bound to a builtin namespace, return it.
    fn builtin_namespace(&self, expr: &Expr) -> Option<&'static str> {
        if let ExprKind::Name(_) = &expr.kind
            && let Some(Binding::Builtin(ns)) = self.res.binding(expr.span)
        {
            return Some(ns);
        }
        None
    }
}

/// Structural validity check on a lowered [`Program`].
pub fn verify(program: &Program) -> Result<(), LowerError> {
    for f in program.functions.values().chain(program.constants.values()) {
        verify_function(f)?;
    }
    Ok(())
}

fn verify_function(f: &Function) -> Result<(), LowerError> {
    let nblocks = f.blocks.len() as BlockId;
    let nvalues = f.values.len() as ValueId;
    let check_block = |b: BlockId| -> Result<(), LowerError> {
        if b >= nblocks {
            return Err(LowerError::Internal(format!(
                "function `{}` references undefined block {b}",
                f.name
            )));
        }
        Ok(())
    };
    let check_value = |v: ValueId| -> Result<(), LowerError> {
        if v >= nvalues {
            return Err(LowerError::Internal(format!(
                "function `{}` references undefined value {v}",
                f.name
            )));
        }
        Ok(())
    };
    if f.entry >= nblocks {
        return Err(LowerError::Internal(format!(
            "function `{}` has an out-of-range entry block",
            f.name
        )));
    }
    for (i, block) in f.blocks.iter().enumerate() {
        if block.id as usize != i {
            return Err(LowerError::Internal(format!(
                "function `{}` block {i} has mismatched id {}",
                f.name, block.id
            )));
        }
        for instr in &block.instrs {
            if let Some(d) = instr.dest {
                check_value(d)?;
            }
        }
        match &block.term {
            Terminator::Return(Some(v)) => check_value(*v)?,
            Terminator::Return(None) | Terminator::Unreachable => {}
            Terminator::Jump(b) => check_block(*b)?,
            Terminator::Branch {
                cond,
                then_blk,
                else_blk,
            } => {
                check_value(*cond)?;
                check_block(*then_blk)?;
                check_block(*else_blk)?;
            }
        }
    }
    Ok(())
}

#[cfg(feature = "interp")]
pub use vm::Vm;

#[cfg(feature = "interp")]
mod vm {
    //! Direct interpreter for the IR — the second semantics oracle (see module docs).

    use std::collections::HashMap;

    use super::*;
    use crate::runtime::builtins::{field_value, member_call, value_call};
    use crate::value::{RunError, Value};

    type Native = std::rc::Rc<dyn Fn(&[Value]) -> Result<Value, RunError>>;

    /// Executes a lowered [`Program`].
    pub struct Vm<'a> {
        program: &'a Program,
        host: HashMap<String, Native>,
        memory: HashMap<String, Value>,
        const_cache: HashMap<String, Value>,
    }

    struct Activation {
        locals: HashMap<LocalId, Value>,
        values: HashMap<ValueId, Value>,
    }

    impl<'a> Vm<'a> {
        pub fn new(program: &'a Program) -> Self {
            Vm {
                program,
                host: HashMap::new(),
                memory: HashMap::new(),
                const_cache: HashMap::new(),
            }
        }

        pub fn set_host_function<F>(&mut self, name: impl Into<String>, f: F)
        where
            F: Fn(&[Value]) -> Result<Value, RunError> + 'static,
        {
            self.host.insert(name.into(), std::rc::Rc::new(f));
        }

        pub fn set_memory(&mut self, name: impl Into<String>, value: Value) {
            self.memory.insert(name.into(), value);
        }

        pub fn memory(&self, name: &str) -> Option<Value> {
            self.memory.get(name).cloned()
        }

        /// Call an exported function by name.
        pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
            let target = self
                .program
                .exports
                .get(name)
                .ok_or_else(|| RunError::UnknownExport(name.to_string()))?;
            match target {
                ExportTarget::Function(f) => {
                    let func = self.lookup_function(f)?;
                    self.run_function(func, args)
                }
                ExportTarget::Const(c) => self.const_value(c),
            }
        }

        /// Host-invoke a closure value previously returned by a call. The closure's captured
        /// cells are shared (by `Rc`), so upvalue writes persist across host calls.
        pub fn call_value(&mut self, callee: Value, args: Vec<Value>) -> Result<Value, RunError> {
            let code = match &callee {
                Value::Closure(c) => c.code.clone(),
                other => {
                    return Err(RunError::Internal(format!(
                        "call_value on a {} value",
                        other.type_name()
                    )));
                }
            };
            let func = self.lookup_function(&code)?;
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(callee);
            argv.extend(args);
            self.run_function(func, argv)
        }

        fn lookup_function(&self, name: &str) -> Result<&'a Function, RunError> {
            self.program
                .functions
                .get(name)
                .ok_or_else(|| RunError::Internal(format!("missing IR function `{name}`")))
        }

        fn const_value(&mut self, name: &str) -> Result<Value, RunError> {
            if let Some(v) = self.const_cache.get(name) {
                return Ok(v.clone());
            }
            let func = self
                .program
                .constants
                .get(name)
                .ok_or_else(|| RunError::Internal(format!("missing IR constant `{name}`")))?;
            let v = self.run_function(func, Vec::new())?;
            self.const_cache.insert(name.to_string(), v.clone());
            Ok(v)
        }

        fn run_function(&mut self, func: &Function, args: Vec<Value>) -> Result<Value, RunError> {
            let mut act = Activation {
                locals: HashMap::new(),
                values: HashMap::new(),
            };
            for (i, &pid) in func.params.iter().enumerate() {
                act.locals
                    .insert(pid, args.get(i).cloned().unwrap_or(Value::Nil));
            }

            let mut current = func.entry;
            loop {
                let block = &func.blocks[current as usize];
                for instr in &block.instrs {
                    let v = self.eval_op(&instr.op, &mut act)?;
                    if let Some(dest) = instr.dest {
                        act.values.insert(dest, v);
                    }
                }
                match &block.term {
                    Terminator::Return(Some(v)) => return self.val(&act, *v),
                    Terminator::Return(None) => return Ok(Value::Nil),
                    Terminator::Jump(b) => current = *b,
                    Terminator::Branch {
                        cond,
                        then_blk,
                        else_blk,
                    } => {
                        let c = self.val(&act, *cond)?;
                        current = if truthy(&c) { *then_blk } else { *else_blk };
                    }
                    Terminator::Unreachable => {
                        return Err(RunError::Internal("reached an unreachable IR block".into()));
                    }
                }
            }
        }

        fn val(&self, act: &Activation, v: ValueId) -> Result<Value, RunError> {
            act.values
                .get(&v)
                .cloned()
                .ok_or_else(|| RunError::Internal(format!("use of undefined IR value {v}")))
        }

        fn eval_op(&mut self, op: &Op, act: &mut Activation) -> Result<Value, RunError> {
            Ok(match op {
                Op::ConstNil => Value::Nil,
                Op::ConstBool(b) => Value::Bool(*b),
                Op::ConstNumber(n) => Value::Number(*n),
                Op::ConstString(s) => Value::string(s.clone()),
                Op::LocalGet(id) => act
                    .locals
                    .get(id)
                    .cloned()
                    .ok_or_else(|| RunError::Internal(format!("read of unset local {id}")))?,
                Op::LocalSet(id, v) => {
                    let val = self.val(act, *v)?;
                    act.locals.insert(*id, val);
                    Value::Nil
                }
                Op::Unary(uop, v) => {
                    let val = self.val(act, *v)?;
                    eval_unary(*uop, val)?
                }
                Op::Binary(bop, l, r) => {
                    let lv = self.val(act, *l)?;
                    let rv = self.val(act, *r)?;
                    eval_binary(*bop, lv, rv)?
                }
                Op::Truthy(v) => Value::Bool(truthy(&self.val(act, *v)?)),
                Op::MakeArray(elems) => {
                    let items = elems
                        .iter()
                        .map(|e| self.val(act, *e))
                        .collect::<Result<Vec<_>, _>>()?;
                    Value::array(items)
                }
                Op::MakeTable(pairs) => {
                    let mut map = std::collections::BTreeMap::new();
                    for (k, v) in pairs {
                        map.insert(k.clone(), self.val(act, *v)?);
                    }
                    Value::table(map)
                }
                Op::ArrayGet(base, idx) => {
                    let b = self.val(act, *base)?;
                    let i = self.val(act, *idx)?;
                    array_get(&b, &i)?
                }
                Op::ArraySet(base, idx, v) => {
                    let b = self.val(act, *base)?;
                    let i = self.val(act, *idx)?;
                    let val = self.val(act, *v)?;
                    array_set(&b, &i, val)?;
                    Value::Nil
                }
                Op::MapGet(base, key) => {
                    let b = self.val(act, *base)?;
                    let k = self.val(act, *key)?;
                    table_get(&b, &k)?
                }
                Op::MapSet(base, key, v) => {
                    let b = self.val(act, *base)?;
                    let k = self.val(act, *key)?;
                    let val = self.val(act, *v)?;
                    table_set(&b, &k, val)?;
                    Value::Nil
                }
                Op::FieldGet(base, name) => {
                    let b = self.val(act, *base)?;
                    b.field(name).unwrap_or(Value::Nil)
                }
                Op::FieldSet(base, name, v) => {
                    let b = self.val(act, *base)?;
                    let val = self.val(act, *v)?;
                    table_set(&b, &Value::string(name.clone()), val)?;
                    Value::Nil
                }
                Op::MapKeys(base) => {
                    let b = self.val(act, *base)?;
                    map_keys(&b)?
                }
                Op::CallScript(name, args) => {
                    let argv = self.eval_args(act, args)?;
                    let func = self.lookup_function(name)?;
                    self.run_function(func, argv)?
                }
                Op::CallHost(name, args) => {
                    let argv = self.eval_args(act, args)?;
                    let f =
                        self.host.get(name).cloned().ok_or_else(|| {
                            RunError::Runtime(format!("host fn `{name}` not set"))
                        })?;
                    f(&argv)?
                }
                Op::CallBuiltinValue(ns, args) => {
                    let argv = self.eval_args(act, args)?;
                    value_call(ns, &argv)?
                }
                Op::CallBuiltinMember(ns, member, args) => {
                    let argv = self.eval_args(act, args)?;
                    member_call(ns, member, &argv)?
                }
                Op::ConstRef(name) => self.const_value(name)?,
                Op::MemoryRef(name) => self
                    .memory
                    .get(name)
                    .cloned()
                    .ok_or_else(|| RunError::Runtime(format!("memory `{name}` not set")))?,
                Op::NamespaceField(ns, field) => field_value(ns, field)?,

                Op::MakeCell(v) => {
                    let val = self.val(act, *v)?;
                    Value::Cell(std::rc::Rc::new(std::cell::RefCell::new(val)))
                }
                Op::CellGet(cell) => match self.val(act, *cell)? {
                    Value::Cell(c) => c.borrow().clone(),
                    other => {
                        return Err(RunError::Internal(format!(
                            "CellGet on a {} value",
                            other.type_name()
                        )));
                    }
                },
                Op::CellSet(cell, v) => {
                    let val = self.val(act, *v)?;
                    match self.val(act, *cell)? {
                        Value::Cell(c) => *c.borrow_mut() = val,
                        other => {
                            return Err(RunError::Internal(format!(
                                "CellSet on a {} value",
                                other.type_name()
                            )));
                        }
                    }
                    Value::Nil
                }
                Op::ClosureEnvGet(clo, i) => match self.val(act, *clo)? {
                    Value::Closure(c) => c.env.get(*i as usize).cloned().ok_or_else(|| {
                        RunError::Internal(format!("upvalue {i} out of range"))
                    })?,
                    other => {
                        return Err(RunError::Internal(format!(
                            "ClosureEnvGet on a {} value",
                            other.type_name()
                        )));
                    }
                },
                Op::MakeClosure(code, cells) => {
                    let env = cells
                        .iter()
                        .map(|c| self.val(act, *c))
                        .collect::<Result<Vec<_>, _>>()?;
                    Value::Closure(std::rc::Rc::new(crate::value::ClosureObj {
                        code: code.clone(),
                        env,
                        keepalive: None,
                    }))
                }
                Op::CallValue(callee, args) => {
                    let cv = self.val(act, *callee)?;
                    let code = match &cv {
                        Value::Closure(c) => c.code.clone(),
                        other => {
                            return Err(RunError::Internal(format!(
                                "CallValue on a {} value",
                                other.type_name()
                            )));
                        }
                    };
                    let func = self.lookup_function(&code)?;
                    // The closure itself is the hidden env parameter (params[0]); real args
                    // follow.
                    let mut argv = Vec::with_capacity(args.len() + 1);
                    argv.push(cv);
                    for a in args {
                        argv.push(self.val(act, *a)?);
                    }
                    self.run_function(func, argv)?
                }
            })
        }

        fn eval_args(
            &mut self,
            act: &mut Activation,
            args: &[ValueId],
        ) -> Result<Vec<Value>, RunError> {
            args.iter().map(|a| self.val(act, *a)).collect()
        }
    }

    fn truthy(v: &Value) -> bool {
        !matches!(v, Value::Nil | Value::Bool(false))
    }

    fn num(v: &Value) -> Result<f64, RunError> {
        v.as_f64()
            .ok_or_else(|| RunError::Internal("expected a number".into()))
    }

    fn eval_unary(op: UnOp, v: Value) -> Result<Value, RunError> {
        Ok(match op {
            UnOp::Neg => Value::Number(-num(&v)?),
            UnOp::Not => Value::Bool(!truthy(&v)),
            UnOp::Len => match v {
                Value::Str(s) => Value::Number(s.len() as f64),
                Value::Array(a) => Value::Number(a.borrow().len() as f64),
                _ => return Err(RunError::Internal("`#` on a non-string/array".into())),
            },
        })
    }

    fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value, RunError> {
        use BinOp::*;
        Ok(match op {
            Add | Sub | Mul | Div | FloorDiv | Mod | Pow => {
                let (a, b) = (num(&l)?, num(&r)?);
                Value::Number(match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    Div => a / b,
                    FloorDiv => (a / b).floor(),
                    Mod => a - (a / b).floor() * b,
                    Pow => a.powf(b),
                    _ => unreachable!(),
                })
            }
            Concat => {
                let a = l
                    .as_string()
                    .ok_or_else(|| RunError::Internal("concat lhs".into()))?;
                let b = r
                    .as_string()
                    .ok_or_else(|| RunError::Internal("concat rhs".into()))?;
                Value::string(format!("{a}{b}"))
            }
            Lt | Le | Gt | Ge => {
                let ord = compare(&l, &r)?;
                Value::Bool(match op {
                    Lt => ord == std::cmp::Ordering::Less,
                    Le => ord != std::cmp::Ordering::Greater,
                    Gt => ord == std::cmp::Ordering::Greater,
                    Ge => ord != std::cmp::Ordering::Less,
                    _ => unreachable!(),
                })
            }
            Eq => Value::Bool(scalar_eq(&l, &r)),
            Ne => Value::Bool(!scalar_eq(&l, &r)),
            And | Or => unreachable!("short-circuited in lowering"),
        })
    }

    fn compare(l: &Value, r: &Value) -> Result<std::cmp::Ordering, RunError> {
        match (l, r) {
            (Value::Number(a), Value::Number(b)) => a
                .partial_cmp(b)
                .ok_or_else(|| RunError::Runtime("NaN comparison".into())),
            (Value::Str(a), Value::Str(b)) => Ok(a.cmp(b)),
            _ => Err(RunError::Internal("bad comparison operands".into())),
        }
    }

    fn scalar_eq(l: &Value, r: &Value) -> bool {
        match (l, r) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Nil, _) | (_, Value::Nil) => false,
            _ => false,
        }
    }

    fn array_get(base: &Value, idx: &Value) -> Result<Value, RunError> {
        match base {
            Value::Array(a) => {
                let i = num(idx)?;
                if i < 1.0 || i.fract() != 0.0 {
                    return Ok(Value::Nil);
                }
                Ok(a.borrow()
                    .get(i as usize - 1)
                    .cloned()
                    .unwrap_or(Value::Nil))
            }
            _ => Err(RunError::Internal("ArrayGet on a non-array".into())),
        }
    }

    fn array_set(base: &Value, idx: &Value, val: Value) -> Result<(), RunError> {
        match base {
            Value::Array(a) => {
                let i = num(idx)?;
                if i < 1.0 || i.fract() != 0.0 {
                    return Err(RunError::Runtime("non-positive-integer array index".into()));
                }
                let i = i as usize;
                let len = a.borrow().len();
                if i <= len {
                    a.borrow_mut()[i - 1] = val;
                    Ok(())
                } else if i == len + 1 {
                    a.borrow_mut().push(val);
                    Ok(())
                } else {
                    Err(RunError::Runtime(format!(
                        "array index {i} out of range (len {len})"
                    )))
                }
            }
            _ => Err(RunError::Internal("ArraySet on a non-array".into())),
        }
    }

    fn table_get(base: &Value, key: &Value) -> Result<Value, RunError> {
        match (base, key) {
            (Value::Table(t), Value::Str(k)) => {
                Ok(t.borrow().get(k.as_ref()).cloned().unwrap_or(Value::Nil))
            }
            _ => Err(RunError::Internal(
                "MapGet on a non-table/non-string key".into(),
            )),
        }
    }

    fn table_set(base: &Value, key: &Value, val: Value) -> Result<(), RunError> {
        match (base, key) {
            (Value::Table(t), Value::Str(k)) => {
                t.borrow_mut().insert(k.to_string(), val);
                Ok(())
            }
            _ => Err(RunError::Internal(
                "MapSet on a non-table/non-string key".into(),
            )),
        }
    }

    fn map_keys(base: &Value) -> Result<Value, RunError> {
        match base {
            Value::Table(t) => Ok(Value::array(
                t.borrow()
                    .keys()
                    .map(|k| Value::string(k.clone()))
                    .collect(),
            )),
            _ => Err(RunError::Internal("MapKeys on a non-table".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(src: &str, cfg: &TypeConfig) -> Program {
        let module = crate::parse(src).expect("parse");
        let res = crate::resolve::resolve(&module, &cfg.to_resolve_config()).expect("resolve");
        let types = crate::types::typecheck(&module, &res, cfg).expect("typecheck");
        let program = lower(&module, &res, &types, cfg).expect("lower");
        verify(&program).expect("verify");
        program
    }

    #[test]
    fn lowers_and_verifies_simple_function() {
        let p = build(
            "function add(a, b) return a + b end",
            &TypeConfig::default(),
        );
        assert!(p.functions.contains_key("add"));
        assert_eq!(
            p.exports.get("add"),
            Some(&ExportTarget::Function("add".into()))
        );
        let f = &p.functions["add"];
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.ret, Type::Number);
    }

    #[test]
    fn lowers_const_and_curated_export() {
        let p = build(
            "K = 7\nfunction impl() return K end\nreturn { run = impl }",
            &TypeConfig::default(),
        );
        assert!(p.constants.contains_key("K"));
        assert_eq!(
            p.exports.get("run"),
            Some(&ExportTarget::Function("impl".into()))
        );
        assert!(!p.exports.contains_key("impl"));
    }

    #[test]
    fn closures_are_lowered_and_lifted() {
        let p = build(
            "function make(b) local f = function(x) return x + b end return f(1) end",
            &TypeConfig::default(),
        );
        // The anonymous function was lifted into its own IR function (synthetic `$c` name),
        // and `make` itself verifies.
        assert!(
            p.functions.keys().any(|k| k.contains("$c")),
            "expected a lifted closure function, got {:?}",
            p.functions.keys().collect::<Vec<_>>()
        );
        let lifted = p.functions.values().find(|f| f.is_closure).expect("a closure fn");
        assert!(lifted.is_closure);
    }
}
