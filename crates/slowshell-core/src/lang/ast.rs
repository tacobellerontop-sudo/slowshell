//! Abstract syntax tree for the Slowshell configuration language.
//!
//! The AST is intentionally small. There is no separate "script" and "markup"
//! mode: an element literal is an expression, which is what makes constructs like
//!
//! ```qml
//! items: [
//!     MenuItem { text: "Open" }
//!     MenuSeparator {}
//! ]
//! ```
//!
//! work without special-casing in the parser.

use std::sync::Arc;

use crate::color::Color;
use crate::diag::Span;
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
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
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }

    /// Higher binds tighter. Mirrors the usual C precedence table so config reads
    /// the way people expect.
    pub fn precedence(self) -> u8 {
        match self {
            BinOp::Or => 1,
            BinOp::And => 2,
            BinOp::Eq | BinOp::Ne => 3,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 4,
            BinOp::Add | BinOp::Sub => 5,
            BinOp::Mul | BinOp::Div | BinOp::Rem => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Clone, Debug)]
pub enum Expr {
    Literal(Value),
    ColorLit(Color),
    /// A bare name: `battery`, `primary`, `left`.
    Ident(Arc<str>),
    /// `a.b`
    Member(Box<Expr>, Arc<str>),
    /// `a[b]`
    Index(Box<Expr>, Box<Expr>),
    /// `a(b, c)`
    Call(Box<Expr>, Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
    Map(Vec<(Arc<str>, Expr)>),
    /// A nested element literal used as a value, e.g. `Acrylic`.
    Item(Arc<Item>),
    /// An object literal, used for anonymous data such as `chart: { values: [1,2,3] }`.
    Block(Vec<Stmt>),
    /// Deferred expression: evaluated only when the property is read.
    ///
    /// Produced by `on*` handlers and by `bind` so that the compiler can tell a
    /// pure expression from one with side effects.
    Lazy(Arc<Expr>),
    /// Raw text handled by an out-of-tree language plugin (e.g. a script block).
    Raw(Arc<str>),
}

impl Expr {
    /// True when the expression cannot have side effects and can therefore be
    /// tracked as a pure dependency.
    pub fn is_pure(&self) -> bool {
        match self {
            Expr::Literal(_) | Expr::ColorLit(_) | Expr::Raw(_) => true,
            Expr::Ident(_) => true,
            Expr::Member(_, _) => true,
            Expr::Index(a, b) => a.is_pure() && b.is_pure(),
            // A call is only pure if it targets a known accessor; conservatively no.
            Expr::Call(_, _) => false,
            Expr::Unary(_, e) => e.is_pure(),
            Expr::Binary(_, a, b) => a.is_pure() && b.is_pure(),
            Expr::Cond(c, a, b) => c.is_pure() && a.is_pure() && b.is_pure(),
            Expr::List(v) => v.iter().all(|e| e.is_pure()),
            Expr::Map(v) => v.iter().all(|(_, e)| e.is_pure()),
            Expr::Item(_) => true,
            Expr::Block(_) => false,
            Expr::Lazy(e) => e.is_pure(),
        }
    }

    /// Rebuild with nested element literals resolved. Used by the include expander.
    pub fn map_items(&self, f: &dyn Fn(&Item) -> Result<Item, crate::diag::Diag>) -> Result<Expr, crate::diag::Diag> {
        Ok(match self {
            Expr::Item(i) => Expr::Item(Arc::new(f(i)?)),
            Expr::Member(a, n) => Expr::Member(Box::new(a.map_items(f)?), n.clone()),
            Expr::Index(a, b) => {
                Expr::Index(Box::new(a.map_items(f)?), Box::new(b.map_items(f)?))
            }
            Expr::Call(a, args) => Expr::Call(
                Box::new(a.map_items(f)?),
                args.iter().map(|e| e.map_items(f)).collect::<Result<_, _>>()?,
            ),
            Expr::Unary(op, e) => Expr::Unary(*op, Box::new(e.map_items(f)?)),
            Expr::Binary(op, a, b) => Expr::Binary(
                *op,
                Box::new(a.map_items(f)?),
                Box::new(b.map_items(f)?),
            ),
            Expr::Cond(c, a, b) => Expr::Cond(
                Box::new(c.map_items(f)?),
                Box::new(a.map_items(f)?),
                Box::new(b.map_items(f)?),
            ),
            Expr::List(v) => Expr::List(
                v.iter().map(|e| e.map_items(f)).collect::<Result<_, _>>()?,
            ),
            Expr::Map(v) => Expr::Map(
                v.iter()
                    .map(|(k, e)| Ok((k.clone(), e.map_items(f)?)))
                    .collect::<Result<_, crate::diag::Diag>>()?,
            ),
            Expr::Lazy(e) => Expr::Lazy(Arc::new(e.map_items(f)?)),
            other => other.clone(),
        })
    }
}

/// A source-level property assignment.
#[derive(Clone, Debug)]
pub struct Prop {
    pub name: Arc<str>,
    pub value: Expr,
    pub span: Span,
}

/// A child element or a named child group (`left { ... }`, `content { ... }`).
#[derive(Clone, Debug)]
pub struct Child {
    pub name: Arc<str>,
    pub item: Arc<Item>,
    pub span: Span,
}

/// An element: `Panel { ... }`.
#[derive(Clone, Debug)]
pub struct Item {
    /// The type name exactly as written, e.g. `Panel`, `Text`, `Row`.
    pub type_name: Arc<str>,
    /// Positional arguments, for `Notification("title", "body")`-style shorthand.
    pub args: Vec<Expr>,
    pub props: Vec<Prop>,
    pub children: Vec<Child>,
    pub span: Span,
    /// `include "widgets/weather.config"` splices another file's items in place.
    pub includes: Vec<(Arc<str>, Span)>,
}

impl Item {
    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| &*p.name == name)
    }

    pub fn prop_names(&self) -> Vec<&str> {
        self.props.iter().map(|p| &*p.name).collect()
    }

    pub fn type_name_str(&self) -> &str {
        &self.type_name
    }
}

/// A statement inside an element body.
#[derive(Clone, Debug)]
pub enum Stmt {
    /// `name: expr`
    Prop(Prop),
    /// `Name { ... }` — child element or named group, resolved at build time.
    Child(Child),
    /// `include "path"`
    Include(Arc<str>, Span),
}

impl Stmt {
    pub fn span(&self) -> &Span {
        match self {
            Stmt::Prop(p) => &p.span,
            Stmt::Child(c) => &c.span,
            Stmt::Include(_, s) => s,
        }
    }
}

/// A parsed source file.
#[derive(Clone, Debug)]
pub struct Document {
    pub file: Arc<str>,
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

impl Document {
    /// Top-level elements, i.e. the panels / overlays a config produces.
    pub fn roots(&self) -> impl Iterator<Item = &Child> {
        self.stmts.iter().filter_map(|s| match s {
            Stmt::Child(c) => Some(c),
            _ => None,
        })
    }
}
