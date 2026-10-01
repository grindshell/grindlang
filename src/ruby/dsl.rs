//! Host-declared DSL keywords (`SPEC-RUBY.md` §5): handler keywords, which gather blocks into a
//! table of closures and may generate a function that dispatches to them, and data keywords,
//! which build a table of records.
//!
//! The host declares keywords on [`RubyConfig`]. A script writes one as a statement directly at
//! the top level, and a data keyword's nested keywords directly in its block. Each use is
//! ordinary top-level code, so what it builds is frozen once the module has initialized.
//!
//! ## What a use becomes
//!
//! The first use of a keyword declares its table, a constant map from id to entry, and each
//! later use adds an entry where the script wrote it:
//!
//! ```text
//! ON = { ["hit"] = on#1(function(dmg) mem.hp = mem.hp - dmg end) }
//! ON["heal"] = on#1(function(amount) … end)
//! ```
//!
//! `on#1` is the keyword's *builder*: a private function, declared once before any other code,
//! whose parameters are annotated with the declared types. Calling it makes the checker check
//! each argument against its declared type where the script wrote it, and a block passed to it
//! takes the declared signature before its body is checked (`SPEC.md` §5.5). A handler's
//! builder and a list keyword's give back their argument; a data keyword's builds the record:
//!
//! ```text
//! local function choice#2(p#1, p#2, p#3) return { text = p#1, cond = p#2, effect = p#3 } end
//! ```
//!
//! So a keyword's arguments are evaluated in the order the host declared its fields, not the
//! order they are written. A handler keyword's dispatch function goes after every use.
//!
//! ## Spans
//!
//! A use's call to its builder takes the span of the keyword's name, and the table's name,
//! declared or read, and the entry's key take the span of the id. The builders and dispatch
//! functions hold nothing the script wrote, so each name in them takes a span of its own past the
//! end of the source ([`Translator::synthetic`]), which no node of the script has. A diagnostic
//! there would point at the end of the script, but none arises: the host's types are checked
//! when it declares them.

use std::collections::{HashMap, HashSet};

use super::RubyConfig;
use super::ast as rb;
use super::desugar::{Tail, Translator, binary, call, expr, ident, index, name, stat};
use crate::ast::{
    BinOp, Block, ConstDecl, Expr, ExprKind, Fallthrough, Field, FnAnnotations, FuncBody, FuncDecl,
    Ident, ParamAnn, RetStat, Spanned, StatKind, TopDecl, TypeAnn,
};
use crate::diagnostics::Span;
use crate::types::{FnType, Type};
use crate::value::Value;

// ---- declaring keywords --------------------------------------------------------------------

/// A keyword the host declared.
#[derive(Clone, Debug)]
pub(super) enum Keyword {
    Handler(Handler),
    Data(Data),
}

impl Keyword {
    fn keyword(&self) -> &str {
        match self {
            Keyword::Handler(h) => &h.keyword,
            Keyword::Data(d) => &d.keyword,
        }
    }

    fn table(&self) -> &str {
        match self {
            Keyword::Handler(h) => &h.table,
            Keyword::Data(d) => &d.table,
        }
    }

