//! A reader for Quint's ITF traces (`spec/README.md` §Traces; implementation/12 §Model-based
//! tests), on `serde_json`.
//!
//! ITF is JSON with a few tagged objects: `{"#bigint": "N"}` for integers, `{"#tup": [..]}`,
//! `{"#set": [..]}`, `{"#map": [[k, v], ..]}`. Records are plain objects, lists plain arrays.
//! Sum types (`Option`, the `mbt::nondetPicks` values) are records `{"tag": .., "value": ..}`.
//! A trace file is `{"#meta": .., "vars": [..], "states": [..]}`; each state is a record of the
//! model's variables plus `#meta`, and with `quint run --mbt` also `mbt::actionTaken` and
//! `mbt::nondetPicks`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// An ITF value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<Value>),
    Tup(Vec<Value>),
    Set(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Record(BTreeMap<String, Value>),
}

/// A malformed trace or a value of the wrong shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItfError(pub String);

impl fmt::Display for ItfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ItfError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ItfError> {
    Err(ItfError(msg.into()))
}

impl Value {
    /// Convert parsed JSON.
    pub fn from_json(j: &serde_json::Value) -> Result<Value, ItfError> {
        use serde_json::Value as J;
        Ok(match j {
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => return err(format!("number {n} is not an i64")),
            },
            J::String(s) => Value::Str(s.clone()),
            J::Array(a) => Value::List(a.iter().map(Value::from_json).collect::<Result<_, _>>()?),
            J::Object(o) => {
                if let Some(J::String(n)) = o.get("#bigint") {
                    return match n.parse() {
                        Ok(i) => Ok(Value::Int(i)),
                        Err(_) => err(format!("#bigint {n} does not fit an i64")),
                    };
                }
                if let Some(J::Array(a)) = o.get("#tup") {
                    return Ok(Value::Tup(
                        a.iter().map(Value::from_json).collect::<Result<_, _>>()?,
                    ));
                }
                if let Some(J::Array(a)) = o.get("#set") {
                    return Ok(Value::Set(
                        a.iter().map(Value::from_json).collect::<Result<_, _>>()?,
                    ));
                }
                if let Some(J::Array(a)) = o.get("#map") {
                    let mut out = Vec::with_capacity(a.len());
                    for pair in a {
                        match pair.as_array().map(Vec::as_slice) {
                            Some([k, v]) => out.push((Value::from_json(k)?, Value::from_json(v)?)),
                            _ => return err("#map entry is not a pair"),
                        }
                    }
                    return Ok(Value::Map(out));
                }
                if o.contains_key("#unserializable") {
                    return err("unserializable value in trace");
                }
                let mut rec = BTreeMap::new();
                for (k, v) in o {
                    if k != "#meta" {
                        rec.insert(k.clone(), Value::from_json(v)?);
                    }
                }
                Value::Record(rec)
            }
            J::Null => return err("null in trace"),
        })
    }

    pub fn int(&self) -> Result<i64, ItfError> {
        match self {
            Value::Int(i) => Ok(*i),
            v => err(format!("expected an int, got {v:?}")),
        }
    }

    pub fn bool(&self) -> Result<bool, ItfError> {
        match self {
            Value::Bool(b) => Ok(*b),
            v => err(format!("expected a bool, got {v:?}")),
        }
    }

    pub fn str(&self) -> Result<&str, ItfError> {
        match self {
            Value::Str(s) => Ok(s),
            v => err(format!("expected a string, got {v:?}")),
        }
    }

    /// The elements of a list, tuple or set.
    pub fn elems(&self) -> Result<&[Value], ItfError> {
        match self {
            Value::List(v) | Value::Tup(v) | Value::Set(v) => Ok(v),
            v => err(format!("expected a list, tuple or set, got {v:?}")),
        }
    }

    pub fn map(&self) -> Result<&[(Value, Value)], ItfError> {
        match self {
            Value::Map(m) => Ok(m),
            v => err(format!("expected a map, got {v:?}")),
        }
    }

    /// A record field.
    pub fn field(&self, name: &str) -> Result<&Value, ItfError> {
        match self {
            Value::Record(r) => match r.get(name) {
                Some(v) => Ok(v),
                None => err(format!("record has no field {name}")),
            },
            v => err(format!("expected a record with field {name}, got {v:?}")),
        }
    }

    /// A sum-type value: `(tag, payload)`.
    pub fn variant(&self) -> Result<(&str, &Value), ItfError> {
        Ok((self.field("tag")?.str()?, self.field("value")?))
    }

    /// The payload of `Some(x)`, or `None`.
    pub fn option(&self) -> Result<Option<&Value>, ItfError> {
        match self.variant()? {
            ("Some", v) => Ok(Some(v)),
            ("None", _) => Ok(None),
            (tag, _) => err(format!("expected Some or None, got {tag}")),
        }
    }
}

