//! Dependency-tracked property graph.
//!
//! A `Property` is either a **source** (pushed by a system provider such as
//! `battery.percentage`) or a **derived** node backed by a closure. Reading a
//! derived property while a tracker is installed records its inputs; writing to a
//! source marks everything downstream dirty so the next read recomputes lazily.
//!
//! Lazy recomputation is what keeps idle CPU near zero: a panel showing only the
//! clock does no work at all when the battery changes.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::value::Value;

pub type PropId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropKind {
    /// Pushed by the runtime.
    Source,
    /// Evaluated on read from a closure.
    Derived,
}

/// One node in the graph.
struct Property {
    id: PropId,
    /// Dotted path such as `battery.percentage`. Empty for anonymous nodes.
    path: Rc<str>,
    kind: PropKind,
    value: RefCell<Value>,
    /// Shared so `get_inner` can invoke the binding without cloning a `dyn Fn`.
    binding: Option<Rc<dyn Fn() -> Value>>,
    /// Everything read during the last evaluation of this node.
    deps: RefCell<Vec<PropId>>,
    /// Nodes that read this one.
    subs: RefCell<Vec<PropId>>,
    dirty: Cell<bool>,
    /// Guards against a binding that reads itself through a cycle.
    evaluating: Cell<bool>,
    /// Number of recomputations; surfaced by the debug overlay.
    recomputes: Cell<u64>,
}

impl Property {
    fn value(&self) -> Value {
        self.value.borrow().clone()
    }
}

impl std::fmt::Debug for Property {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Property")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("dirty", &self.dirty.get())
            .field("recomputes", &self.recomputes.get())
            .finish()
    }
}

thread_local! {
    /// One frame per in-progress evaluation.
    ///
    /// This is a *stack*, not a single slot, because evaluating a derived node runs
    /// its binding, which reads other derived nodes, which push their own frames.
    /// With a single slot the inner frame would overwrite the outer one and the
    /// outer node would record the wrong dependencies — a bug that shows up as
    /// "changing `a` doesn't update `c`".
    static TRACKER: RefCell<Vec<Vec<PropId>>> = const { RefCell::new(Vec::new()) };
}

/// Read a property while tracking, making the current evaluation depend on it.
pub fn read_tracked(id: PropId) {
    TRACKER.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(frame) = t.last_mut() {
            if !frame.contains(&id) {
                frame.push(id);
            }
        }
    });
}

/// Whether a tracker is currently collecting.
pub fn is_tracking() -> bool {
    TRACKER.with(|t| !t.borrow().is_empty())
}

