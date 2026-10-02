//! Expression evaluation.
//!
//! Evaluation is *reactive-aware*: any property read inside an expression is
//! recorded as a dependency by the [`crate::react`] tracker, which is what makes
//! `text: battery.percentage + "%"` update on its own.
//!
//! Method calls never execute inline. They are pushed onto an action queue that
//! the runtime drains between frames, so a handler cannot re-enter the graph while
//! it is being rebuilt.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::color::Color;
use crate::diag::{Diag, DiagKind, DiagResult, Span};
use crate::lang::ast::{BinOp, Expr, UnOp};
use crate::react::Reactor;
use crate::value::{format_float, Value};

/// A call requested by config, resolved and performed by the runtime.
#[derive(Clone, Debug)]
pub struct Action {
    /// Dotted path of the callable, e.g. `launcher.open` or `audio.setVolume`.
    pub path: Vec<String>,
    pub args: Vec<Value>,
    /// Where the call came from, for error reporting.
    pub span: Option<Span>,
}

impl Action {
    pub fn render(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}({args})", self.path.join("."))
    }
}

/// Read-only access to named objects exposed to config.
pub trait Host {
    /// Resolve a dotted path such as `battery` or `screens.primary`.
    fn lookup(&self, path: &str) -> Option<Value>;
    /// Paths the host knows about, for `did you mean` hints.
    fn paths(&self) -> Vec<String>;
    /// Names under a prefix, used to expand `screens.*`.
    fn children_of(&self, prefix: &str) -> Vec<String>;
}

/// The evaluation context: a property graph plus a host and an action queue.
pub struct Env {
    pub reactor: Rc<Reactor>,
    pub host: Rc<dyn Host>,
    pub actions: Rc<RefCell<Vec<Action>>>,
}

impl Env {
    pub fn new(reactor: Rc<Reactor>, host: Rc<dyn Host>) -> Env {
        Env { reactor, host, actions: Rc::new(RefCell::new(Vec::new())) }
    }

    /// Queue a method call for the runtime to perform after evaluation finishes.
    pub fn queue_action(&self, path: Vec<String>, args: Vec<Value>, span: Option<Span>) {
        self.actions.borrow_mut().push(Action { path, args, span });
    }

    pub fn take_actions(&self) -> Vec<Action> {
        std::mem::take(&mut *self.actions.borrow_mut())
    }

    pub fn has_actions(&self) -> bool {
        !self.actions.borrow().is_empty()
    }
}

/// Collects the dotted path of an expression when it is a plain name chain.
///
/// Used to route `battery.percentage` to the reactor before falling back to the
/// host object, which is what makes reactive tracking work for system values.
pub fn path_of(expr: &Expr) -> Option<Vec<&str>> {
    match expr {
        Expr::Ident(n) => Some(vec![&**n]),
        Expr::Member(base, name) => {
            let mut p = path_of(base)?;
            p.push(name);
            Some(p)
        }
        _ => None,
    }
}