    fn dispatch(&self) -> Option<&str> {
        match self {
            Keyword::Handler(h) => h.dispatch.as_deref(),
            Keyword::Data(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Handler {
    keyword: String,
    table: String,
    params: Vec<Type>,
    ret: Type,
    dispatch: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct Data {
    keyword: String,
    table: String,
    schema: Schema,
}

/// The record a data keyword builds (`SPEC-RUBY.md` §5.2): its fields, each with a type and
/// what in a use fills it. Every record of one keyword has every field, so a field a use leaves
/// out takes its default.
#[derive(Clone, Debug)]
pub struct Schema {
    fields: Vec<FieldDecl>,
}

#[derive(Clone, Debug)]
struct FieldDecl {
    name: String,
    ty: Type,
    source: Source,
    /// What a use that leaves the field out gives it, besides `nil` for an optional type.
    default: Option<Value>,
}

/// What in a use fills a field.
#[derive(Clone, Debug)]
enum Source {
    /// The next positional argument.
    Positional,
    /// The named argument `label:`.
    Label(String),
    /// The keyword's block.
    Block,
    /// Each use of this keyword in the block adds its argument.
    List(String),
    /// Each use of this keyword in the block adds the record it builds.
    Child(String, Schema),
}

/// A handler keyword being declared, from [`RubyConfig::handler`].
pub struct HandlerKeyword<'c> {
    config: &'c mut RubyConfig,
    at: usize,
}

/// Names a keyword can't take: what the parser or translator gives a meaning of its own, and the
/// builtins the translation calls (`SPEC-RUBY.md` §2.3).
const NOT_KEYWORDS: &[&str] = &[
    "lambda", "proc", "format", "math", "string", "tostring", "tonumber",
];

/// The builtins, which a table or dispatch function can't be named after.
const BUILTINS: &[&str] = &["math", "string", "tostring", "tonumber"];

/// Whether `s` is a name: a letter or `_`, then letters, digits, and `_`.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `s` is a lowercase name, which a Ruby call or label can be.
fn is_word(s: &str) -> bool {
    is_name(s) && !s.starts_with(|c: char| c.is_ascii_uppercase())
}

/// Whether a script can hold a value of type `t`, so a keyword may declare it: `()` only as
/// what a function gives, and no tuple or host userdata.
fn writable(t: &Type) -> bool {
    match t {
        Type::Number | Type::Bool | Type::String | Type::Nil => true,
        Type::Optional(inner) | Type::Array(inner) | Type::Map(inner) => writable(inner),
        Type::Record(fields) => fields.values().all(writable),
        Type::Function(ft) => {
            ft.params.iter().all(writable) && (matches!(*ft.ret, Type::Unit) || writable(&ft.ret))
        }
        Type::Unit | Type::Tuple(_) | Type::Userdata(_) | Type::Var(_) | Type::Error => false,
    }
}

#[track_caller]
fn check_type(t: &Type) {
    assert!(
        writable(t),
        "RubyConfig: a keyword can't declare the type `{t}`: a script can't write it"
    );
}

#[track_caller]
fn check_keyword_name(keyword: &str) {
    assert!(
        is_word(keyword) && !super::lexer::is_reserved(keyword) && !NOT_KEYWORDS.contains(&keyword),
        "RubyConfig: `{keyword}` can't be a keyword: it must be a lowercase name Ruby doesn't \
         already give a meaning"
    );
}

/// `T?`, or `t` itself if it is already optional.
fn optional(t: Type) -> Type {
    match t {
        Type::Optional(_) => t,
        t => Type::optional(t),
    }
}

/// The function type `t` is, seen through an optional.
fn function_type(t: &Type) -> Option<&FnType> {
    match t {
        Type::Function(ft) => Some(ft),
        Type::Optional(inner) => function_type(inner),
        _ => None,
    }
}

impl RubyConfig {
    /// Declare the handler keyword `keyword` (`SPEC-RUBY.md` §5.1). Each
    /// `keyword :id do |…| … end` directly at the top level adds its block to `table`, an
    /// exported constant mapping each id to its closure. [`HandlerKeyword::block`] sets the
    /// signature every block must fit.
    ///
    /// ```
    /// use grindlang::Type;
    /// use grindlang::ruby::RubyConfig;
    /// let mut ruby = RubyConfig::new().ivar_memory("mem");
    /// ruby.handler("on", "ON").block(&[Type::Number], Type::Unit).dispatch("on_event");
    /// ```
    ///
    /// # Panics
    ///
    /// If `keyword` isn't a lowercase name Ruby gives no meaning of its own, `table` isn't a
    /// name, or another keyword already takes either.
    pub fn handler(&mut self, keyword: &str, table: &str) -> HandlerKeyword<'_> {
        self.claim(keyword, table);
        self.keywords.push(Keyword::Handler(Handler {
            keyword: keyword.to_string(),
            table: table.to_string(),
            params: Vec::new(),
            ret: Type::Unit,
            dispatch: None,
        }));
        let at = self.keywords.len() - 1;
        HandlerKeyword { config: self, at }
    }

    /// Declare the data keyword `keyword` (`SPEC-RUBY.md` §5.2). Each `keyword :id, … end`
    /// directly at the top level adds the record it builds to `table`, an exported constant
    /// mapping each id to its record. Declare the record's fields on the [`Schema`] this
    /// returns.
    ///
    /// ```
    /// use grindlang::Type;
    /// use grindlang::ruby::RubyConfig;
    /// let mut ruby = RubyConfig::new();
    /// ruby.data("node", "NODES")
    ///     .list("say", "lines", Type::String)
    ///     .child("choice", "choices", |c| {
    ///         c.positional("text", Type::String)
    ///             .keyword("if", "cond", Type::optional(Type::function(vec![], Type::Bool)))
    ///             .block("effect", &[], Type::Unit)
    ///             .optional()
    ///     });
    /// ```
    ///
    /// # Panics
    ///
    /// As [`handler`](Self::handler).
    pub fn data(&mut self, keyword: &str, table: &str) -> &mut Schema {
        self.claim(keyword, table);
        self.keywords.push(Keyword::Data(Data {
            keyword: keyword.to_string(),
            table: table.to_string(),
            schema: Schema { fields: Vec::new() },
        }));
        match self.keywords.last_mut() {
            Some(Keyword::Data(d)) => &mut d.schema,
            _ => unreachable!("just pushed"),
        }
    }

    #[track_caller]
    fn claim(&self, keyword: &str, table: &str) {
        check_keyword_name(keyword);
        for n in [keyword, table] {
            self.check_free(n);
        }
        assert!(
            keyword != table,
            "RubyConfig: `{keyword}` can't name both a keyword and its table"
        );
    }

    /// Check that `n` can name a generated table or function: a name no keyword takes yet.
    #[track_caller]
    fn check_free(&self, n: &str) {
        assert!(
            is_name(n) && !BUILTINS.contains(&n),
            "RubyConfig: `{n}` can't name a keyword's table or function"
        );
        assert!(
            self.claimant(n).is_none(),
            "RubyConfig: `{n}` is already taken by another keyword"
        );
    }

    /// The keyword whose name, table, or dispatch function `n` is, if any.
    fn claimant(&self, n: &str) -> Option<&Keyword> {
        self.keywords
            .iter()
            .find(|k| k.keyword() == n || k.table() == n || k.dispatch() == Some(n))
    }

    pub(super) fn top_keyword(&self, n: &str) -> Option<&Keyword> {
        self.keywords.iter().find(|k| k.keyword() == n)
    }

    /// The keyword whose block takes `n` as one of its own, if any.
    fn parent_of(&self, n: &str) -> Option<&str> {
        self.keywords.iter().find_map(|k| match k {
            Keyword::Data(d) => d.schema.parent_of(n, &d.keyword),
            Keyword::Handler(_) => None,
        })
    }
}

impl HandlerKeyword<'_> {
    fn handler(&mut self) -> &mut Handler {
        match &mut self.config.keywords[self.at] {
            Keyword::Handler(h) => h,
            Keyword::Data(_) => unreachable!("declared as a handler"),
        }
    }

    /// What each handler's block takes, and gives: `ret` is [`Type::Unit`] for nothing. A block
    /// may leave out parameters at the end that it doesn't use. Without this, a block takes
    /// nothing and gives nothing.
    ///
    /// # Panics
    ///
    /// If a script can't write one of the types: a tuple, host userdata, or `()` other than as
    /// `ret`.
    #[track_caller]
    pub fn block(&mut self, params: &[Type], ret: Type) -> &mut Self {
        check_type(&Type::function(params.to_vec(), ret.clone()));
        let h = self.handler();
        h.params = params.to_vec();
        h.ret = ret;
        self
    }

    /// Also generate the exported function `name(id, …)`, which runs the handler for `id` with
    /// the arguments after it and gives what that gives, or does nothing and gives `nil` when
    /// there is none.
    ///
    /// # Panics
    ///
    /// If `name` isn't a name, or another keyword already takes it.
    #[track_caller]
    pub fn dispatch(&mut self, name: &str) -> &mut Self {
        self.config.check_free(name);
        self.handler().dispatch = Some(name.to_string());
        self
    }
}

impl Schema {
    /// A field filled by the next positional argument. It is required unless its type is
    /// optional or [`default`](Self::default) gives it a value.
    ///
    /// # Panics
    ///
    /// If `field` isn't a name or is already declared, or a script can't write `ty`.
    #[track_caller]
    pub fn positional(&mut self, field: &str, ty: Type) -> &mut Self {
        self.add(field, ty, Source::Positional)
    }

    /// A field filled by the named argument `label:`, which may be a keyword of Ruby's
    /// (`if:`). It is required unless its type is optional or [`default`](Self::default) gives
    /// it a value. A function-typed field takes a lambda, `if: -> { … }`.
    ///
    /// # Panics
    ///
    /// As [`positional`](Self::positional), and if `label` isn't a lowercase name or is
    /// already declared.
    #[track_caller]
    pub fn keyword(&mut self, label: &str, field: &str, ty: Type) -> &mut Self {
        assert!(
            is_word(label),
            "RubyConfig: `{label}:` can't be a named argument: it must be a lowercase name"
        );
        assert!(
            !self.labels().any(|l| l == label),
            "RubyConfig: `{label}:` is already declared"
        );
        self.add(field, ty, Source::Label(label.to_string()))
    }

    /// A field filled by the keyword's block: a function taking `params` and giving `ret`,
    /// [`Type::Unit`] for nothing. A block may leave out parameters at the end that it doesn't
    /// use. The field is required unless [`optional`](Self::optional) follows.
    ///
    /// # Panics
    ///
    /// As [`positional`](Self::positional), and if the keyword already takes a block, for
    /// another field or for nested keywords.
    #[track_caller]
    pub fn block(&mut self, field: &str, params: &[Type], ret: Type) -> &mut Self {
        assert!(
            !self.takes_block(),
            "RubyConfig: a keyword's block can fill only one field, or hold nested keywords"
        );
        self.add(field, Type::function(params.to_vec(), ret), Source::Block)
    }

    /// An array field of `ty`s, empty unless the keyword's block holds `keyword value`
    /// statements, each of which adds its value.
    ///
    /// # Panics
    ///
    /// As [`positional`](Self::positional), if `keyword` can't be a keyword or is already one of
    /// this keyword's, and if the keyword's block already fills a field.
    #[track_caller]
    pub fn list(&mut self, keyword: &str, field: &str, ty: Type) -> &mut Self {
        self.check_nested(keyword);
        self.add(field, Type::array(ty), Source::List(keyword.to_string()))
    }

    /// An array field of records, empty unless the keyword's block holds `keyword …`
    /// statements, each of which adds the record it builds. `declare` declares its fields.
    ///
    /// # Panics
    ///
    /// As [`list`](Self::list).
    #[track_caller]
    pub fn child(
        &mut self,
        keyword: &str,
        field: &str,
        declare: impl FnOnce(&mut Schema) -> &mut Schema,
    ) -> &mut Self {
        self.check_nested(keyword);
        let mut child = Schema { fields: Vec::new() };
        declare(&mut child);
        let ty = Type::array(child.record());
        self.add(field, ty, Source::Child(keyword.to_string(), child))
    }

    /// Let a use leave out the field declared last, which then gives it `nil`: its type becomes
    /// optional.
    ///
    /// # Panics
    ///
    /// If no field is declared yet, or the last is an array of nested keywords, which may
    /// always be left out.
    #[track_caller]
    pub fn optional(&mut self) -> &mut Self {
        let f = self
            .fields
            .last_mut()
            .expect("RubyConfig: `optional` follows a field");
        assert!(
            !matches!(f.source, Source::List(_) | Source::Child(..)),
            "RubyConfig: `{}` is filled by nested keywords, so it is never required",
            f.name
        );
        f.ty = optional(std::mem::replace(&mut f.ty, Type::Nil));
        self
    }

    /// Let a use leave out the field declared last, which then gives it `value`: `nil` for an
    /// optional type, or a bool, number, or string of its type.
    ///
    /// # Panics
    ///
    /// If no field is declared yet, the last isn't filled by an argument, or `value` doesn't fit
    /// its type.
    #[track_caller]
    pub fn default(&mut self, value: Value) -> &mut Self {
        let f = self
            .fields
            .last_mut()
            .expect("RubyConfig: `default` follows a field");
        assert!(
            matches!(f.source, Source::Positional | Source::Label(_)),
            "RubyConfig: only a field filled by an argument takes a default, and `{}` isn't",
            f.name
        );
        let inner = match &f.ty {
            Type::Optional(inner) => inner,
            t => t,
        };
        let fits = match &value {
            Value::Nil => matches!(f.ty, Type::Optional(_)),
            Value::Bool(_) => matches!(inner, Type::Bool),
            Value::Number(_) => matches!(inner, Type::Number),
            Value::Str(_) => matches!(inner, Type::String),
            _ => false,
        };
        assert!(
            fits,
            "RubyConfig: {value} can't be the default of `{}`, a {}",
            f.name, f.ty
        );
        f.default = Some(value);
        self
    }

    #[track_caller]
    fn add(&mut self, field: &str, ty: Type, source: Source) -> &mut Self {
        assert!(
            is_name(field),
            "RubyConfig: `{field}` can't name a field: it must be a name"
        );
        assert!(
            !self.fields.iter().any(|f| f.name == field),
            "RubyConfig: the field `{field}` is already declared"
        );
        check_type(&ty);
        self.fields.push(FieldDecl {
            name: field.to_string(),
            ty,
            source,
            default: None,
        });
        self
    }

    #[track_caller]
    fn check_nested(&self, keyword: &str) {
        check_keyword_name(keyword);
        assert!(
            !self.nested().any(|k| k == keyword),
            "RubyConfig: `{keyword}` is already one of this keyword's"
        );
        assert!(
            !self
                .fields
                .iter()
                .any(|f| matches!(f.source, Source::Block)),
            "RubyConfig: a keyword's block can fill only one field, or hold nested keywords"
        );
    }

    /// Whether the keyword's block fills a field, or holds nested keywords.
    fn takes_block(&self) -> bool {
        self.fields.iter().any(|f| {
            matches!(
                f.source,
                Source::Block | Source::List(_) | Source::Child(..)
            )
        })
    }

    fn labels(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().filter_map(|f| match &f.source {
            Source::Label(l) => Some(l.as_str()),
            _ => None,
        })
    }

    /// The keywords this keyword's block may hold.
    fn nested(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().filter_map(|f| match &f.source {
            Source::List(k) | Source::Child(k, _) => Some(k.as_str()),
            _ => None,
        })
    }

    /// The type of the record this schema builds.
    fn record(&self) -> Type {
        Type::Record(
            self.fields
                .iter()
                .map(|f| (f.name.clone(), f.ty.clone()))
                .collect(),
        )
    }

    /// The keyword, this one (`me`) or one nested in it, whose block takes `n`, if any.
    fn parent_of<'s>(&'s self, n: &str, me: &'s str) -> Option<&'s str> {
        self.fields.iter().find_map(|f| match &f.source {
            Source::List(k) if k == n => Some(me),
            Source::Child(k, _) if k == n => Some(me),
            Source::Child(k, child) => child.parent_of(n, k),
            _ => None,
        })
    }
}

// ---- translating uses ----------------------------------------------------------------------

/// What translating the keywords keeps track of.
pub(super) struct Dsl {
    /// Where the next span past the end of the source starts.
    next_span: u32,
    /// The builder of each keyword declared so far, by the keyword's path (`node`,
    /// `node.choice`), and their declarations.
    builder_names: HashMap<String, String>,
    builders: Vec<TopDecl>,
    /// The ids each top-level keyword has taken.
    ids: HashMap<String, HashSet<String>>,
    /// The tables declared so far: a later use of their keyword adds to them.
    tables: HashSet<String>,
}

impl Dsl {
    /// For a source whose last byte is before `end`.
    pub(super) fn new(end: u32) -> Self {
        Dsl {
            next_span: end + 1,
            builder_names: HashMap::new(),
            builders: Vec::new(),
            ids: HashMap::new(),
            tables: HashSet::new(),
        }
    }
}

/// A keyword written as a statement: `name args block`, or `name` alone.
struct Use<'r> {
    name: Ident,
    args: &'r [rb::Arg],
    block: Option<&'r rb::Block>,
    span: Span,
}

/// The keyword use `s` could be: a call without a receiver as a statement.
fn use_of(s: &rb::Stmt) -> Option<Use<'_>> {
    let rb::StmtKind::Expr(e) = &s.kind else {
        return None;
    };
    match &e.kind {
        rb::ExprKind::Call { name, args, block } => Some(Use {
            name: name.clone(),
            args,
            block: block.as_deref(),
            span: e.span,
        }),
        rb::ExprKind::Name(n) => Some(Use {
            name: Spanned::new(n.clone(), e.span),
            args: &[],
            block: None,
            span: e.span,
        }),
        _ => None,
    }
}

