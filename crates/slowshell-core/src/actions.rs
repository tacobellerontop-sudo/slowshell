//! The action registry: what `onClick: something.doThing()` resolves to.
//!
//! Config can *read* the system through the host and *write* to it through this
//! registry. Keeping the two separate is deliberate: reading is reactive and
//! cheap, writing is a side effect that must happen on the shell's own thread, at
//! a time of its choosing, with a real answer if it fails.
//!
//! It lives here, in the language crate, rather than in the runtime because it is
//! a property of the *language*: what a config may name, and what happens when
//! it names something that does not exist. The runtime and the platform each
//! register into it; neither owns it.
//!
//! ## Why a registry and not a method on the provider object
//!
//! The obvious design is `battery.setChargeLimit(80)` on the same object that
//! answers `battery.percentage`. It is the wrong one. Reading a path is
//! allowed from inside a reactive expression, and a bare write hidden behind the
//! same accessor means any future `screen.width = …` typo silently becomes a
//! write to a system object during layout. A separate namespace — actions are
//! always a call, never a bare name — makes the destructive half of the language
//! visible at the call site.
//!
//! ## The signature is checked here
//!
//! Config is not trusted to be well-formed about arity, so a mismatch is a
//! diagnostic at *call* time, not a panic and not silence. Every failure names
//! the action, what it got, and what it expected.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::diag::closest;
use crate::Value;

/// What one action does. `Rc` rather than `fn` so an action can capture shell
/// state without a global.
pub type ActionFn = Rc<dyn Fn(&[Value]) -> Result<Value, String>>;

/// One registered action.
#[derive(Clone)]
pub struct ActionSpec {
    /// Dotted path, e.g. `launcher.toggle`.
    pub path: String,
    /// One line for `shellctl actions` and the docs.
    pub doc: String,
    /// Parameter names, for arity and type errors.
    pub args: Vec<&'static str>,
    pub run: ActionFn,
}

impl std::fmt::Debug for ActionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The closure is not printable and is not interesting; the shape is.
        f.debug_struct("ActionSpec")
            .field("path", &self.path)
            .field("args", &self.args)
            .finish()
    }
}