pub fn eval(env: &Env, expr: &Expr) -> DiagResult<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::ColorLit(c) => Ok(Value::Color(*c)),
        Expr::Raw(s) => Err(Diag::error(
            DiagKind::Runtime,
            "Raw script blocks are not enabled in this build",
        )
        .with_note(format!("The block was: `{}`", truncate(s, 60)))),
        Expr::Ident(name) => resolve_path(env, &[&**name]),

        Expr::Member(_, _) | Expr::Index(_, _) => {
            let path = path_of(expr);
            if let Some(p) = &path {
                if let Some(v) = resolve_path_opt(env, p) {
                    return Ok(v);
                }
            }
            eval_fallback(env, expr)
        }

        Expr::Call(callee, args) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(env, a)?);
            }
            let path = path_of(callee).map(|p| p.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            match path {
                Some(p) if !p.is_empty() => {
                    // A method call. Defer execution to the runtime so a handler
                    // cannot re-enter the graph while it is being rebuilt.
                    env.queue_action(p, vals, None);
                    Ok(Value::Null)
                }
                _ => {
                    // Calling the result of an expression, e.g. `handlers.open()`.
                    let target = eval(env, callee)?;
                    match target {
                        Value::Callable(c) => match &c.native {
                            Some(f) => f(&vals)
                                .map_err(|e| Diag::error(DiagKind::Runtime, format!("Call failed: {e}"))),
                            None => Err(Diag::error(
                                DiagKind::Runtime,
                                "This function has no native implementation",
                            )),
                        },
                        other => Err(Diag::error(
                            DiagKind::Runtime,
                            format!("`{}` is not callable", other.display_short()),
                        )),
                    }
                }
            }
        }

        Expr::Unary(op, e) => {
            let v = eval(env, e)?;
            match op {
                UnOp::Not => Ok(Value::Bool(!v.truthy())),
                UnOp::Neg => match v {
                    Value::Int(i) => Ok(Value::Int(-i)),
                    other => Ok(Value::Float(-other.to_f64_lossy())),
                },
            }
        }

        Expr::Binary(op, a, b) => eval_binary(env, *op, a, b),

        Expr::Cond(c, a, b) => {
            if eval(env, c)?.truthy() {
                eval(env, a)
            } else {
                eval(env, b)
            }
        }

        Expr::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for i in items {
                out.push(eval(env, i)?);
            }
            Ok(Value::list(out))
        }

        Expr::Map(pairs) => {
            let mut m = std::collections::BTreeMap::new();
            for (k, v) in pairs {
                m.insert(k.to_string(), eval(env, v)?);
            }
            Ok(Value::map(m))
        }

        Expr::Item(item) => {
            // A nested element used as a value. It is represented as an object so a
            // widget can inspect it, e.g. `background: Acrylic { tint: "#111" }`.
            Ok(Value::Map(Arc::new(build_element_value(item)?)))
        }

        Expr::Block(stmts) => {
            // An object literal body: evaluate every property into a map.
            let mut m = std::collections::BTreeMap::new();
            for s in stmts {
                match s {
                    crate::lang::ast::Stmt::Prop(p) => {
                        m.insert(p.name.to_string(), eval(env, &p.value)?);
                    }
                    crate::lang::ast::Stmt::Child(c) => {
                        m.insert(c.name.to_string(), Value::str(c.item.type_name.to_string()));
                    }
                    crate::lang::ast::Stmt::Include(p, _) => {
                        m.insert("include".into(), Value::str(p.to_string()));
                    }
                }
            }
            Ok(Value::map(m))
        }

        Expr::Lazy(inner) => eval(env, inner),
    }
}

/// Turn an element literal into a plain map of evaluated properties. Nested
/// elements become nested maps, which is enough for `Acrylic { tint: ... }`.
fn build_element_value(
    item: &crate::lang::ast::Item,
) -> DiagResult<std::collections::BTreeMap<String, Value>> {
    let mut m = std::collections::BTreeMap::new();
    for (i, a) in item.args.iter().enumerate() {
        m.insert(format!("arg{i}"), literal_of(a));
    }
    for p in &item.props {
        m.insert(p.name.to_string(), literal_of(&p.value));
    }
    for c in &item.children {
        m.insert(c.name.to_string(), Value::str(c.item.type_name.to_string()));
    }
    Ok(m)
}

/// Best-effort literal extraction for element-as-value, without an `Env`.
///
/// Widgets such as `Acrylic` are configuration, not data, so the properties are
/// read as literals where possible. Anything non-literal becomes a marker string
/// that the widget re-resolves against the real element.
fn literal_of(expr: &Expr) -> Value {
    match expr {
        Expr::Literal(v) => v.clone(),
        Expr::ColorLit(c) => Value::Color(*c),
        Expr::Ident(n) => Value::str(n.to_string()),
        Expr::Member(_, _) | Expr::Call(_, _) => Value::str("<expr>"),
        _ => Value::Null,
    }
}

