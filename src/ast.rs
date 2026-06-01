//! Abstract syntax tree for Grindlang.
//!
//! The tree mirrors `SPEC.md` §3. Expressions and statements carry their [`Span`] in a
//! single `span` field next to a `*Kind` enum so spans are uniform and matching stays
//! ergonomic. Smaller nodes use the [`Spanned`] wrapper.
//!
//! This is the *untyped* AST produced by the parser. Type information (Phase 3) is
//! attached by later phases, not here.

use crate::diagnostics::Span;

/// A value paired with the source span it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}

/// An identifier occurrence.
pub type Ident = Spanned<String>;

/// A whole compiled script: a set of export declarations and an optional curated
/// export table (`return { ... }`).
#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub decls: Vec<TopDecl>,
    /// Fields of the trailing `return { ... }`, if present.
    pub export: Option<Spanned<Vec<Field>>>,
    pub span: Span,
}

/// A top-level declaration: an exported function or an exported constant.
#[derive(Clone, Debug, PartialEq)]
pub enum TopDecl {
    Function(FuncDecl),
    Const(ConstDecl),
}

#[derive(Clone, Debug, PartialEq)]
pub struct FuncDecl {
    pub name: Ident,
    pub body: FuncBody,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConstDecl {
    pub name: Ident,
    /// Validated as compile-time-constant in a later phase; parsed as a general expr.
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FuncBody {
    pub params: Vec<Ident>,
    pub block: Block,
    pub span: Span,
}

/// A sequence of statements with an optional terminating `return`.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub stats: Vec<Stat>,
    pub ret: Option<RetStat>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RetStat {
    pub exprs: Vec<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    pub kind: StatKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatKind {
    /// A lone `;`.
    Empty,
    /// `local a, b = e1, e2` (the init list may be empty).
    Local { names: Vec<Ident>, exprs: Vec<Expr> },
    /// `local function f(...) ... end`.
    LocalFunction { name: Ident, body: FuncBody },
    /// `a, b.c, d[e] = e1, e2`. Targets are assignable expressions (validated by parser).
    Assign {
        targets: Vec<Expr>,
        exprs: Vec<Expr>,
    },
    /// An expression statement; always a call expression.
    Call(Expr),
    /// `do ... end`.
    Do(Block),
    /// `while cond do ... end`.
    While { cond: Expr, body: Block },
    /// `if c1 then b1 elseif c2 then b2 ... else be end`.
    If {
        arms: Vec<(Expr, Block)>,
        else_block: Option<Block>,
    },
    /// `for v = start, end[, step] do ... end`.
    NumericFor {
        var: Ident,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Block,
    },
    /// `for names in ipairs(e)/pairs(e) do ... end`.
    GenericFor {
        names: Vec<Ident>,
        iter: IterExpr,
        body: Block,
    },
    /// `break`.
    Break,
}

/// The restricted iterator forms permitted in a generic `for` (SPEC §3.3).
#[derive(Clone, Debug, PartialEq)]
pub enum IterExpr {
    IPairs { arg: Expr, span: Span },
    Pairs { arg: Expr, span: Span },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Nil,
    Bool(bool),
    Number(f64),
    Str(String),
    /// Anonymous function `function(...) ... end` (in-body only; SPEC §5.5).
    Function(FuncBody),
    /// A bare name reference.
    Name(String),
    /// `base[index]`.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    /// `base.name`.
    Field {
        base: Box<Expr>,
        name: Ident,
    },
    /// `callee(args)`.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `receiver:method(args)` (host userdata only; SPEC §7).
    MethodCall {
        receiver: Box<Expr>,
        method: Ident,
        args: Vec<Expr>,
    },
    /// `{ ... }`.
    Table(Vec<Field>),
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    /// `( e )` — preserved so multi-value adjustment rules can see the parens later.
    Paren(Box<Expr>),
}

/// A field inside a table constructor.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    /// `expr` — positional (array element).
    Positional(Expr),
    /// `name = expr` — record field.
    Named { name: Ident, value: Expr },
    /// `[key] = expr` — computed/string-keyed entry.
    Keyed { key: Expr, value: Expr },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FloorDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "^",
            BinOp::Concat => "..",
            BinOp::Eq => "==",
            BinOp::Ne => "~=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum UnOp {
    /// `-`
    Neg,
    /// `not`
    Not,
    /// `#`
    Len,
}

impl UnOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "not",
            UnOp::Len => "#",
        }
    }
}