pub struct Reactor {
    nodes: RefCell<Vec<Rc<Property>>>,
    by_path: RefCell<HashMap<Rc<str>, PropId>>,
    /// Nodes whose value is stale, drained by the runtime each frame.
    pending: RefCell<Vec<PropId>>,
    /// Set when anything was invalidated so the runtime can request a frame.
    needs_frame: Cell<bool>,
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Reactor {
    pub fn new() -> Reactor {
        Reactor {
            nodes: RefCell::new(Vec::new()),
            by_path: RefCell::new(HashMap::new()),
            pending: RefCell::new(Vec::new()),
            needs_frame: Cell::new(false),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.borrow().is_empty()
    }

    fn node(&self, id: PropId) -> Option<Rc<Property>> {
        self.nodes.borrow().get(id as usize).cloned()
    }

    pub fn node_info(&self, id: PropId) -> Option<PropertyInfo> {
        self.node(id).map(|n| PropertyInfo {
            id: n.id,
            path: n.path.to_string(),
            kind: n.kind.clone(),
            dirty: n.dirty.get(),
            recomputes: n.recomputes.get(),
            deps: n.deps.borrow().len(),
            subs: n.subs.borrow().len(),
        })
    }

    /// All nodes, for the debug overlay and `shellctl inspect graph`.
    pub fn all_nodes(&self) -> Vec<PropertyInfo> {
        self.nodes
            .borrow()
            .iter()
            .map(|n| PropertyInfo {
                id: n.id,
                path: n.path.to_string(),
                kind: n.kind.clone(),
                dirty: n.dirty.get(),
                recomputes: n.recomputes.get(),
                deps: n.deps.borrow().len(),
                subs: n.subs.borrow().len(),
            })
            .collect()
    }

    fn push(&self, mut prop: Property) -> PropId {
        let mut nodes = self.nodes.borrow_mut();
        let id = nodes.len() as PropId;
        // Every node carries its own id so `subs`/`deps` can be traced back to a
        // node without an extra lookup table.
        prop.id = id;
        nodes.push(Rc::new(prop));
        id
    }

    /// Define (or redefine) a source and return its id.
    ///
    /// Redefinition keeps node identity so existing dependents stay wired. A path
    /// already held by a derived node is replaced, since a path names one value.
    pub fn source(&self, path: impl Into<Rc<str>>, initial: Value) -> PropId {
        let path: Rc<str> = path.into();
        let existing = self.by_path.borrow().get(&path).copied();
        if let Some(id) = existing {
            if self.node(id).is_some_and(|n| matches!(n.kind, PropKind::Source)) {
                self.set(path.as_ref(), initial);
                return id;
            }
        }
        let id = self.push(Property {
            id: 0,
            path: path.clone(),
            kind: PropKind::Source,
            value: RefCell::new(initial),
            binding: None,
            deps: RefCell::new(Vec::new()),
            subs: RefCell::new(Vec::new()),
            dirty: Cell::new(false),
            evaluating: Cell::new(false),
            recomputes: Cell::new(0),
        });
        self.by_path.borrow_mut().insert(path, id);
        id
    }

    /// Define a derived node whose value is computed on read.
    ///
    /// The path makes the node addressable from config and from `shellctl inspect
    /// graph`, which is what makes reactivity debuggable rather than magic.
    pub fn derived(
        &self,
        path: impl Into<Rc<str>>,
        binding: impl Fn() -> Value + 'static,
    ) -> PropId {
        let path: Rc<str> = path.into();
        let id = self.push(Property {
            id: 0,
            path: path.clone(),
            kind: PropKind::Derived,
            value: RefCell::new(Value::Null),
            binding: Some(Rc::new(binding)),
            deps: RefCell::new(Vec::new()),
            subs: RefCell::new(Vec::new()),
            dirty: Cell::new(true),
            evaluating: Cell::new(false),
            recomputes: Cell::new(0),
        });
        self.by_path.borrow_mut().insert(path, id);
        id
    }

    pub fn lookup(&self, path: &str) -> Option<PropId> {
        self.by_path.borrow().get(path).copied()
    }

    /// Push a new value into a source.
    ///
    /// Returns whether the value actually changed. An unchanged write is a
    /// complete no-op, which is what keeps a high-frequency provider from
    /// waking the UI when nothing is different.
    pub fn set(&self, path: &str, value: Value) -> bool {
        let Some(&id) = self.by_path.borrow().get(path) else {
            let id = self.source(path.to_string(), value);
            self.invalidate(id);
            return true;
        };
        self.set_id(id, value)
    }

    /// Push a new value into a source by id. Returns whether it changed.
    pub fn set_id(&self, id: PropId, value: Value) -> bool {
        let Some(node) = self.node(id) else { return false };
        if *node.value.borrow() == value {
            return false;
        }
        *node.value.borrow_mut() = value;
        self.invalidate(id);
        true
    }

    /// Mark a node and everything downstream of it stale.
    ///
    /// Iterative rather than recursive: a deep graph must not overflow the stack,
    /// and a cyclic graph must terminate.
    ///
    /// # The visited set is per traversal, not the node's `dirty` flag
    ///
    /// `dirty` means "this node's cached value is stale", and that is only
    /// meaningful for a **derived** node. A source's value is assigned directly by
    /// `set_id` and is therefore never stale, and nothing ever recomputes a source
    /// to clear the flag.
    ///
    /// Using the flag as the "already walked this" guard therefore meant that the
    /// *second* push to any source skipped the whole downstream subtree: the flag
    /// was still set from the first push, so the `continue` fired before the
    /// subscribers were reached. A clock ticking to the same second twice, or a
    /// battery going from 87% to 86%, would leave every widget showing the old
    /// value with no error anywhere.
    ///
    /// A local `seen` list gives the same cycle and depth guarantees and cannot
    /// leak between calls.
    fn invalidate(&self, id: PropId) {
        let mut stack = vec![id];
        let mut seen: Vec<PropId> = Vec::new();
        while let Some(cur) = stack.pop() {
            if seen.contains(&cur) {
                // Already reached on this traversal, so its subscribers are
                // already queued. A cycle terminates here.
                continue;
            }
            seen.push(cur);
            let Some(node) = self.node(cur) else { continue };
            // A source is never dirty: it is the thing being pushed. A derived
            // node's cached value now disagrees with its binding, so it is.
            if !node.kind.eq(&PropKind::Source) {
                node.dirty.set(true);
            }
            // Only derived nodes go into `pending`, because only they are work:
            // reading a source is a pointer copy. That makes "pending is empty" a
            // truthful answer to "does anything on screen need redrawing".
            if !node.kind.eq(&PropKind::Source) {
                let mut p = self.pending.borrow_mut();
                if !p.contains(&cur) {
                    p.push(cur);
                }
            }
            stack.extend(node.subs.borrow().iter().copied());
        }
        self.needs_frame.set(true);
    }

    /// Read a node, recomputing it if stale. Creates a dependency edge when a
    /// tracker is installed.
    pub fn get(&self, id: PropId) -> Value {
        let tracking = is_tracking();
        self.get_inner(id, tracking)
    }

    fn get_inner(&self, id: PropId, tracking: bool) -> Value {
        let Some(node) = self.node(id) else { return Value::Null };
        if tracking {
            read_tracked(id);
        }
        let Some(binding) = node.binding.clone() else {
            return node.value();
        };
        if !node.dirty.get() || node.evaluating.get() {
            // A cycle: the last known value beats hanging or recursing forever.
            return node.value();
        }
        node.evaluating.set(true);
        let (value, new_deps) = tracked(|| binding());
        node.evaluating.set(false);
        self.rewire(id, &new_deps);
        *node.value.borrow_mut() = value.clone();
        node.dirty.set(false);
        node.recomputes.set(node.recomputes.get() + 1);
        value
    }

    /// Replace this node's dependency set, adding and removing edges as needed.
    fn rewire(&self, id: PropId, new_deps: &[PropId]) {
        let Some(node) = self.node(id) else { return };
        let mut old = node.deps.borrow_mut();
        old.clear();
        old.extend_from_slice(new_deps);
        for &d in new_deps {
            if d == id {
                continue;
            }
            if let Some(dep) = self.node(d) {
                let mut subs = dep.subs.borrow_mut();
                if !subs.contains(&id) {
                    subs.push(id);
                }
            }
        }
    }

    /// Test seam: point a node at a fixed set of dependencies.
    ///
    /// A cycle cannot be built through the public API, and a cycle is exactly
    /// what the traversal's visited set exists to survive, so a test needs a way
    /// to make one.
    #[cfg(test)]
    pub fn rewire_for_test(&self, id: PropId, deps: &[PropId]) {
        self.rewire(id, deps);
    }

    /// Stale node ids since the last call, cleared afterwards.
    pub fn take_pending(&self) -> Vec<PropId> {
        self.needs_frame.set(false);
        std::mem::take(&mut *self.pending.borrow_mut())
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.borrow().is_empty()
    }

    pub fn needs_frame(&self) -> bool {
        self.needs_frame.get()
    }

    /// Nodes that take part in a dependency cycle.
    ///
    /// A cycle shows up as a node reachable from itself through `subs`. It is
    /// reported rather than diagnosed inline because the guard in `get_inner`
    /// already keeps evaluation terminating; this is for `shellctl doctor` and the
    /// debug overlay to point at the config that caused it.
    pub fn cycles(&self) -> Vec<String> {
        let nodes = self.nodes.borrow();
        let mut out = Vec::new();
        for node in nodes.iter() {
            if node.deps.borrow().is_empty() {
                continue;
            }
            let start = node.id;
            let mut seen = vec![false; nodes.len()];
            let mut stack: Vec<PropId> = node.subs.borrow().clone();
            let mut found = false;
            while let Some(cur) = stack.pop() {
                if cur == start {
                    found = true;
                    break;
                }
                if std::mem::replace(&mut seen[cur as usize], true) {
                    continue;
                }
                if let Some(n) = nodes.get(cur as usize) {
                    stack.extend(n.subs.borrow().iter().copied());
                }
            }
            if found {
                out.push(node.path.to_string());
            }
        }
        out
    }

    /// Drop every node, used when a config reload replaces the whole graph.
    pub fn clear(&self) {
        self.nodes.borrow_mut().clear();
        self.by_path.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        self.needs_frame.set(false);
    }

    pub fn stats(&self) -> ReactorStats {
        let nodes = self.nodes.borrow();
        ReactorStats {
            nodes: nodes.len(),
            sources: nodes.iter().filter(|n| n.binding.is_none()).count(),
            derived: nodes.iter().filter(|n| n.binding.is_some()).count(),
            recomputes: nodes.iter().map(|n| n.recomputes.get()).sum(),
            edges: nodes.iter().map(|n| n.deps.borrow().len()).sum(),
            pending: self.pending.borrow().len(),
        }
    }
}

/// Run `f` with a fresh dependency frame, returning what the frame collected.
///
/// The frame is popped even if `f` panics, because the frame stack is a
/// thread-local and a stale frame would mis-attribute every later read.
pub fn tracked<T>(f: impl FnOnce() -> T) -> (T, Vec<PropId>) {
    TRACKER.with(|t| t.borrow_mut().push(Vec::new()));
    let out = f();
    let frame = TRACKER.with(|t| t.borrow_mut().pop()).unwrap_or_default();
    (out, frame)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyInfo {
    pub id: PropId,
    pub path: String,
    pub kind: PropKind,
    pub dirty: bool,
    pub recomputes: u64,
    pub deps: usize,
    pub subs: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReactorStats {
    pub nodes: usize,
    pub sources: usize,
    pub derived: usize,
    pub recomputes: u64,
    pub edges: usize,
    pub pending: usize,
}

impl std::fmt::Display for ReactorStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} nodes ({} source, {} derived), {} edges, {} evaluations, {} pending",
            self.nodes, self.sources, self.derived, self.edges, self.recomputes, self.pending
        )
    }
}