fn eval_binary(env: &Env, op: BinOp, a: &Expr, b: &Expr) -> DiagResult<Value> {
    // Short-circuit before evaluating the right side, so `network.connected && f()`
    // never queues the call when offline.
    match op {
        BinOp::And => {
            return Ok(Value::Bool(eval(env, a)?.truthy() && eval(env, b)?.truthy()));
        }
        BinOp::Or => {
            return Ok(Value::Bool(eval(env, a)?.truthy() || eval(env, b)?.truthy()));
        }
        _ => {}
    }
    let (l, r) = (eval(env, a)?, eval(env, b)?);
    match op {
        BinOp::Eq => Ok(Value::Bool(l.loose_eq(&r))),
        BinOp::Ne => Ok(Value::Bool(!l.loose_eq(&r))),
        // Ordering on strings as well as numbers: comparing `screens[0].name < "x"`.
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ord = match (&l, &r) {
                (Value::Str(x), Value::Str(y)) => Some(x.as_ref().cmp(y.as_ref())),
                _ => match (l.to_f64_lossy(), r.to_f64_lossy()) {
                    (x, y) if x.is_finite() && y.is_finite() => x.partial_cmp(&y),
                    _ => None,
                },
            };
            let Some(ord) = ord else {
                return Err(Diag::error(
                    DiagKind::TypeMismatch,
                    format!(
                        "Cannot compare {} with {}",
                        l.type_name(),
                        r.type_name()
                    ),
                ));
            };
            Ok(Value::Bool(match op {
                BinOp::Lt => ord.is_lt(),
                BinOp::Le => ord.is_le(),
                BinOp::Gt => ord.is_gt(),
                BinOp::Ge => ord.is_ge(),
                _ => unreachable!(),
            }))
        }
        BinOp::Add => {
            // `+` is string concatenation when either side is text, which is what
            // makes `text: "Battery: " + battery.percentage + "%"` read naturally.
            if matches!(l, Value::Str(_)) || matches!(r, Value::Str(_)) {
                return Ok(Value::str(format!("{}{}", l.to_string_lossy(), r.to_string_lossy())));
            }
            if let (Value::Color(x), y) = (&l, &r) {
                // `background: theme.accent + 0.2` lightens; a negative value darkens.
                let t = y.to_f64_lossy();
                let target = if t < 0.0 { Color::BLACK } else { Color::WHITE };
                return Ok(Value::Color(x.lerp(target, t.abs() as f32)));
            }
            if let (Value::Int(x), y) = (&l, &r) {
                return match y {
                    Value::Int(i) => Ok(Value::Int(x.wrapping_add(*i))),
                    _ => Ok(Value::Float(*x as f64 + y.to_f64_lossy())),
                };
            }
            if let (x, Value::Int(y)) = (&l, &r) {
                return match x {
                    Value::Int(i) => Ok(Value::Int(i.wrapping_add(*y))),
                    _ => Ok(Value::Float(x.to_f64_lossy() + *y as f64)),
                };
            }
            Ok(Value::Float(l.to_f64_lossy() + r.to_f64_lossy()))
        }
        BinOp::Sub => arith(l, r, |x, y| x - y, |x, y| x.wrapping_sub(y)),
        BinOp::Mul => arith(l, r, |x, y| x * y, |x, y| x.wrapping_mul(y)),
        BinOp::Div => {
            let d = r.to_f64_lossy();
            if d == 0.0 {
                return Err(Diag::error(DiagKind::Runtime, "Division by zero"));
            }
            Ok(Value::Float(l.to_f64_lossy() / d))
        }
        BinOp::Rem => {
            let d = r.to_f64_lossy();
            if d == 0.0 {
                return Err(Diag::error(DiagKind::Runtime, "Remainder by zero"));
            }
            Ok(Value::Float(l.to_f64_lossy() % d))
        }
        BinOp::And | BinOp::Or => unreachable!("handled above"),
    }
}

fn arith(
    l: Value,
    r: Value,
    f: impl Fn(f64, f64) -> f64,
    i: impl Fn(i64, i64) -> i64,
) -> DiagResult<Value> {
    if let (Value::Int(x), Value::Int(y)) = (&l, &r) {
        return Ok(Value::Int(i(*x, *y)));
    }
    Ok(Value::Float(f(l.to_f64_lossy(), r.to_f64_lossy())))
}

fn eval_fallback(env: &Env, expr: &Expr) -> DiagResult<Value> {
    match expr {
        Expr::Index(base, idx) => {
            let b = eval(env, base)?;
            let i = eval(env, idx)?;
            b.index(&i).ok_or_else(|| {
                Diag::error(
                    DiagKind::UnknownIdentifier,
                    format!("No entry for `{}`", i.to_string_lossy()),
                )
                .with_span(span_of(expr))
            })
        }
        Expr::Member(base, name) => {
            let b = eval(env, base)?;
            b.get(name).ok_or_else(|| {
                Diag::error(
                    DiagKind::UnknownProperty,
                    format!("`{}` has no property `{name}`", b.display_short()),
                )
                .with_span(span_of(expr))
                .suggest_from(&b_object_keys(&b), name)
            })
        }
        other => Err(Diag::error(
            DiagKind::UnknownIdentifier,
            format!("Could not evaluate {}", describe(other)),
        )
        .with_span(span_of(other))),
    }
}

fn b_object_keys(v: &Value) -> Vec<&'static str> {
    match v {
        Value::Object(o) => o.keys(),
        _ => Vec::new(),
    }
}

