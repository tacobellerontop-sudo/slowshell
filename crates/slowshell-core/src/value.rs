//! The dynamic value type shared by the language, the reactive graph and the widget layer.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::color::Color;
use crate::diag::{type_name, TypeError, ValueResult};

/// A user-defined object exposed to config, e.g. `screens` or `battery`.
///
/// Objects are `Arc`-shared and read-only from the language side; mutation always
/// goes through a named method so the runtime can keep the UI graph informed.
pub trait Object: fmt::Debug + Send + Sync + 'static {
    /// Full path used in diagnostics, e.g. `system.cpu`.
    fn path(&self) -> &str;
    fn get(&self, key: &str) -> Option<Value>;
    /// `Some(true)` handled, `Some(false)` handled with wrong arity/type, `None` unknown.
    fn call(&self, method: &str, args: &[Value]) -> Option<Result<Value, String>>;
    /// Property names, for `did you mean` hints.
    fn keys(&self) -> Vec<&'static str> {
        Vec::new()
    }
    fn as_any(&self) -> &dyn Any;
}

use std::any::Any;

/// A method value: either a language expression or a native function.
#[derive(Clone)]
pub struct Callable {
    pub name: Arc<str>,
    pub native: Option<Arc<dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync>>,
    /// Source of a language-defined call, for tracing and error messages.
    pub source: Option<Arc<str>>,
}

impl fmt::Debug for Callable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<callable {}>", self.name)
    }
}

/// The single value type flowing through the system.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Arc<str>),
    Color(Color),
    List(Arc<[Value]>),
    Map(Arc<BTreeMap<String, Value>>),
    Object(Arc<dyn Object>),
    Callable(Arc<Callable>),
}

impl Default for Value {
    fn default() -> Self {
        Value::Null
    }
}

impl Value {
    pub fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Arc::from(s.as_ref()))
    }
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Arc::from(items.into_boxed_slice()))
    }
    pub fn map(m: BTreeMap<String, Value>) -> Value {
        Value::Map(Arc::from(m))
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn type_name(&self) -> &'static str {
        type_name(self)
    }

    // ---- truthiness -------------------------------------------------------
    // Mirrors what a user expects from a scripting language: `if (battery.charging)`.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
            _ => true,
        }
    }

    // ---- typed access, with errors that name both sides -------------------
    pub fn as_bool(&self) -> ValueResult<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Int(i) => Ok(*i != 0),
            _ => Err(self.mismatch("bool")),
        }
    }

    pub fn as_f64(&self) -> ValueResult<f64> {
        match self {
            Value::Int(i) => Ok(*i as f64),
            Value::Float(f) => Ok(*f),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            _ => Err(self.mismatch("number")),
        }
    }

    pub fn as_i64(&self) -> ValueResult<i64> {
        match self {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => {
                if f.is_finite() {
                    Ok(*f as i64)
                } else {
                    Err(self.mismatch("int"))
                }
            }
            _ => Err(self.mismatch("int")),
        }
    }

    pub fn as_str(&self) -> ValueResult<&str> {
        match self {
            Value::Str(s) => Ok(s),
            _ => Err(self.mismatch("string")),
        }
    }

    pub fn as_color(&self) -> ValueResult<Color> {
        match self {
            Value::Color(c) => Ok(*c),
            // A bare string that happens to look like a color is accepted; themes
            // routinely pass hex strings through variables.
            Value::Str(s) => Color::parse(s).ok_or_else(|| self.mismatch("color")),
            _ => Err(self.mismatch("color")),
        }
    }

    pub fn as_list(&self) -> ValueResult<&[Value]> {
        match self {
            Value::List(l) => Ok(l),
            Value::Null => Ok(&[]),
            _ => Err(self.mismatch("list")),
        }
    }

    pub fn as_object(&self) -> ValueResult<&Arc<dyn Object>> {
        match self {
            Value::Object(o) => Ok(o),
            _ => Err(self.mismatch("object")),
        }
    }

    /// Index into a list or a map, `None` when out of range or key missing.
    pub fn index(&self, key: &Value) -> Option<Value> {
        match (self, key) {
            (Value::List(l), Value::Int(i)) => {
                let i = if *i < 0 { l.len() as i64 + i } else { *i };
                if i < 0 {
                    None
                } else {
                    l.get(i as usize).cloned()
                }
            }
            (Value::Map(m), Value::Str(k)) => m.get(k.as_ref()).cloned(),
            (Value::Object(o), Value::Str(k)) => o.get(k.as_ref()),
            (Value::List(l), Value::Str(k)) => k.parse::<usize>().ok().and_then(|i| l.get(i).cloned()),
            (Value::Null, _) => None,
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        match self {
            Value::Map(m) => m.get(key).cloned(),
            Value::Object(o) => o.get(key),
            _ => None,
        }
    }

    pub fn mismatch(&self, expected: &'static str) -> TypeError {
        TypeError { expected, got: self.display_short(), got_type: self.type_name() }
    }

    /// A compact, human-readable rendering used in type errors and logs.
    pub fn display_short(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => format!("\"{s}\""),
            Value::Color(c) => c.to_string(),
            Value::List(l) => format!("[{} items]", l.len()),
            Value::Map(m) => format!("{{{} keys}}", m.len()),
            Value::Object(o) => o.path().to_string(),
            Value::Callable(c) => format!("{}()", c.name),
        }
    }

    // ---- lenient conversions used by layout and styling -------------------
    // These never fail: unparseable input falls back so a bad value degrades the
    // visuals instead of tearing down a panel.
    pub fn to_f64_lossy(&self) -> f64 {
        match self {
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Str(s) => s.trim().parse::<f64>().unwrap_or(0.0),
            Value::Color(c) => c.luminance() as f64,
            _ => 0.0,
        }
    }

    pub fn to_color_lossy(&self) -> Color {
        match self {
            Value::Color(c) => *c,
            Value::Str(s) => Color::parse(s).unwrap_or(Color::TRANSPARENT),
            _ => Color::TRANSPARENT,
        }
    }

    pub fn to_string_lossy(&self) -> String {
        match self {
            Value::Str(s) => s.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Bool(b) => b.to_string(),
            Value::Color(c) => c.to_string(),
            other => other.display_short(),
        }
    }

    /// Best-effort equality for the reactive layer: changing a value must not
    /// schedule a frame, so `1` and `1.0` compare equal and lists compare deeply.
    pub fn loose_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
                (*a as f64) == *b
            }
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Color(a), Value::Color(b)) => a == b,
            (Value::List(a), Value::List(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.loose_eq(y))
            }
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va.loose_eq(vb))
            }
            (Value::Object(a), Value::Object(b)) => Arc::ptr_eq(a, b),
            (Value::Callable(a), Value::Callable(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// JSON projection, used by the plugin API and `shellctl inspect`.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Int(i) => J::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
            Value::Str(s) => J::String(s.to_string()),
            Value::Color(c) => J::String(c.to_string()),
            Value::List(l) => J::Array(l.iter().map(|v| v.to_json()).collect()),
            Value::Map(m) => {
                J::Object(m.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
            }
            Value::Object(o) => {
                let mut map = serde_json::Map::new();
                for k in o.keys() {
                    if let Some(v) = o.get(k) {
                        map.insert(k.to_string(), v.to_json());
                    }
                }
                J::Object(map)
            }
            Value::Callable(c) => J::String(format!("<callable {}>", c.name)),
        }
    }
}

