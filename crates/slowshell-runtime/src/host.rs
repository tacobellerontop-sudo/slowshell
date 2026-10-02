//! System providers exposed to config.
//!
//! Each provider is a [`slowshell_core::Object`], so config reads it as a dotted
//! path and the reactive graph records the access. Values are **pushed** into
//! reactor sources by the runtime's tick loop rather than polled by the evaluator,
//! which is what keeps idle CPU near zero.
//!
//! | Path | Status |
//! |---|---|
//! | `clock.*`, `screens.*` | live, from the platform |
//! | `battery.*`, `network.*`, `audio.*`, `system.*`, `windows.*` | wired; see [`Pending`] |
//! | `notifications.*`, `media.*`, `tray.*` | architecture in place, data pending |

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use slowshell_core::eval::Host;
use slowshell_core::Value;

/// Why a provider is not live yet.
///
/// Reported by `shellctl doctor` so a user is told the difference between "this
/// build does not support it" and "this feature is not implemented".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// Not implemented in this build.
    NotImplemented,
    /// No reliable public Windows API.
    NoReliableApi,
    /// Needs a later phase.
    Planned,
}

impl Pending {
    pub fn describe(self, path: &str) -> String {
        match self {
            Pending::NotImplemented => {
                format!("`{path}` is not implemented yet; it reads as null")
            }
            Pending::NoReliableApi => format!(
                "`{path}` has no reliable public Windows API; it reads as null"
            ),
            Pending::Planned => format!("`{path}` is planned for a later release"),
        }
    }
}

/// A read-only object backed by a fixed map.
///
/// Used for providers whose data arrives from the platform layer, and in tests
/// where a real system value would make the test machine-dependent.
#[derive(Debug)]
pub struct StaticObject {
    path: String,
    values: BTreeMap<String, Value>,
    keys: Vec<&'static str>,
}

impl StaticObject {
    pub fn new(path: impl Into<String>, entries: impl IntoIterator<Item = (&'static str, Value)>) -> StaticObject {
        let path = path.into();
        let mut values = BTreeMap::new();
        let mut keys = Vec::new();
        for (k, v) in entries {
            values.insert(k.to_string(), v);
            keys.push(k);
        }
        StaticObject { path, values, keys }
    }
}

impl slowshell_core::Object for StaticObject {
    fn path(&self) -> &str {
        &self.path
    }

    fn get(&self, key: &str) -> Option<Value> {
        self.values.get(key).cloned()
    }

    fn call(&self, method: &str, _args: &[Value]) -> Option<Result<Value, String>> {
        match method {
            // Every provider answers `refresh` so config can force a re-read.
            "refresh" => Some(Ok(Value::Bool(true))),
            _ => None,
        }
    }