/// Resolve a dotted name chain, preferring reactor sources so reads are tracked.
fn resolve_path_opt(env: &Env, path: &[&str]) -> Option<Value> {
    if path.is_empty() {
        return None;
    }
    let joined = path.join(".");
    if let Some(id) = env.reactor.lookup(&joined) {
        return Some(env.reactor.get(id));
    }
    // A host may publish flat dotted paths, so try the whole chain first.
    if let Some(v) = env.host.lookup(&joined) {
        return Some(v);
    }
    // Otherwise take the longest prefix that exists as an object and walk the rest.
    for split in (1..path.len()).rev() {
        let prefix = path[..split].join(".");
        let mut v = match env.reactor.lookup(&prefix) {
            Some(id) => env.reactor.get(id),
            None => match env.host.lookup(&prefix) {
                Some(v) => v,
                None => continue,
            },
        };
        let mut resolved = true;
        for key in &path[split..] {
            match v.get(key) {
                Some(next) => v = next,
                None => {
                    resolved = false;
                    break;
                }
            }
        }
        if resolved {
            return Some(v);
        }
    }
    None
}

fn resolve_path(env: &Env, path: &[&str]) -> DiagResult<Value> {
    if let Some(v) = resolve_path_opt(env, path) {
        return Ok(v);
    }
    let root = path[0];
    let mut hints = Vec::new();
    // A longer prefix of the path that does resolve, e.g. `battery` in
    // `battery.nonesuch`.
    if let Some(prefix) =
        (1..path.len()).rev().find(|n| env.host.lookup(&path[..*n].join(".")).is_some())
    {
        hints.push(format!(
            "`{}` exists but has no `{}`.",
            path[..prefix].join("."),
            path[prefix..].join(".")
        ));
    } else if let Some(obj) = env.host.lookup(root) {
        let keys = obj.as_object().map(|o| o.keys()).unwrap_or_default();
        if !keys.is_empty() {
            hints.push(format!("Available on `{root}`: {}", keys.join(", ")));
        }
    }
    // A near-miss on the root itself, e.g. `batter.percentage` -> `battery`.
    let roots: Vec<String> = {
        let mut seen: Vec<String> = Vec::new();
        for p in env.host.paths() {
            if let Some(r) = p.split('.').next().map(str::to_string) {
                if !seen.contains(&r) {
                    seen.push(r);
                }
            }
        }
        seen
    };
    let root_refs: Vec<&str> = roots.iter().map(String::as_str).collect();
    for name in crate::diag::closest(root, &root_refs, 2) {
        hints.push(format!("Did you mean `{name}`?"));
    }
    if hints.is_empty() {
        let known = env.host.children_of(root);
        if !known.is_empty() {
            hints.push(format!("Did you mean `{root}.{}`?", known.join(&format!("`, `{root}."))));
        }
    }
    let mut d = Diag::error(DiagKind::UnknownIdentifier, format!("Unknown `{root}`"));
    for h in hints {
        d = d.with_hint(h);
    }
    Err(d)
}

pub fn span_of(_expr: &Expr) -> Span {
    // Expressions do not each carry a span in the AST; the evaluator reports
    // without a position and the compiler attaches the property span.
    Span::unknown()
}

