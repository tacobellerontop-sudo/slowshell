//! # Slowshell core
//!
//! The platform-independent half of the framework: the configuration language, the
//! reactive property graph, diagnostics, logging and the CLI protocol.
//!
//! Nothing in this crate calls a Windows API, which keeps the language and the
//! reactivity model testable on any machine and keeps platform code isolated in
//! `slowshell-win`.
//!
//! ## The shape of a shell
//!
//! ```text
//!   shell.config  ──parse──▶  Document ──build──▶  Scene (element tree)
//!                                    │
//!                                    ├──resolve──▶  Env (Reactor + Host)
//!                                                    │
//!   battery.percentage ──push──▶  Reactor ──invalidate──▶  dirty props
//!                                                    │
//!                                                    └──read──▶  new values
//! ```

pub mod actions;
pub mod color;
pub mod diag;
pub mod ease;
pub mod eval;
pub mod ipc;
pub mod lang;
pub mod log;
pub mod paths;
pub mod react;
pub mod value;

pub use actions::{ActionFn, ActionSpec, Registry};
pub use color::Color;
pub use diag::{closest, Diag, DiagKind, DiagResult, Pos, Severity, Span};
pub use eval::{eval, path_of, Action, Env, Host};
pub use lang::{ast, parse, Document, Item, Prop, Stmt};
pub use react::{PropId, PropKind, Reactor, ReactorStats, Signal};
pub use value::{Callable, Object, Value};

/// The framework version, reported by `Shell.exe --version` and `shellctl status`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parse a config file, returning the document or a fully-rendered diagnostic.
pub fn parse_config(file: impl Into<std::sync::Arc<str>>, source: &str) -> DiagResult<Document> {
    parse(file, source)
}