/// One state of a trace: the model's variables by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub vars: BTreeMap<String, Value>,
}

impl State {
    pub fn var(&self, name: &str) -> Result<&Value, ItfError> {
        match self.vars.get(name) {
            Some(v) => Ok(v),
            None => err(format!("state has no variable {name}")),
        }
    }

    /// `mbt::actionTaken`: the action that led to this state (`init` for the first).
    pub fn action(&self) -> Result<&str, ItfError> {
        self.var("mbt::actionTaken")?.str()
    }

    /// One of `mbt::nondetPicks`, `None` when the action did not pick it.
    pub fn pick(&self, name: &str) -> Result<Option<&Value>, ItfError> {
        self.var("mbt::nondetPicks")?.field(name)?.option()
    }
}

/// A whole trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub vars: Vec<String>,
    pub states: Vec<State>,
}

impl Trace {
    pub fn parse(text: &str) -> Result<Trace, ItfError> {
        let j: serde_json::Value =
            serde_json::from_str(text).map_err(|e| ItfError(format!("trace is not JSON: {e}")))?;
        let vars = match j.get("vars").and_then(|v| v.as_array()) {
            Some(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            None => Vec::new(),
        };
        let Some(states) = j.get("states").and_then(|v| v.as_array()) else {
            return err("trace has no states");
        };
        let mut out = Vec::with_capacity(states.len());
        for s in states {
            match Value::from_json(s)? {
                Value::Record(vars) => out.push(State { vars }),
                v => return err(format!("state is not a record: {v:?}")),
            }
        }
        Ok(Trace { vars, states: out })
    }

    pub fn read(path: &Path) -> Result<Trace, ItfError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ItfError(format!("{}: {e}", path.display())))?;
        Trace::parse(&text).map_err(|e| ItfError(format!("{}: {e}", path.display())))
    }
}

/// The `*.itf.json` files under `tests/itf/<model>/` of the workspace, sorted; empty when the
/// directory does not exist (traces are generated locally and not committed).
pub fn trace_files(model: &str) -> Vec<PathBuf> {
    let dir = workspace_root().join("tests").join("itf").join(model);
    let mut out: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".itf.json"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// The workspace root: the parent of `crates/`.
pub fn workspace_root() -> PathBuf {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    here.parent()
        .and_then(Path::parent)
        .map_or_else(|| here.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_itf_encodings() {
        let text = r##"{"#meta":{"format":"ITF"},"vars":["x","m"],"states":[
            {"#meta":{"index":0},"x":{"#bigint":"-1"},"b":true,"l":[1,{"#bigint":"2"}],
             "t":{"#tup":["a",{"#bigint":"3"}]},"s":{"#set":[]},
             "m":{"#map":[[{"#tup":[{"#bigint":"0"},{"#bigint":"1"}]},{"f":"v"}]]},
             "mbt::actionTaken":"init",
             "mbt::nondetPicks":{"k":{"tag":"Some","value":"a"},"v":{"tag":"None","value":{"#tup":[]}}}}]}"##;
        let t = Trace::parse(text).unwrap();
        assert_eq!(t.vars, ["x", "m"]);
        let s = &t.states[0];
        assert_eq!(s.var("x").unwrap().int().unwrap(), -1);
        assert!(s.var("b").unwrap().bool().unwrap());
        assert_eq!(
            s.var("l").unwrap().elems().unwrap(),
            [Value::Int(1), Value::Int(2)]
        );
        assert_eq!(s.var("t").unwrap().elems().unwrap()[0].str().unwrap(), "a");
        assert!(s.var("s").unwrap().elems().unwrap().is_empty());
        let (k, v) = &s.var("m").unwrap().map().unwrap()[0];
        assert_eq!(k, &Value::Tup(vec![Value::Int(0), Value::Int(1)]));
        assert_eq!(v.field("f").unwrap().str().unwrap(), "v");
        assert_eq!(s.action().unwrap(), "init");
        assert_eq!(s.pick("k").unwrap().unwrap().str().unwrap(), "a");
        assert!(s.pick("v").unwrap().is_none());
        assert!(s.var("#meta").is_err());
    }
}