/// A lightweight observable for events that are not property reads
/// (`onClick`, lifecycle hooks, plugin signals).
pub struct Signal<T> {
    handlers: RefCell<Vec<Rc<dyn Fn(&T)>>>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        Signal { handlers: self.handlers.clone() }
    }
}

impl<T> Default for Signal<T> {
    fn default() -> Self {
        Signal { handlers: RefCell::new(Vec::new()) }
    }
}

impl<T> Signal<T> {
    pub fn new() -> Signal<T> {
        Signal::default()
    }

    pub fn connect(&self, f: impl Fn(&T) + 'static) {
        self.handlers.borrow_mut().push(Rc::new(f));
    }

    pub fn emit(&self, value: &T) {
        // Clone first so a handler may connect or disconnect during dispatch.
        let handlers = self.handlers.borrow().clone();
        for h in handlers {
            h(value);
        }
    }

    pub fn len(&self) -> usize {
        self.handlers.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reactor is held behind an `Rc` in real code because bindings capture
    /// it; tests mirror that so a test failure reflects a real usage error.
    fn graph() -> Rc<Reactor> {
        Rc::new(Reactor::new())
    }

    fn value_of(r: &Reactor, path: &str) -> Value {
        r.get(r.lookup(path).expect("path registered"))
    }

    #[test]
    fn source_read_returns_value() {
        let r = graph();
        r.source("a.b", Value::Int(1));
        assert!(value_of(&r, "a.b").loose_eq(&Value::Int(1)));
    }

    #[test]
    fn derived_computes_lazily_on_first_read() {
        let calls = Rc::new(Cell::new(0u32));
        let r = graph();
        r.source("src", Value::Int(3));
        let c = calls.clone();
        let rr = r.clone();
        r.derived("doubled", move || {
            c.set(c.get() + 1);
            Value::Int(rr.get(rr.lookup("src").unwrap()).to_f64_lossy() as i64 * 2)
        });
        assert_eq!(calls.get(), 0, "binding must not run until read");
        assert!(value_of(&r, "doubled").loose_eq(&Value::Int(6)));
        assert_eq!(calls.get(), 1);
        // Nothing was invalidated, so a second read is served from cache.
        assert!(value_of(&r, "doubled").loose_eq(&Value::Int(6)));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn pushing_a_source_invalidates_only_its_dependents() {
        let runs = Rc::new(Cell::new(0u32));
        let r = graph();
        r.source("clock.time", Value::str("12:00"));
        r.source("battery.percentage", Value::Int(50));
        let rr = r.clone();
        let k = runs.clone();
        r.derived("clock.label", move || {
            k.set(k.get() + 1);
            Value::str(format!("t={}", rr.get(rr.lookup("clock.time").unwrap()).to_string_lossy()))
        });
        assert!(value_of(&r, "clock.label").loose_eq(&Value::str("t=12:00")));
        assert_eq!(runs.get(), 1);
        // An unrelated source change must leave the derived node cached.
        r.set("battery.percentage", Value::Int(51));
        assert!(value_of(&r, "clock.label").loose_eq(&Value::str("t=12:00")));
        assert_eq!(runs.get(), 1, "unrelated change must not recompute");
        r.set("clock.time", Value::str("12:01"));
        assert!(value_of(&r, "clock.label").loose_eq(&Value::str("t=12:01")));
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn transitive_invalidation_works() {
        let r = graph();
        r.source("a", Value::Int(1));
        let ra = r.clone();
        r.derived("b", move || {
            Value::Int(ra.get(ra.lookup("a").unwrap()).to_f64_lossy() as i64 + 1)
        });
        let rb = r.clone();
        r.derived("c", move || {
            Value::Int(rb.get(rb.lookup("b").unwrap()).to_f64_lossy() as i64 * 10)
        });
        assert!(value_of(&r, "c").loose_eq(&Value::Int(20)));
        r.set("a", Value::Int(2));
        assert!(value_of(&r, "c").loose_eq(&Value::Int(30)));
    }

    #[test]
    fn equal_value_does_not_invalidate() {
        let r = graph();
        r.source("x", Value::Int(5));
        r.set("x", Value::Int(5));
        assert!(!r.has_pending());
    }

    #[test]
    fn deep_chain_does_not_overflow() {
        let r = graph();
        r.source("n0", Value::Int(0));
        for i in 1..2000 {
            let inner = r.clone();
            r.derived(format!("n{i}"), move || {
                let prev = format!("n{}", i - 1);
                Value::Int(inner.get(inner.lookup(&prev).unwrap()).to_f64_lossy() as i64 + 1)
            });
        }
        assert!(value_of(&r, "n1999").loose_eq(&Value::Int(1999)));
    }

    #[test]
    fn a_dependency_cycle_terminates_and_is_reported() {
        // Build a genuine two-node cycle: `a` reads `b`, `b` reads `a`. Each
        // binding gets its own cell so the back-reference is unambiguous.
        let r = graph();
        let peer_of_a = Rc::new(RefCell::new(None::<PropId>));
        let peer_of_b = Rc::new(RefCell::new(None::<PropId>));

        let pa = peer_of_a.clone();
        let ra = r.clone();
        let a = r.derived("a", move || match *pa.borrow() {
            Some(id) => Value::Int(ra.get(id).to_f64_lossy() as i64 + 1),
            None => Value::Int(0),
        });

        let pb = peer_of_b.clone();
        let rb = r.clone();
        let b = r.derived("b", move || match *pb.borrow() {
            Some(id) => Value::Int(rb.get(id).to_f64_lossy() as i64 + 1),
            None => Value::Int(0),
        });

        *peer_of_a.borrow_mut() = Some(b);
        *peer_of_b.borrow_mut() = Some(a);

        // Reading `a` walks into `b`, which reads `a` again. It must terminate.
        let v = r.get(a);
        assert!(v.to_f64_lossy().is_finite());
        let cycles = r.cycles();
        assert!(
            cycles.len() == 2 && cycles.contains(&"a".to_string()) && cycles.contains(&"b".to_string()),
            "both nodes should be reported as cyclic, got {cycles:?}"
        );
    }

    #[test]
    fn pending_is_drained_once() {
        let r = graph();
        r.source("x", Value::Int(0));
        let rc = r.clone();
        let y = r.derived("y", move || rc.get(rc.lookup("x").expect("x exists")));
        assert_eq!(r.get(y), Value::Int(0));
        assert!(r.take_pending().is_empty());
        r.set("x", Value::Int(1));
        assert_eq!(r.take_pending(), vec![y], "only the derived node is work");
        assert!(r.take_pending().is_empty(), "and it is drained once");
    }

    #[test]
    fn stats_add_up() {
        let r = graph();
        r.source("a", Value::Null);
        r.source("b", Value::Null);
        r.derived("c", || Value::Null);
        let s = r.stats();
        assert_eq!(s.nodes, 3);
        assert_eq!(s.sources, 2);
        assert_eq!(s.derived, 1);
    }

    #[test]
    fn clear_drops_every_node() {
        let r = graph();
        r.source("a", Value::Int(1));
        r.clear();
        assert!(r.is_empty());
        assert!(r.lookup("a").is_none());
    }

    #[test]
    fn tracked_reports_what_was_read() {
        let r = graph();
        let id = r.source("x", Value::Int(1));
        let (_, deps) = tracked(|| r.get(id));
        assert_eq!(deps, vec![id]);
        let (_, none) = tracked(|| 1 + 1);
        assert!(none.is_empty());
    }

    #[test]
    fn signal_delivers_to_every_handler() {
        let s: Signal<i32> = Signal::new();
        let seen = Rc::new(Cell::new(0));
        s.connect({
            let t = seen.clone();
            move |v| t.set(t.get() + v)
        });
        s.connect({
            let t = seen.clone();
            move |v| t.set(t.get() + v)
        });
        s.emit(&2);
        s.emit(&3);
        assert_eq!(seen.get(), 10);
    }

    #[test]
    fn a_source_pushed_twice_reaches_its_subscribers_both_times() {
        // The bug this guards: `invalidate` used a node's own `dirty` flag as the
        // "already walked" guard, and a source's flag is never cleared. So the
        // *second* push to any source skipped its subscribers entirely and every
        // widget kept showing the old value with no error anywhere.
        //
        // A clock does this constantly — it republishes the same second, then the
        // next one — which is why this looked like "the clock sometimes lags".
        let r = Rc::new(Reactor::new());
        r.source("tick", Value::Int(0));
        let rc = r.clone();
        let id = r.derived("out", move || rc.get(rc.lookup("tick").expect("tick exists")));

        assert_eq!(r.get(id), Value::Int(0));
        for expected in [1, 2, 3, 4] {
            assert!(r.set("tick", Value::Int(expected)), "{expected} is a change");
            assert_eq!(
                r.get(id),
                Value::Int(expected),
                "push {expected} did not reach the subscriber"
            );
        }
    }

    #[test]
    fn an_unchanged_push_reaches_nobody() {
        // The other half, and the whole idle budget: the same value twice must
        // not dirty anything, or every provider would redraw the screen forever.
        let r = Rc::new(Reactor::new());
        r.source("v", Value::Int(1));
        let rc = r.clone();
        let id = r.derived("out", move || rc.get(rc.lookup("v").expect("v exists")));
        assert_eq!(r.get(id), Value::Int(1));
        let _ = r.take_pending();

        assert!(!r.set("v", Value::Int(1)), "the same value is not a change");
        assert!(r.take_pending().is_empty(), "nothing should be waiting for a frame");
    }

    #[test]
    fn a_source_nobody_reads_queues_no_work() {
        // `pending` holding only derived nodes is what makes "is anything on
        // screen stale?" a question with a cheap yes/no answer.
        let r = Rc::new(Reactor::new());
        r.source("unread", Value::Int(0));
        let _ = r.take_pending();
        assert!(r.set("unread", Value::Int(1)));
        assert!(
            r.take_pending().is_empty(),
            "a source with no subscribers is not work"
        );

        r.source("read", Value::Int(0));
        let rc = r.clone();
        let id = r.derived("out", move || rc.get(rc.lookup("read").expect("read exists")));
        assert_eq!(r.get(id), Value::Int(0));
        let _ = r.take_pending();
        assert!(r.set("read", Value::Int(1)));
        assert_eq!(r.take_pending(), vec![id], "a read source is work");
    }

    #[test]
    fn a_cycle_in_the_graph_terminates() {
        // The per-traversal visited set is what makes this safe, and a graph that
        // loops on itself must not hang the shell.
        let r = Rc::new(Reactor::new());
        r.source("s", Value::Int(0));
        let a = r.derived("a", || Value::Int(1));
        let rc = r.clone();
        let b = r.derived("b", move || rc.get(a));
        // `a` depends on `b` and `b` on `a`: a cycle the ordinary API cannot make.
        r.rewire_for_test(a, &[b]);
        r.rewire_for_test(b, &[a]);
        assert!(r.set("s", Value::Int(1)));
        assert_eq!(r.get(b), Value::Int(1), "the cycle resolved to a value");
    }
}