/// Every action a config may call.
#[derive(Default)]
pub struct Registry {
    specs: BTreeMap<String, ActionSpec>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry { specs: BTreeMap::new() }
    }

    /// Register an action. Re-registering a path replaces it, which is what a
    /// shell restart after an extension loads does.
    pub fn register(
        &mut self,
        path: &str,
        doc: &str,
        args: &[&'static str],
        run: impl Fn(&[Value]) -> Result<Value, String> + 'static,
    ) {
        self.specs.insert(
            path.to_string(),
            ActionSpec {
                path: path.to_string(),
                doc: doc.to_string(),
                args: args.to_vec(),
                run: Rc::new(run),
            },
        );
    }

    pub fn get(&self, path: &str) -> Option<&ActionSpec> {
        self.specs.get(path)
    }

    pub fn paths(&self) -> Vec<&str> {
        self.specs.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    /// Every registered action, for `shellctl actions` and the docs build.
    pub fn describe(&self) -> Vec<String> {
        self.specs.values().map(describe_spec).collect()
    }

    /// Run an action.
    ///
    /// Three failure modes, all reported rather than swallowed: the action does
    /// not exist (with a near-miss suggestion), the arguments are the wrong
    /// shape, or the action itself declined.
    pub fn call(&self, path: &[String], args: &[Value]) -> Result<Value, String> {
        let dotted = path.join(".");
        let Some(spec) = self.specs.get(&dotted) else {
            return Err(unknown_action(&dotted, &self.paths()));
        };
        if args.len() != spec.args.len() {
            let wanted = if spec.args.is_empty() {
                "no arguments".to_string()
            } else {
                format!("{} ({})", spec.args.len(), spec.args.join(", "))
            };
            return Err(format!(
                "`{dotted}` takes {wanted}, but was given {} ({}).",
                args.len(),
                args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(", ")
            ));
        }
        (spec.run)(args)
    }
}

/// The error for a name that is not an action, with the nearest match if there
/// is one plausibly near.
fn unknown_action(dotted: &str, names: &[&str]) -> String {
    match closest(dotted, names, 1).first() {
        Some(near) => format!("`{dotted}` is not an action. Did you mean `{near}`?"),
        None => format!("`{dotted}` is not an action."),
    }
}

fn describe_spec(spec: &ActionSpec) -> String {
    let sig = if spec.args.is_empty() {
        spec.path.clone()
    } else {
        format!("{}({})", spec.path, spec.args.join(", "))
    };
    format!("  {sig:<34} {}", spec.doc)
}

/// Check a path against a registry without running it.
///
/// Used at build time so `onClick: launhcer.toggle()` is a diagnostic in the
/// editor rather than a shrug at the moment the user clicks.
pub fn check(registry: &Registry, path: &[String]) -> Result<(), String> {
    let dotted = path.join(".");
    if registry.get(&dotted).is_some() {
        return Ok(());
    }
    Err(unknown_action(&dotted, &registry.paths()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    fn registry() -> Registry {
        let mut r = Registry::new();
        r.register("launcher.toggle", "Show or hide the launcher.", &[], |_| {
            Ok(Value::Bool(true))
        });
        r.register("audio.setVolume", "Set the output volume.", &["level"], |args| {
            Ok(args[0].clone())
        });
        r
    }

    fn p(s: &str) -> Vec<String> {
        s.split('.').map(str::to_string).collect()
    }

    #[test]
    fn an_action_runs_with_no_arguments() {
        let r = registry();
        let v = r.call(&p("launcher.toggle"), &[]).unwrap();
        assert_eq!(v, Value::Bool(true));
    }

    #[test]
    fn an_argument_reaches_the_action() {
        let r = registry();
        let v = r.call(&p("audio.setVolume"), &[Value::Int(40)]).unwrap();
        assert_eq!(v, Value::Int(40));
    }

    #[test]
    fn a_misspelled_action_suggests_the_real_one() {
        let r = registry();
        let e = r.call(&p("launcher.toggl"), &[]).unwrap_err();
        assert!(e.contains("Did you mean `launcher.toggle`"), "{e}");
    }

    #[test]
    fn an_unknown_action_names_itself() {
        let r = registry();
        let e = r.call(&p("nonsense.thing"), &[]).unwrap_err();
        assert!(e.contains("`nonsense.thing` is not an action"), "{e}");
        // Nothing similar exists, so no misleading suggestion.
        assert!(!e.contains("Did you mean"), "{e}");
    }

    #[test]
    fn the_wrong_number_of_arguments_is_reported_with_both_counts() {
        let r = registry();
        let e = r.call(&p("audio.setVolume"), &[]).unwrap_err();
        assert!(e.contains("takes 1 (level)"), "{e}");
        assert!(e.contains("given 0"), "{e}");
        let e = r.call(&p("launcher.toggle"), &[Value::Int(1)]).unwrap_err();
        assert!(e.contains("takes no arguments"), "{e}");
    }

    #[test]
    fn a_failing_action_reports_its_own_reason() {
        let mut r = registry();
        r.register("test.fail", "Always fails.", &[], |_| Err("the disk is full".into()));
        let e = r.call(&p("test.fail"), &[]).unwrap_err();
        assert_eq!(e, "the disk is full");
    }

    #[test]
    fn check_reports_the_same_as_call_without_running() {
        let r = registry();
        assert!(check(&r, &p("launcher.toggle")).is_ok());
        let e = check(&r, &p("launhcer.toggle")).unwrap_err();
        assert!(e.contains("Did you mean"), "{e}");
    }

    #[test]
    fn describing_an_action_shows_its_signature() {
        let r = registry();
        let all = r.describe().join("\n");
        assert!(all.contains("launcher.toggle"), "{all}");
        assert!(all.contains("audio.setVolume(level)"), "{all}");
    }

    #[test]
    fn an_empty_registry_never_suggests_anything() {
        let r = Registry::new();
        assert!(r.is_empty());
        let e = r.call(&p("launcher.toggle"), &[]).unwrap_err();
        assert!(!e.contains("Did you mean"), "{e}");
    }
}