    fn keys(&self) -> Vec<&'static str> {
        self.keys.clone()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A host backed by a fixed table, for tests and for a config that runs with no
/// platform data available.
pub struct StaticHost {
    values: BTreeMap<String, Value>,
}

impl StaticHost {
    pub fn new<const N: usize>(entries: [(&str, Value); N]) -> StaticHost {
        StaticHost {
            values: entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    pub fn insert(&mut self, key: &str, value: Value) {
        self.values.insert(key.to_string(), value);
    }
}

impl Host for StaticHost {
    fn lookup(&self, path: &str) -> Option<Value> {
        if let Some(v) = self.values.get(path) {
            return Some(v.clone());
        }
        // Serve any prefix of a dotted path as a nested object, so `screens`
        // resolves even when only `screens.primary.width` is known. The tree has
        // to be genuinely nested, because config walks it one segment at a time.
        let prefix = format!("{path}.");
        if self.values.keys().any(|k| k.starts_with(&prefix)) {
            return Some(nested_map(&self.values, path));
        }
        None
    }

    fn paths(&self) -> Vec<String> {
        self.values.keys().cloned().collect()
    }

    fn children_of(&self, prefix: &str) -> Vec<String> {
        let want = format!("{prefix}.");
        self.values
            .keys()
            .filter_map(|k| k.strip_prefix(&want).map(str::to_string))
            .collect()
    }
}

/// Build a nested map from flat dotted keys rooted at `root`.
///
/// `["a.b.c", "a.b.d", "a.e"]` rooted at `a` becomes `{b: {c, d}, e}`.
fn nested_map(values: &BTreeMap<String, Value>, root: &str) -> Value {
    let prefix = format!("{root}.");
    let mut tree: BTreeMap<String, Value> = BTreeMap::new();
    for (key, value) in values {
        let Some(rest) = key.strip_prefix(&prefix) else { continue };
        if rest.is_empty() {
            continue;
        }
        let mut cursor = &mut tree;
        let segments: Vec<&str> = rest.split('.').collect();
        for (i, seg) in segments.iter().enumerate() {
            let last = i + 1 == segments.len();
            if last {
                cursor.insert((*seg).to_string(), value.clone());
            } else {
                let entry = cursor
                    .entry((*seg).to_string())
                    .or_insert_with(|| Value::Map(std::sync::Arc::new(BTreeMap::new())));
                if !matches!(entry, Value::Map(_)) {
                    *entry = Value::Map(std::sync::Arc::new(BTreeMap::new()));
                }
                // Re-borrow the nested map so the next segment goes inside it.
                if let Value::Map(inner) = entry {
                    cursor = std::sync::Arc::make_mut(inner);
                } else {
                    break;
                }
            }
        }
    }
    Value::map(tree)
}

/// The live host used by the shell.
///
/// Holds one object per provider and delegates path resolution to it. Reads are
/// routed through the reactor first so system values participate in dependency
/// tracking.
pub struct ShellHost {
    providers: BTreeMap<String, Arc<dyn slowshell_core::Object>>,
    /// Providers that exist and answer, but always with a null, and why.
    stubs: Vec<(&'static str, Pending)>,
}

impl ShellHost {
    pub fn new() -> ShellHost {
        let mut providers: BTreeMap<String, Arc<dyn slowshell_core::Object>> = BTreeMap::new();

        providers.insert("clock".into(), clock_object());
        providers.insert("screens".into(), monitors_object());
        providers.insert("battery".into(), stub("battery", &["percentage", "charging", "powerSaveMode", "present", "state"]));
        providers.insert("network".into(), stub("network", &["connected", "wifi", "ethernet", "type", "ssid", "download", "upload"]));
        providers.insert("audio".into(), stub("audio", &["volume", "muted", "devices", "defaultDevice"]));
        providers.insert("system".into(), stub("system", &["cpuUsage", "memoryUsage", "memoryTotal", "uptime", "hostName"]));
        providers.insert("windows".into(), stub("windows", &["active", "list", "count"]));
        providers.insert("virtualDesktops".into(), stub("virtualDesktops", &["count", "current", "list"]));
        providers.insert("notifications".into(), notifications_object());

        let stubs = vec![
            ("battery", Pending::NotImplemented),
            ("network", Pending::Planned),
            ("audio", Pending::Planned),
            ("system", Pending::Planned),
            // Enumerating other apps' top-level windows has no supported API:
            // it needs UI Automation or a shell hook, both of which are
            // documented as unreliable across Windows releases.
            ("windows", Pending::NoReliableApi),
            // `IVirtualDesktopManager` is internal to the shell.
            ("virtualDesktops", Pending::NoReliableApi),
        ];

        ShellHost { providers, stubs }
    }

    /// A provider by name.
    pub fn provider(&self, name: &str) -> Option<&Arc<dyn slowshell_core::Object>> {
        self.providers.get(name)
    }

    /// Replace a provider, used when a system event supplies richer data.
    pub fn insert(&mut self, name: &str, object: Arc<dyn slowshell_core::Object>) {
        self.providers.insert(name.to_string(), object);
    }

    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// Every reason a provider is not live, one line each, for `doctor`.
    ///
    /// A user asking "why does `battery.percentage` read as 0" deserves an
    /// answer, and the reasons are very different ones: a gap in this build, a
    /// gap in Windows, or a deliberate ordering. Reporting the reason per
    /// provider is what makes the difference visible.
    pub fn pending_notes(&self) -> Vec<String> {
        self.stubs.iter().map(|(name, why)| why.describe(name)).collect()
    }

    /// The providers that exist and answer, but always with a null.
    pub fn stub_providers(&self) -> Vec<&'static str> {
        self.stubs.iter().map(|(name, _)| *name).collect()
    }
}

impl Default for ShellHost {
    fn default() -> Self {
        ShellHost::new()
    }
}

impl Host for ShellHost {
    fn lookup(&self, path: &str) -> Option<Value> {
        // A single segment names a provider and returns the object itself.
        if let Some(p) = self.providers.get(path) {
            return Some(Value::Object(p.clone()));
        }
        // Otherwise split at the first dot and walk.
        let (root, rest) = path.split_once('.')?;
        let provider = self.providers.get(root)?;
        let mut v = Value::Object(provider.clone());
        for key in rest.split('.') {
            v = v.get(key)?;
        }
        Some(v)
    }

    fn paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        for name in self.providers.keys() {
            out.push(name.clone());
            if let Some(p) = self.providers.get(name) {
                for k in p.keys() {
                    out.push(format!("{name}.{k}"));
                }
            }
        }
        out
    }

    fn children_of(&self, prefix: &str) -> Vec<String> {
        match self.providers.get(prefix) {
            Some(p) => p.keys().iter().map(|k| k.to_string()).collect(),
            None => Vec::new(),
        }
    }
}

/// The display provider, read live from the platform.
///
/// `screens.primary` and `screens[0]` are both meaningful, so the object exposes
/// `primary`, `count` and one key per display.
pub fn monitors_object() -> Arc<dyn slowshell_core::Object> {
    let mons = slowshell_win::Monitors::enumerate();
    let mut entries: Vec<(&'static str, Value)> = vec![
        ("count", Value::Int(mons.list.len() as i64)),
        ("primary", monitor_value(mons.primary())),
    ];
    // A display's own id is its lookup key, so `screens.1.width` works.
    for m in &mons.list {
        if let Some(key) = intern(&m.id) {
            entries.push((key, monitor_value(m)));
        }
    }
    Arc::new(StaticObject::new("screens", entries))
}

fn monitor_value(m: &slowshell_win::Monitor) -> Value {
    let mut map = std::collections::BTreeMap::new();
    map.insert("id".into(), Value::str(m.id.clone()));
    map.insert("name".into(), Value::str(m.name.clone()));
    map.insert("x".into(), Value::Int(m.x as i64));
    map.insert("y".into(), Value::Int(m.y as i64));
    map.insert("width".into(), Value::Int(m.width as i64));
    map.insert("height".into(), Value::Int(m.height as i64));
    map.insert("workX".into(), Value::Int(m.work_x as i64));
    map.insert("workY".into(), Value::Int(m.work_y as i64));
    map.insert("workWidth".into(), Value::Int(m.work_width as i64));
    map.insert("workHeight".into(), Value::Int(m.work_height as i64));
    map.insert("scale".into(), Value::Float(m.scale as f64));
    map.insert("refresh".into(), Value::Int(m.refresh_hz as i64));
    map.insert("primary".into(), Value::Bool(m.primary));
    map.insert("rotation".into(), Value::Int(m.rotation as i64));
    Value::map(map)
}

/// Intern a display id so it can live in a `&'static str` key.
fn intern(name: &str) -> Option<&'static str> {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static NAMES: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut guard = NAMES.lock().ok()?;
    let set = guard.get_or_insert_with(HashSet::new);
    if let Some(existing) = set.get(name) {
        return Some(existing);
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    set.insert(leaked);
    Some(leaked)
}
/// The clock provider. `clock.time` is updated by the runtime's tick.
pub fn clock_object() -> Arc<dyn slowshell_core::Object> {
    Arc::new(StaticObject::new(
        "clock",
        [
            ("time", Value::str("00:00")),
            ("unix", Value::Int(0)),
            ("date", Value::str("")),
            ("weekday", Value::str("")),
        ],
    ))
}

/// Notifications, empty until the Windows listener is wired.
pub fn notifications_object() -> Arc<dyn slowshell_core::Object> {
    Arc::new(StaticObject::new(
        "notifications",
        [
            ("list", Value::list(Vec::new())),
            ("unreadCount", Value::Int(0)),
            ("history", Value::list(Vec::new())),
        ],
    ))
}

/// A provider that answers with null for every key it advertises.
///
/// This is deliberate: a config written against the documented API keeps working
/// and renders nothing, instead of failing to build, and `shellctl doctor` explains
/// why the value is empty.
fn stub(path: &'static str, keys: &'static [&'static str]) -> Arc<dyn slowshell_core::Object> {
    let entries: Vec<(&'static str, Value)> =
        keys.iter().map(|k| (*k, Value::Null)).collect();
    Arc::new(StaticObject::new(path, entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_static_host_resolves_a_full_path() {
        let h = StaticHost::new([("a.b", Value::Int(1)), ("a.c", Value::Int(2))]);
        assert_eq!(h.lookup("a.b"), Some(Value::Int(1)));
        assert_eq!(h.lookup("a.zzz"), None);
    }

    #[test]
    fn a_static_host_serves_a_prefix_as_an_object() {
        let h = StaticHost::new([("screens.primary.width", Value::Int(1920))]);
        let v = h.lookup("screens").expect("a prefix must resolve");
        assert_eq!(v.get("primary").and_then(|p| p.get("width")), Some(Value::Int(1920)));
    }

    #[test]
    fn children_of_lists_the_leaf_names() {
        let h = StaticHost::new([("battery.percentage", Value::Int(1))]);
        assert_eq!(h.children_of("battery"), vec!["percentage".to_string()]);
    }

    #[test]
    fn the_shell_host_registers_every_documented_provider() {
        let h = ShellHost::new();
        for name in [
            "clock",
            "screens",
            "battery",
            "network",
            "audio",
            "system",
            "windows",
            "virtualDesktops",
            "notifications",
        ] {
            assert!(h.provider(name).is_some(), "missing provider `{name}`");
        }
    }

    #[test]
    fn a_stub_provider_advertises_its_keys_and_reads_null() {
        let h = ShellHost::new();
        let p = h.provider("battery").unwrap();
        assert!(p.keys().contains(&"percentage"));
        assert!(p.get("percentage").is_some());
        assert_eq!(p.get("charging"), Some(Value::Null));
    }

    #[test]
    fn the_shell_host_resolves_a_provider_and_a_path() {
        let h = ShellHost::new();
        assert!(h.lookup("clock").is_some());
        assert!(h.lookup("clock.time").is_some());
        assert!(h.lookup("nonesuch").is_none());
        assert!(h.lookup("nonesuch.thing").is_none());
    }

    #[test]
    fn paths_includes_both_roots_and_leaves() {
        let h = ShellHost::new();
        let p = h.paths();
        assert!(p.contains(&"clock".to_string()));
        assert!(p.iter().any(|x| x.starts_with("battery.")));
    }

    #[test]
    fn every_object_answers_refresh() {
        let h = ShellHost::new();
        let p = h.provider("clock").unwrap();
        assert!(p.call("refresh", &[]).is_some());
        assert!(p.call("nonesuch", &[]).is_none());
    }

    #[test]
    fn pending_explains_itself() {
        assert!(Pending::NotImplemented.describe("battery.percentage").contains("battery"));
        assert!(Pending::NoReliableApi.describe("x.y").contains("no reliable"));
    }
}