/// A use's id: its first argument, a symbol or string written out.
struct Id {
    text: String,
    span: Span,
}

fn arg_span(arg: &rb::Arg) -> Span {
    match arg {
        rb::Arg::Pos(e) => e.span,
        rb::Arg::Label { name, value } => name.span.to(value.span),
    }
}

/// `n` `what`s, in words for none.
fn plural(n: usize, what: &str) -> String {
    match n {
        0 => format!("no {what}s"),
        1 => format!("1 {what}"),
        n => format!("{n} {what}s"),
    }
}

/// The list `a`, `b` and `c`, or `none`.
fn listed(names: impl Iterator<Item = String>) -> String {
    let names: Vec<String> = names.map(|n| format!("`{n}`")).collect();
    match names.as_slice() {
        [] => "none".to_string(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The annotation spelling of `t`, which [`writable`] has allowed.
fn ann(t: &Type) -> TypeAnn {
    match t {
        Type::Number => TypeAnn::Named("number".to_string()),
        Type::Bool => TypeAnn::Named("bool".to_string()),
        Type::String => TypeAnn::Named("string".to_string()),
        Type::Nil => TypeAnn::Named("nil".to_string()),
        Type::Optional(inner) => TypeAnn::Optional(Box::new(ann(inner))),
        Type::Array(inner) => TypeAnn::Array(Box::new(ann(inner))),
        Type::Map(inner) => TypeAnn::Map(Box::new(ann(inner))),
        Type::Record(fields) => {
            TypeAnn::Record(fields.iter().map(|(k, v)| (k.clone(), ann(v))).collect())
        }
        Type::Function(ft) => TypeAnn::Function {
            params: ft.params.iter().map(ann).collect(),
            ret: match *ft.ret {
                Type::Unit => None,
                ref r => Some(Box::new(ann(r))),
            },
        },
        _ => unreachable!("checked when declared"),
    }
}

/// The literal a default is.
fn literal(v: &Value, span: Span) -> Expr {
    let kind = match v {
        Value::Bool(b) => ExprKind::Bool(*b),
        Value::Number(n) => ExprKind::Number(*n),
        Value::Str(s) => ExprKind::Str(s.to_string()),
        _ => ExprKind::Nil,
    };
    expr(kind, span)
}

impl Translator<'_> {
    /// A span of its own, past the end of the source (the module docs).
    fn synthetic(&mut self) -> Span {
        let start = self.dsl.next_span;
        self.dsl.next_span += 1;
        Span {
            start,
            end: start + 1,
        }
    }

    /// Translate `s` if it uses a top-level keyword, adding what it becomes to `decls`, and the
    /// table it declares, if it does, to `exports`. Returns whether it did.
    pub(super) fn top_keyword(
        &mut self,
        s: &rb::Stmt,
        decls: &mut Vec<TopDecl>,
        exports: &mut Vec<Ident>,
    ) -> bool {
        let Some(u) = use_of(s) else {
            return false;
        };
        let ruby = self.ruby;
        match ruby.top_keyword(&u.name.node) {
            Some(Keyword::Handler(h)) => self.handler_use(h, u, decls, exports),
            Some(Keyword::Data(d)) => self.data_use(d, u, decls, exports),
            None => return false,
        }
        true
    }

    /// What `E0612` says of calling `n` anywhere but where a keyword is written, if `n` names
    /// one and no function.
    pub(super) fn misplaced_keyword(&self, n: &str) -> Option<String> {
        if self.defs.contains(n) || self.cfg.host_functions.contains_key(n) {
            return None;
        }
        if self.ruby.top_keyword(n).is_some() {
            return Some(format!(
                "`{n}` is a keyword, which is written only as a statement directly at the top level"
            ));
        }
        let parent = self.ruby.parent_of(n)?;
        Some(format!(
            "`{n}` is a keyword of `{parent}`'s block, which is written only as a statement \
             directly in it"
        ))
    }

    /// What `E0612` says of a `def` or constant named `n`, if a keyword takes that name.
    pub(super) fn keyword_claims(&self, n: &str) -> Option<String> {
        let k = self.ruby.claimant(n)?;
        let what = if k.keyword() == n {
            "a keyword the host declared".to_string()
        } else if k.table() == n {
            format!("the table the host's `{}` keyword builds", k.keyword())
        } else {
            format!(
                "the function the host's `{}` keyword generates",
                k.keyword()
            )
        };
        Some(format!(
            "`{n}` is {what}, so no `def` or constant can take its name"
        ))
    }

    /// The dispatch functions, after every use, and the builders, before any other code.
    pub(super) fn finish_keywords(&mut self, decls: &mut Vec<TopDecl>, exports: &mut Vec<Ident>) {
        let ruby = self.ruby;
        for k in &ruby.keywords {
            if let Keyword::Handler(h) = k
                && let Some(dispatch) = &h.dispatch
            {
                let f = self.dispatch(h, dispatch);
                exports.push(f.name.clone());
                decls.push(TopDecl::Function(f));
            }
        }
        let mut all = std::mem::take(&mut self.dsl.builders);
        all.append(decls);
        *decls = all;
    }

    /// `keyword :id do |…| … end`: the block, typed by the builder, added to the table.
    fn handler_use(
        &mut self,
        h: &Handler,
        u: Use,
        decls: &mut Vec<TopDecl>,
        exports: &mut Vec<Ident>,
    ) {
        let k = &u.name.node;
        if let Some(rb::Arg::Label { name: label, .. }) =
            u.args.iter().find(|a| matches!(a, rb::Arg::Label { .. }))
        {
            self.error(
                "E0613",
                format!(
                    "`{k}` has no `{}:` argument: it takes an id and a block",
                    label.node
                ),
                label.span,
            );
            return;
        }
        let Some(id) = self.id(&u) else { return };
        if let Some(extra) = u.args.get(1) {
            self.error(
                "E0614",
                format!("`{k}` takes one argument, the handler's id"),
                arg_span(extra),
            );
            return;
        }
        if !self.new_id(k, &id) {
            return;
        }
        let Some(block) = u.block else {
            self.error(
                "E0615",
                format!("`{k}` needs a block, the handler: `{k} … do … end`"),
                u.name.span,
            );
            return;
        };
        let ft = FnType {
            params: h.params.clone(),
            ret: Box::new(h.ret.clone()),
        };
        let Some(f) = self.dsl_function(k, block, &ft) else {
            return;
        };
        let builder = self.identity_builder(k, Type::Function(ft));
        let value = call(name(&builder, u.name.span), vec![f], u.span);
        self.insert(&h.table, id, value, u.span, decls, exports);
    }

    /// `keyword :id, … do … end`: the record, added to the table.
    fn data_use(&mut self, d: &Data, u: Use, decls: &mut Vec<TopDecl>, exports: &mut Vec<Ident>) {
        let Some(id) = self.id(&u) else { return };
        if !self.new_id(&d.keyword, &id) {
            return;
        }
        let Some(value) = self.record(
            &d.schema,
            &d.keyword,
            &u.name,
            &u.args[1..],
            u.block,
            u.span,
        ) else {
            return;
        };
        self.insert(&d.table, id, value, u.span, decls, exports);
    }

    /// A top-level use's id: its first argument, a symbol or a string with no interpolation.
    fn id(&mut self, u: &Use) -> Option<Id> {
        let k = &u.name.node;
        let msg = format!(
            "`{k}`'s first argument is its id, a symbol or string written out: `{k} :name …`"
        );
        let e = match u.args.first() {
            Some(rb::Arg::Pos(e)) => e,
            Some(arg @ rb::Arg::Label { .. }) => {
                self.error("E0614", msg, arg_span(arg));
                return None;
            }
            None => {
                self.error(
                    "E0615",
                    format!("`{k}` needs an id first: `{k} :name …`"),
                    u.name.span,
                );
                return None;
            }
        };
        let text = match &e.kind {
            rb::ExprKind::Symbol(s) => s.clone(),
            rb::ExprKind::Str(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        rb::StrPart::Text(t) => text.push_str(t),
                        rb::StrPart::Interp(_) => {
                            self.error("E0614", msg, e.span);
                            return None;
                        }
                    }
                }
                text
            }
            _ => {
                self.error("E0614", msg, e.span);
                return None;
            }
        };
        Some(Id { text, span: e.span })
    }

    /// Record `id` as taken for `keyword`; `false`, with `E0616`, if it already was.
    fn new_id(&mut self, keyword: &str, id: &Id) -> bool {
        let fresh = self
            .dsl
            .ids
            .entry(keyword.to_string())
            .or_default()
            .insert(id.text.clone());
        if !fresh {
            self.error(
                "E0616",
                format!(
                    "`{keyword}` already has the id `{}`: each id names one entry",
                    id.text
                ),
                id.span,
            );
        }
        fresh
    }

    /// Add `value` to `table` under `id`: the first use declares the table, an exported
    /// constant, and a later one assigns into it.
    fn insert(
        &mut self,
        table: &str,
        id: Id,
        value: Expr,
        span: Span,
        decls: &mut Vec<TopDecl>,
        exports: &mut Vec<Ident>,
    ) {
        let key = expr(ExprKind::Str(id.text), id.span);
        if self.dsl.tables.insert(table.to_string()) {
            let table = ident(table, id.span);
            let map = expr(ExprKind::Table(vec![Field::Keyed { key, value }]), span);
            decls.push(TopDecl::Const(ConstDecl {
                name: table.clone(),
                value: map,
                span,
            }));
            exports.push(table);
        } else {
            let target = index(name(table, id.span), key, false, span);
            let assign = StatKind::Assign {
                targets: vec![target],
                exprs: vec![value],
            };
            decls.push(TopDecl::Stat(Box::new(stat(assign, span))));
        }
    }

    /// The record a data keyword builds, as a call to its builder. `path` names the keyword
    /// for its builder, `kw` is the keyword as written, and `args` its arguments after any id.
    fn record(
        &mut self,
        schema: &Schema,
        path: &str,
        kw: &Ident,
        args: &[rb::Arg],
        block: Option<&rb::Block>,
        span: Span,
    ) -> Option<Expr> {
        let k = &kw.node;
        let before = self.error_count();
        let mut positional = Vec::new();
        let mut labelled: HashMap<&str, &rb::Expr> = HashMap::new();
        for arg in args {
            match arg {
                rb::Arg::Pos(e) => positional.push(e),
                rb::Arg::Label { name: label, value } => {
                    if !schema.labels().any(|l| l == label.node) {
                        let takes = listed(schema.labels().map(|l| format!("{l}:")));
                        self.error(
                            "E0613",
                            format!(
                                "`{k}` has no `{}:` argument; its named arguments: {takes}",
                                label.node,
                            ),
                            label.span,
                        );
                    } else if labelled.insert(&label.node, value).is_some() {
                        self.error(
                            "E0614",
                            format!("`{}:` is given twice", label.node),
                            label.span,
                        );
                    }
                }
            }
        }
        let slots = schema
            .fields
            .iter()
            .filter(|f| matches!(f.source, Source::Positional))
            .count();
        if let Some(extra) = positional.get(slots) {
            self.error(
                "E0614",
                format!(
                    "`{k}` takes {}{}",
                    plural(slots, "argument"),
                    if path.contains('.') {
                        ""
                    } else {
                        " after its id"
                    }
                ),
                extra.span,
            );
        }
        // The block fills a field, or holds the nested keywords.
        let mut items: Vec<Vec<Expr>> = vec![Vec::new(); schema.fields.len()];
        let fills = schema
            .fields
            .iter()
            .any(|f| matches!(f.source, Source::Block));
        if let Some(b) = block
            && !fills
        {
            if schema.nested().next().is_none() {
                self.error("E0614", format!("`{k}` takes no block"), b.span);
            } else {
                if let Some(p) = b.params.first() {
                    self.error(
                        "E0614",
                        format!("`{k}`'s block takes no parameters: it holds `{k}`'s keywords"),
                        p.span,
                    );
                }
                for s in &b.body.stmts {
                    self.nested(schema, path, k, s, &mut items);
                }
            }
        }
        let mut positional = positional.into_iter();
        let mut values = Vec::with_capacity(schema.fields.len());
        for (i, f) in schema.fields.iter().enumerate() {
            let value = match &f.source {
                Source::Positional => positional.next().map(|e| self.field_value(k, e, &f.ty)),
                Source::Label(l) => labelled
                    .get(l.as_str())
                    .map(|e| self.field_value(k, e, &f.ty)),
                // A block that doesn't fit has been reported: it isn't missing.
                Source::Block => match (block, function_type(&f.ty)) {
                    (Some(b), Some(ft)) => Some(
                        self.dsl_function(k, b, ft)
                            .unwrap_or_else(|| expr(ExprKind::Nil, b.span)),
                    ),
                    _ => None,
                },
                Source::List(_) | Source::Child(..) => {
                    let fields = std::mem::take(&mut items[i])
                        .into_iter()
                        .map(Field::Positional)
                        .collect();
                    Some(expr(ExprKind::Table(fields), span))
                }
            };
            let value = value.unwrap_or_else(|| self.missing(k, f, kw.span));
            values.push(value);
        }
        if self.error_count() > before {
            return None;
        }
        let builder = self.record_builder(path, schema);
        Some(call(name(&builder, kw.span), values, span))
    }

    /// A statement in the block of the data keyword `parent`, which must use one of its
    /// keywords: what it adds goes in `items`, by field.
    fn nested(
        &mut self,
        schema: &Schema,
        path: &str,
        parent: &str,
        s: &rb::Stmt,
        items: &mut [Vec<Expr>],
    ) {
        let found = use_of(s).and_then(|u| {
            let (i, f) = schema.fields.iter().enumerate().find(|(_, f)| {
                matches!(&f.source, Source::List(k) | Source::Child(k, _) if *k == u.name.node)
            })?;
            Some((u, i, f))
        });
        let Some((u, i, f)) = found else {
            self.error(
                "E0613",
                format!(
                    "only {} can be written in `{parent}`'s block",
                    listed(schema.nested().map(str::to_string))
                ),
                s.span,
            );
            return;
        };
        let path = format!("{path}.{}", u.name.node);
        let item = match &f.source {
            Source::List(_) => {
                let Type::Array(elem) = &f.ty else {
                    unreachable!("a list is an array")
                };
                self.list_item(&path, &u, elem)
            }
            Source::Child(_, child) => self.record(child, &path, &u.name, u.args, u.block, u.span),
            _ => unreachable!("found by its keyword"),
        };
        if let Some(item) = item {
            items[i].push(item);
        }
    }

    /// `keyword value` in a data keyword's block, adding `value` to a list.
    fn list_item(&mut self, path: &str, u: &Use, ty: &Type) -> Option<Expr> {
        let k = &u.name.node;
        let value = match u.args {
            [rb::Arg::Pos(e)] => e,
            [] => {
                self.error(
                    "E0615",
                    format!("`{k}` needs a value: `{k} …`"),
                    u.name.span,
                );
                return None;
            }
            args => {
                match args.iter().find_map(|a| match a {
                    rb::Arg::Label { name, .. } => Some(name),
                    rb::Arg::Pos(_) => None,
                }) {
                    Some(label) => self.error(
                        "E0613",
                        format!(
                            "`{k}` has no `{}:` argument: it takes one value",
                            label.node
                        ),
                        label.span,
                    ),
                    None => self.error(
                        "E0614",
                        format!("`{k}` takes one value"),
                        arg_span(&args[1]),
                    ),
                }
                return None;
            }
        };
        if let Some(b) = u.block {
            self.error("E0614", format!("`{k}` takes no block"), b.span);
            return None;
        }
        let v = self.field_value(k, value, ty);
        let builder = self.identity_builder(path, ty.clone());
        Some(call(name(&builder, u.name.span), vec![v], u.span))
    }

    /// An argument for a field of type `ty`. A lambda written there for a function type is
    /// translated as a keyword's block is.
    fn field_value(&mut self, kw: &str, e: &rb::Expr, ty: &Type) -> Expr {
        if let rb::ExprKind::Lambda(block) = &e.kind
            && let Some(ft) = function_type(ty)
        {
            return self
                .dsl_function(kw, block, ft)
                .unwrap_or_else(|| expr(ExprKind::Nil, e.span));
        }
        self.expr(e)
    }

    /// A field a use left out: its default, `nil` for an optional type, or else `E0615`.
    fn missing(&mut self, kw: &str, f: &FieldDecl, at: Span) -> Expr {
        if let Some(v) = &f.default {
            return literal(v, at);
        }
        if !matches!(f.ty, Type::Optional(_)) {
            let msg = match &f.source {
                Source::Positional => format!("`{kw}` needs its `{}` argument", f.name),
                Source::Label(l) => format!("`{kw}` needs `{l}:`"),
                Source::Block => format!("`{kw}` needs a block, its `{}`", f.name),
                Source::List(_) | Source::Child(..) => unreachable!("a list may be empty"),
            };
            self.error("E0615", msg, at);
        }
        expr(ExprKind::Nil, at)
    }

    /// A keyword's block, or a lambda written as one of its arguments, as a function of type
    /// `ft`: one whose last expression gives its value only if `ft` gives one, and which takes
    /// the parameters it leaves out at the end too, under names no Ruby name can be.
    fn dsl_function(&mut self, kw: &str, block: &rb::Block, ft: &FnType) -> Option<Expr> {
        let want = ft.params.len();
        if let Some(extra) = block.params.get(want) {
            let takes = match want {
                0 => "none".to_string(),
                n => format!("at most {}", plural(n, "parameter")),
            };
            self.error(
                "E0614",
                format!(
                    "`{kw}` calls this with {}, so it takes {takes}",
                    plural(want, "argument"),
                ),
                extra.span,
            );
            return None;
        }
        let mut params = block.params.clone();
        while params.len() < want {
            let at = self.synthetic();
            params.push(ident(&format!("_#{}", params.len() + 1), at));
        }
        let tail = if matches!(*ft.ret, Type::Unit) {
            Tail::None
        } else {
            Tail::Return
        };
        let body = self.within(None, |t| t.body(&block.body, Some(block.span), tail));
        let f = FuncBody {
            params,
            block: body,
            span: block.span,
            fallthrough: Fallthrough::Nil,
        };
        Some(expr(ExprKind::Function(f), block.span))
    }

    /// The builder that gives back its argument, of type `ty`.
    fn identity_builder(&mut self, path: &str, ty: Type) -> String {
        self.builder(path, vec![ty.clone()], ty, |t, params| {
            let at = t.synthetic();
            name(&params[0].node, at)
        })
    }

    /// The builder of `schema`'s record.
    fn record_builder(&mut self, path: &str, schema: &Schema) -> String {
        let types = schema.fields.iter().map(|f| f.ty.clone()).collect();
        self.builder(path, types, schema.record(), |t, params| {
            let fields = schema
                .fields
                .iter()
                .zip(params)
                .map(|(f, p)| {
                    let at = t.synthetic();
                    Field::Named {
                        name: ident(&f.name, at),
                        value: name(&p.node, at),
                    }
                })
                .collect();
            let at = t.synthetic();
            expr(ExprKind::Table(fields), at)
        })
    }

    /// The builder of the keyword at `path`, declared the first time: a private function whose
    /// parameters are annotated with `params` and which gives `ret`, the value `body` builds
    /// from those parameters.
    fn builder(
        &mut self,
        path: &str,
        params: Vec<Type>,
        ret: Type,
        body: impl FnOnce(&mut Self, &[Ident]) -> Expr,
    ) -> String {
        if let Some(b) = self.dsl.builder_names.get(path) {
            return b.clone();
        }
        let keyword = path.rsplit('.').next().unwrap_or(path);
        let fname = self.temp(keyword);
        let names: Vec<Ident> = (1..=params.len())
            .map(|i| {
                let at = self.synthetic();
                ident(&format!("p#{i}"), at)
            })
            .collect();
        let value = body(self, &names);
        let at = self.synthetic();
        let annotations = FnAnnotations {
            params: names
                .iter()
                .zip(&params)
                .map(|(p, t)| ParamAnn {
                    name: p.clone(),
                    ty: Spanned::new(ann(t), p.span),
                })
                .collect(),
            ret: vec![Spanned::new(ann(&ret), at)],
        };
        let block = Block {
            stats: Vec::new(),
            ret: Some(RetStat {
                exprs: vec![value],
                span: at,
            }),
            span: at,
        };
        self.dsl.builders.push(TopDecl::Function(FuncDecl {
            name: ident(&fname, at),
            local: true,
            body: FuncBody {
                params: names,
                block,
                span: at,
                fallthrough: Fallthrough::Error,
            },
            annotations,
            span: at,
        }));
        self.dsl
            .builder_names
            .insert(path.to_string(), fname.clone());
        fname
    }

    /// The dispatch function `fname(id#, arg#1, …)` of the handler keyword `h`:
    ///
    /// ```text
    /// local h# = ON[id#]
    /// if h# ~= nil then h#(arg#1, …) end         -- or `return h#(…)`, when it gives a value
    /// ```
    ///
    /// or nothing, when the script declares no handler.
    fn dispatch(&mut self, h: &Handler, fname: &str) -> FuncDecl {
        let at = self.synthetic();
        let id = ident("id#", self.synthetic());
        let args: Vec<Ident> = (1..=h.params.len())
            .map(|i| {
                let at = self.synthetic();
                ident(&format!("arg#{i}"), at)
            })
            .collect();
        let mut annotated = vec![ParamAnn {
            name: id.clone(),
            ty: Spanned::new(ann(&Type::String), id.span),
        }];
        annotated.extend(args.iter().zip(&h.params).map(|(a, t)| ParamAnn {
            name: a.clone(),
            ty: Spanned::new(ann(t), a.span),
        }));
        let gives = !matches!(h.ret, Type::Unit);
        let ret = if gives {
            vec![Spanned::new(ann(&optional(h.ret.clone())), at)]
        } else {
            Vec::new()
        };
        let mut stats = Vec::new();
        if self.dsl.tables.contains(&h.table) {
            let lookup = index(
                name(&h.table, self.synthetic()),
                name("id#", self.synthetic()),
                false,
                at,
            );
            stats.push(stat(
                StatKind::Local {
                    names: vec![ident("h#", self.synthetic())],
                    exprs: vec![lookup],
                },
                at,
            ));
            let found = binary(
                BinOp::Ne,
                name("h#", self.synthetic()),
                expr(ExprKind::Nil, at),
                at,
            );
            let arg_reads = args
                .iter()
                .map(|a| {
                    let at = self.synthetic();
                    name(&a.node, at)
                })
                .collect();
            let run = call(name("h#", self.synthetic()), arg_reads, at);
            let then = if gives {
                Block {
                    stats: Vec::new(),
                    ret: Some(RetStat {
                        exprs: vec![run],
                        span: at,
                    }),
                    span: at,
                }
            } else {
                Block {
                    stats: vec![stat(StatKind::Call(run), at)],
                    ret: None,
                    span: at,
                }
            };
            stats.push(stat(
                StatKind::If {
                    arms: vec![(found, then)],
                    else_block: None,
                },
                at,
            ));
        }
        let mut params = vec![id];
        params.extend(args);
        FuncDecl {
            name: ident(fname, at),
            local: false,
            body: FuncBody {
                params,
                block: Block {
                    stats,
                    ret: None,
                    span: at,
                },
                span: at,
                fallthrough: Fallthrough::Nil,
            },
            annotations: FnAnnotations {
                params: annotated,
                ret,
            },
            span: at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice() -> RubyConfig {
        let mut ruby = RubyConfig::new();
        ruby.data("node", "NODES")
            .list("say", "lines", Type::String)
            .child("choice", "choices", |c| {
                c.positional("text", Type::String)
                    .keyword(
                        "if",
                        "cond",
                        Type::optional(Type::function(vec![], Type::Bool)),
                    )
                    .block("effect", &[], Type::Unit)
                    .optional()
            });
        ruby
    }

    #[test]
    fn a_schema_types_its_record() {
        let ruby = choice();
        let Some(Keyword::Data(d)) = ruby.top_keyword("node") else {
            panic!()
        };
        assert_eq!(
            d.schema.record().to_string(),
            "record { choices: array<record { cond: fn() -> bool?, effect: fn() -> ()?, text: string }>, \
             lines: array<string> }"
        );
        assert_eq!(ruby.parent_of("choice"), Some("node"));
        assert_eq!(ruby.parent_of("say"), Some("node"));
        assert_eq!(ruby.parent_of("node"), None);
    }

    #[test]
    #[should_panic(expected = "already taken")]
    fn a_table_is_one_keywords() {
        let mut ruby = choice();
        ruby.handler("on", "NODES");
    }

    #[test]
    #[should_panic(expected = "already taken")]
    fn a_dispatch_function_is_one_keywords() {
        let mut ruby = choice();
        ruby.handler("on", "ON").dispatch("node");
    }

    #[test]
    #[should_panic(expected = "can't be a keyword")]
    fn a_keyword_is_a_free_lowercase_name() {
        RubyConfig::new().handler("if", "IFS");
    }

    #[test]
    #[should_panic(expected = "can't be a keyword")]
    fn a_keyword_is_not_lambda() {
        RubyConfig::new().data("lambda", "LAMBDAS");
    }

    #[test]
    #[should_panic(expected = "a script can't write it")]
    fn a_type_is_one_a_script_writes() {
        RubyConfig::new()
            .data("item", "ITEMS")
            .positional("pair", Type::tuple(vec![Type::Number, Type::Number]));
    }

    #[test]
    #[should_panic(expected = "only one field, or hold nested keywords")]
    fn a_block_fills_one_field() {
        RubyConfig::new()
            .data("node", "NODES")
            .list("say", "lines", Type::String)
            .block("run", &[], Type::Unit);
    }

    #[test]
    #[should_panic(expected = "can't be the default")]
    fn a_default_fits_its_field() {
        RubyConfig::new()
            .data("item", "ITEMS")
            .positional("weight", Type::Number)
            .default(Value::Bool(true));
    }

    #[test]
    #[should_panic(expected = "already declared")]
    fn a_field_is_declared_once() {
        RubyConfig::new()
            .data("item", "ITEMS")
            .positional("name", Type::String)
            .keyword("label", "name", Type::String);
    }
}
