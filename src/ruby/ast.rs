//! The Ruby-shaped syntax tree that the Ruby parser builds (`SPEC-RUBY.md` §3).
//!
//! It records what was written, not what it means: `x` may be a local or a call with no
//! arguments, `r.name` a field read or a method, and `a[i]` an array or a map access. Telling
//! these apart, and everything else about meaning, happens when the tree is translated into the
//! shared [`crate::ast::Module`] (`PLAN.md` Phase 10, step R2).
//!
//! Like [`crate::ast`], each expression and statement pairs a `*Kind` enum with its [`Span`].

use crate::ast::Spanned;
use crate::diagnostics::Span;
use crate::lexer::DocComment;

/// A name occurrence.
pub type Ident = Spanned<String>;

/// A whole Ruby-surface script: its top-level items in source order.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub items: Vec<Item>,
    pub span: Span,
}

/// A top-level item.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Def(Def),
    /// A bare `private` or `public`, which sets the visibility of the `def`s after it.
    Visibility(Spanned<Visibility>),
    /// A statement of top-level code.
    Stmt(Stmt),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Public,
}

/// `def name(params) … end`, only ever at the top level.
#[derive(Clone, Debug, PartialEq)]
pub struct Def {
    pub name: Ident,
    pub params: Vec<Ident>,
    pub body: Body,
    /// The `private` or `public` written in front of this `def`, if any.
    pub visibility: Option<Visibility>,
    /// The comment lines directly above the `def`, with no blank line between, which may hold
    /// `# @param` and `# @return` annotations. Each one's text follows its `#`.
    pub docs: Vec<DocComment>,
    pub span: Span,
}

/// A sequence of statements: the body of a `def`, block, lambda, branch, or loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StmtKind {
    /// An expression used as a statement: a call, a call without parentheses, an `if` or
    /// `case`, or a value, which is what a body's last statement returns (`SPEC-RUBY.md` §4.2).
    Expr(Expr),
    /// `a, b.c, d[i] = e1, e2`. Every target is a name, constant, `@name`, field, or index.
    Assign {
        targets: Vec<Expr>,
        values: Vec<Expr>,
    },
    /// `x += e`, `x ||= e`, and the other compound assignments.
    OpAssign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    /// `while cond … end`, or with `until`, `until cond … end`.
    While {
        cond: Expr,
        body: Body,
        until: bool,
    },
    /// `loop do … end`.
    Loop(Body),
    Break,
    Next,
    /// `return`, `return e`, or `return a, b`.
    Return(Vec<Expr>),
    /// `stmt if cond`, and likewise with `unless`, `while`, or `until`.
    Modified {
        stmt: Box<Stmt>,
        modifier: Modifier,
        cond: Expr,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    If,
    Unless,
    While,
    Until,
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
    /// A number literal. `-2` written together is one negative literal, as in Ruby, where
    /// `-2.abs` is `(-2).abs`.
    Number(f64),
    /// A string literal: text, with `#{…}` interpolations between. Empty text is left out, so
    /// `""` has no parts.
    Str(Vec<StrPart>),
    /// `:name`, holding `name`.
    Symbol(String),
    /// A name starting with a lowercase letter or `_`: a local, or a call with no arguments and
    /// no parentheses.
    Name(String),
    /// A name starting with an uppercase letter.
    Const(String),
    /// `@name`, a field of host memory, holding `name`.
    Ivar(String),
    /// `[a, b, c]`.
    Array(Vec<Expr>),
    /// `{ key: v, "k" => v }`.
    Hash(Vec<Pair>),
    /// `->(x) { … }`, `-> do … end`, or `lambda { |x| … }`.
    Lambda(Box<Block>),
    /// `lo..hi`, or with `exclusive`, `lo...hi`.
    Range {
        lo: Box<Expr>,
        hi: Box<Expr>,
        exclusive: bool,
    },
    /// `name(args)`, `name args` at the start of a statement, or `name` with a block.
    Call {
        name: Ident,
        args: Vec<Arg>,
        block: Option<Box<Block>>,
    },
    /// `recv.name(args)`, or `recv.name` with a block.
    MethodCall {
        recv: Box<Expr>,
        name: Ident,
        args: Vec<Arg>,
        block: Option<Box<Block>>,
    },
    /// `recv.name` with no parentheses or block: a field read, or a method with no arguments
    /// (`SPEC-RUBY.md` §4.11).
    Field {
        recv: Box<Expr>,
        name: Ident,
    },
    /// `f.(args)`: calling a closure.
    CallValue {
        callee: Box<Expr>,
        args: Vec<Arg>,
    },
    /// `base[index]`.
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        operand: Box<Expr>,
    },
    /// `cond ? a : b`.
    Ternary {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
    /// `if … elsif … else … end`, or `unless … else … end`.
    If(Box<If>),
    /// `case subject when … else … end`.
    Case(Box<Case>),
    /// `( e )`.
    Paren(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrPart {
    Text(String),
    /// `#{expr}`.
    Interp(Expr),
}

/// One `key: value` or `key => value` entry of a hash literal.
#[derive(Clone, Debug, PartialEq)]
pub struct Pair {
    pub key: Key,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Key {
    /// `name: value`, whose key is the string `"name"`.
    Label(Ident),
    /// `key => value`.
    Expr(Expr),
}

/// A call argument.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Pos(Expr),
    /// `name: value`, for a DSL keyword's named argument (`SPEC-RUBY.md` §5).
    Label {
        name: Ident,
        value: Expr,
    },
}

/// A block, `do |params| … end` or `{ |params| … }`, or a lambda's parameters and body.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub params: Vec<Ident>,
    pub body: Body,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct If {
    /// Each condition with its branch. An `unless` has exactly one.
    pub arms: Vec<(Expr, Body)>,
    pub else_body: Option<Body>,
    /// Whether this is an `unless`, whose branch runs when its condition is false.
    pub unless: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    pub subject: Expr,
    pub whens: Vec<When>,
    pub else_body: Option<Body>,
}

/// `when a, b then …`. A value may be a range, `lo..hi`.
#[derive(Clone, Debug, PartialEq)]
pub struct When {
    pub values: Vec<Expr>,
    pub body: Body,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `&&` or `and`.
    And,
    /// `||` or `or`.
    Or,
    /// `<<`, which appends to an array.
    Append,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::Append => "<<",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    /// `-`
    Neg,
    /// `!` or `not`
    Not,
}

/// The operator of a compound assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    /// `||=`
    Or,
    /// `&&=`
    And,
}

impl AssignOp {
    pub fn symbol(self) -> &'static str {
        match self {
            AssignOp::Add => "+=",
            AssignOp::Sub => "-=",
            AssignOp::Mul => "*=",
            AssignOp::Div => "/=",
            AssignOp::Mod => "%=",
            AssignOp::Pow => "**=",
            AssignOp::Or => "||=",
            AssignOp::And => "&&=",
        }
    }
}