/// Trim float noise so `50.0` prints as `50` but `50.5` keeps its fraction.
pub fn format_float(f: f64) -> String {
    if !f.is_finite() {
        return format!("{f}");
    }
    if f == f.trunc() && f.abs() < 1e15 {
        return format!("{}", f as i64);
    }
    let mut s = format!("{f:.4}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.loose_eq(other)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i as i64)
    }
}
impl From<u32> for Value {
    fn from(i: u32) -> Self {
        Value::Int(i as i64)
    }
}
impl From<usize> for Value {
    fn from(i: usize) -> Self {
        Value::Int(i as i64)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::str(s)
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::str(s)
    }
}
impl From<Color> for Value {
    fn from(c: Color) -> Self {
        Value::Color(c)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_matches_scripting_expectations() {
        assert!(!Value::Null.truthy());
        assert!(!Value::str("").truthy());
        assert!(Value::str("x").truthy());
        assert!(!Value::Float(0.0).truthy());
        assert!(Value::Int(1).truthy());
    }

    #[test]
    fn int_and_float_compare_equal() {
        assert!(Value::Int(3).loose_eq(&Value::Float(3.0)));
        assert!(!Value::Int(3).loose_eq(&Value::Float(3.5)));
    }

    #[test]
    fn float_formatting_drops_trailing_noise() {
        assert_eq!(format_float(50.0), "50");
        assert_eq!(format_float(50.5), "50.5");
    }

    #[test]
    fn negative_list_index_counts_from_end() {
        let l = Value::list(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(l.index(&Value::Int(-1)).unwrap().loose_eq(&Value::Int(3)));
    }

    #[test]
    fn string_is_accepted_as_color() {
        let c = Value::str("#89b4fa").as_color().unwrap();
        assert_eq!(c, Color::rgb(0x89, 0xb4, 0xfa));
    }

    #[test]
    fn type_error_names_both_sides() {
        let e = Value::str("hi").as_i64().unwrap_err();
        assert_eq!(e.expected, "int");
        assert_eq!(e.got_type, "string");
    }
}