fn describe(expr: &Expr) -> String {
    match expr {
        Expr::Ident(n) => format!("`{n}`"),
        Expr::Call(c, _) => format!("a call to `{}`", describe(c)),
        Expr::Member(_, n) => format!("property `{n}`"),
        Expr::Index(_, _) => "an index expression".to_string(),
        Expr::List(_) => "a list".to_string(),
        Expr::Map(_) => "an object".to_string(),
        Expr::Item(i) => format!("element `{}`", i.type_name),
        _ => "this expression".to_string(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Render a float the way the language prints it. Re-exported for the UI layer.
pub fn fmt_float(f: f64) -> String {
    format_float(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Color;
    use crate::lang::parse;

    struct TestHost {
        values: std::collections::BTreeMap<String, Value>,
    }

    impl Host for TestHost {
        fn lookup(&self, path: &str) -> Option<Value> {
            self.values.get(path).cloned()
        }
        fn paths(&self) -> Vec<String> {
            self.values.keys().cloned().collect()
        }
        fn children_of(&self, prefix: &str) -> Vec<String> {
            let want = format!("{prefix}.");
            self.values
                .keys()
                .filter_map(|k| k.strip_prefix(&want).map(|s| s.to_string()))
                .collect()
        }
    }

    fn env_with(pairs: &[(&str, Value)]) -> Env {
        let values = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        Env::new(Rc::new(Reactor::new()), Rc::new(TestHost { values }))
    }

    fn eval_src(env: &Env, src: &str) -> DiagResult<Value> {
        let doc = parse("t.config", &format!("Text {{ v: {src} }}"))?;
        let item = doc.roots().next().unwrap().item.clone();
        let p = item.prop("v").unwrap();
        eval(env, &p.value)
    }

    #[test]
    fn arithmetic_follows_precedence() {
        let env = env_with(&[]);
        assert!(eval_src(&env, "1 + 2 * 3").unwrap().loose_eq(&Value::Int(7)));
        assert!(eval_src(&env, "(1 + 2) * 3").unwrap().loose_eq(&Value::Int(9)));
    }

    #[test]
    fn plus_concatenates_when_a_side_is_text() {
        let env = env_with(&[("battery.percentage", Value::Int(87))]);
        assert_eq!(
            eval_src(&env, "\"Battery: \" + battery.percentage + \"%\"").unwrap().to_string_lossy(),
            "Battery: 87%"
        );
    }

    #[test]
    fn reads_host_objects() {
        let env = env_with(&[("battery.percentage", Value::Int(87))]);
        assert!(eval_src(&env, "battery.percentage").unwrap().loose_eq(&Value::Int(87)));
    }

    #[test]
    fn reads_are_tracked_as_dependencies() {
        let reactor = Rc::new(Reactor::new());
        reactor.source("clock.time", Value::str("12:00"));
        let env = Env::new(reactor.clone(), Rc::new(TestHost { values: Default::default() }));
        let doc = parse("t.config", "Text { v: clock.time }").unwrap();
        let p = doc.roots().next().unwrap().item.prop("v").unwrap().clone();

        // Evaluating outside a tracker records nothing.
        eval(&env, &p.value).unwrap();
        assert_eq!(reactor.stats().edges, 0);

        // Inside a tracker, the read creates the edge.
        let (out, deps) = crate::react::tracked(|| eval(&env, &p.value).unwrap());
        assert!(out.loose_eq(&Value::str("12:00")));
        assert!(deps.contains(&reactor.lookup("clock.time").unwrap()));
    }

    #[test]
    fn method_calls_are_queued_not_executed() {
        let env = env_with(&[]);
        eval_src(&env, "launcher.open()").unwrap();
        let actions = env.take_actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].path, vec!["launcher", "open"]);
        assert!(actions[0].render().starts_with("launcher.open("));
    }

    #[test]
    fn and_short_circuits_before_queueing() {
        let env = env_with(&[("network.connected", Value::Bool(false))]);
        eval_src(&env, "network.connected && audio.mute()").unwrap();
        assert!(env.take_actions().is_empty());
    }

    #[test]
    fn comparison_and_logic() {
        let env = env_with(&[("battery.percentage", Value::Int(15))]);
        assert!(eval_src(&env, "battery.percentage < 20").unwrap().loose_eq(&Value::Bool(true)));
        assert!(eval_src(&env, "battery.percentage > 90 || true").unwrap().loose_eq(&Value::Bool(true)));
        assert!(eval_src(&env, "!false").unwrap().loose_eq(&Value::Bool(true)));
    }

    #[test]
    fn conditional_picks_one_branch() {
        let env = env_with(&[("battery.charging", Value::Bool(true))]);
        assert_eq!(
            eval_src(&env, "battery.charging ? \"plugged\" : \"on battery\"").unwrap().to_string_lossy(),
            "plugged"
        );
    }

    #[test]
    fn unknown_identifier_suggests_a_near_miss_root() {
        let env = env_with(&[("battery.percentage", Value::Int(50))]);
        let e = eval_src(&env, "batter.percentage").unwrap_err();
        assert_eq!(e.kind, DiagKind::UnknownIdentifier);
        assert!(
            e.hints.iter().any(|h| h.contains("battery")),
            "expected a `battery` suggestion, got {:?}",
            e.hints
        );
    }

    #[test]
    fn division_by_zero_is_reported() {
        let env = env_with(&[]);
        assert!(eval_src(&env, "1 / 0").is_err());
    }

    #[test]
    fn color_arithmetic_lightens() {
        let env = env_with(&[("theme.accent", Value::Color(Color::rgb(0, 0, 0)))]);
        let v = eval_src(&env, "theme.accent + 1.0").unwrap();
        let c = v.to_color_lossy();
        assert_eq!(c.r, 255);
        assert_eq!(c.g, 255);
    }
}
