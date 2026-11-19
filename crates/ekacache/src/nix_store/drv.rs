//! Derivations (`behaviors/jig-nix-store.md §Derivations`): the `add-drv` JSON → ATerm encoder
//! (`§add-drv input → ATerm`) and the `builtin:fetchurl` derivation (`§The fetchurl
//! derivation`).
//!
//! Fields are decoded in the order of the spec's table, because the order decides between a
//! clean failure (a non-string `env` value) and an abort (every other type error, Q-NIX-4).
//! The JSON `"name"` key is never read and references are never derived (Q-NIX-13).

use std::borrow::Cow;

use serde_json::{Map, Value};

use super::json;

/// `Q(s)`: `"` + `s` with `"`, `\`, LF, CR and TAB escaped + `"`; every other byte verbatim.
fn quote(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// `[` + `render(item)` joined by `,` + `]`.
fn list<T>(out: &mut Vec<u8>, items: &[T], mut render: impl FnMut(&mut Vec<u8>, &T)) {
    out.push(b'[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        render(out, item);
    }
    out.push(b']');
}

/// Entries sorted by key, bytewise (the `std::map` the C++ collects them into).
fn sorted<T>(mut entries: Vec<(Cow<'_, str>, T)>) -> Vec<(Cow<'_, str>, T)> {
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    entries
}

/// `DerivationToATerm(json_text)`: the ATerm, or `None` for "derivation JSON did not parse"
/// (a parse error, a non-object top level, or a non-string `env` value). Other type errors
/// abort.
#[must_use]
pub fn derivation_to_aterm(json_text: &[u8]) -> Option<Vec<u8>> {
    let doc = json::parse(json_text)?;
    if !doc.is_object() {
        return None;
    }
    aterm(&doc)
}

/// The encoder over a parsed object.
fn aterm(drv: &Value) -> Option<Vec<u8>> {
    // 1. outputs: entries, values not checked yet
    let outputs = sorted(json::value_items(drv, "outputs"));
    // 2. inputDrvs: each value an array of strings (abort), names sorted, duplicates kept (Q-NIX-5)
    let input_drvs = sorted(
        json::value_items(drv, "inputDrvs")
            .into_iter()
            .map(|(path, outs)| {
                let mut names = json::get_str_vec(outs);
                names.sort_unstable();
                (path, names)
            })
            .collect(),
    );
    // 3. inputSrcs: an array of strings (abort, `null` included), sorted, duplicates kept (Q-NIX-5)
    let mut input_srcs = json::value_str_vec(drv, "inputSrcs");
    input_srcs.sort_unstable();
    // 4. env: a non-string value is the clean failure, not an abort
    let mut env = Vec::new();
    for (key, value) in json::value_items(drv, "env") {
        let Value::String(s) = value else {
            return None;
        };
        env.push((key, s.as_bytes()));
    }
    let env = sorted(env);
    // 5. args: an array of strings (abort), order kept
    let args = json::value_str_vec(drv, "args");

    let mut out = b"Derive(".to_vec();
    // 6. each output spec, while rendering: an object with string `path`, `hashAlgo`, `hash`
    list(&mut out, &outputs, |out, (name, spec)| {
        out.push(b'(');
        quote(out, name.as_bytes());
        out.push(b',');
        quote(out, json::value_str(spec, "path"));
        out.push(b',');
        quote(out, json::value_str(spec, "hashAlgo"));
        out.push(b',');
        quote(out, json::value_str(spec, "hash"));
        out.push(b')');
    });
    out.push(b',');
    list(&mut out, &input_drvs, |out, (path, names)| {
        out.push(b'(');
        quote(out, path.as_bytes());
        out.push(b',');
        list(out, names, |out, n| quote(out, n));
        out.push(b')');
    });
    out.push(b',');
    list(&mut out, &input_srcs, |out, s| quote(out, s));
    // 7. system, builder: strings (abort)
    out.push(b',');
    quote(&mut out, json::value_str(drv, "system"));
    out.push(b',');
    quote(&mut out, json::value_str(drv, "builder"));
    out.push(b',');
    list(&mut out, &args, |out, a| quote(out, a));
    out.push(b',');
    list(&mut out, &env, |out, (key, value)| {
        out.push(b'(');
        quote(out, key.as_bytes());
        out.push(b',');
        quote(out, value);
        out.push(b')');
    });
    out.push(b')');
    Some(out)
}

/// A JSON string from bytes that came out of validated JSON (or the store path built from
/// them). Legacy's `dump()` of the derivation object would abort on invalid UTF-8; only a
/// non-UTF-8 `JIG_STORE_DIR` could get here.
fn json_string(s: &[u8]) -> Value {
    match std::str::from_utf8(s) {
        Ok(s) => Value::String(s.to_owned()),
        // Q-NIX-4: `dump()`'s strict UTF-8 check aborts
        Err(_) => std::process::abort(),
    }
}

/// `FetchurlDerivation(file, out)`: the ATerm of the `builtin:fetchurl` derivation for one
/// `fetchurls` element, shaped as `<nix/fetchurl.nix>` makes it. Each field of `file` is read
/// as a string (absent → `""`, any other type or a non-object `file` aborts).
///
/// Legacy builds the JSON object, serialises it, re-parses it and encodes it; the round trip is
/// lossless, so the object goes to the encoder directly.
#[must_use]
pub fn fetchurl_derivation(file: &Value, out: &[u8]) -> Vec<u8> {
    let name = json::value_str(file, "name");
    let algo = json::value_str(file, "algo");
    let sri = json::value_str(file, "sri");
    let url = json::value_str(file, "url");
    let hex = json::value_str(file, "hex");
    let oha: &[u8] = if sri.starts_with(&[algo, b"-"].concat()) {
        b""
    } else {
        algo
    };
    let object = |pairs: &[(&str, Value)]| -> Value {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect::<Map<String, Value>>(),
        )
    };
    let s = json_string;
    let drv = object(&[
        ("name", s(name)),
        ("system", s(b"builtin")),
        ("builder", s(b"builtin:fetchurl")),
        ("args", Value::Array(Vec::new())),
        (
            "outputs",
            object(&[(
                "out",
                object(&[("path", s(out)), ("hashAlgo", s(algo)), ("hash", s(hex))]),
            )]),
        ),
        ("inputDrvs", Value::Object(Map::new())),
        ("inputSrcs", Value::Array(Vec::new())),
        (
            "env",
            object(&[
                ("name", s(name)),
                ("out", s(out)),
                ("outputHash", s(sri)),
                ("outputHashAlgo", s(oha)),
                ("outputHashMode", s(b"flat")),
                ("url", s(url)),
                ("urls", s(url)),
                ("executable", s(b"")),
                ("unpack", s(b"")),
                (
                    "impureEnvVars",
                    s(b"http_proxy https_proxy ftp_proxy all_proxy no_proxy"),
                ),
                ("preferLocalBuild", s(b"1")),
                ("system", s(b"builtin")),
                ("builder", s(b"builtin:fetchurl")),
            ]),
        ),
    ]);
    // every env value is a string, so the encoder cannot report failure
    aterm(&drv).unwrap_or_else(|| unreachable!("fetchurl env values are strings"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aterm_of(json: &str) -> Option<String> {
        derivation_to_aterm(json.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    fn vectors() -> ekacache_testkit::vectors::VectorFile {
        ekacache_testkit::vectors::VectorFile::load("jig-nix-store")
    }

    /// `§Test vectors` `### ATerm [test]`: the `aterm` row of
    /// `tests/vectors/jig-nix-store.toml`.
    #[test]
    fn aterm_vector() {
        let file = vectors();
        let v = file.one("aterm");
        assert_eq!(aterm_of(v.str("json")).as_deref(), Some(v.str("aterm")));
    }

    #[test]
    fn empty_and_iteration_shapes() {
        assert_eq!(
            aterm_of("{}").as_deref(),
            Some(r#"Derive([],[],[],"","",[],[])"#)
        );
        // arrays iterate with index keys sorted as strings, scalars as key "", null as nothing
        let got = aterm_of(
            r#"{"env":"x","outputs":null,"inputDrvs":[["b","a","a"]],"inputSrcs":["b","a","b"]}"#,
        );
        assert_eq!(
            got.as_deref(),
            Some(r#"Derive([],[("0",["a","a","b"])],["a","b","b"],"","",[],[("","x")])"#)
        );
        let many: Vec<String> = (0..11).map(|i| format!("\"v{i}\"")).collect();
        let got = aterm_of(&format!("{{\"env\":[{}]}}", many.join(",")));
        assert!(
            got.unwrap()
                .contains(r#"[("0","v0"),("1","v1"),("10","v10"),("2","v2")"#)
        );
    }

    #[test]
    fn quoting() {
        let got = aterm_of(r#"{"system":"a\"b\\c\nd\re\tf\u0000g\u0001é"}"#).unwrap();
        assert!(got.contains("\"a\\\"b\\\\c\\nd\\re\\tf\0g\u{1}é\""));
    }

    #[test]
    fn failures() {
        for input in [
            "",
            "abc",
            "[]",
            "\"x\"",
            "1",
            "null",
            r#"{"env":{"a":1}}"#,
            r#"{"env":[null]}"#,
            r#"{"env":5}"#,
        ] {
            assert_eq!(aterm_of(input), None, "{input}");
        }
        // a non-string env value is a failure even when args (checked later) is bad
        assert_eq!(aterm_of(r#"{"env":{"a":1},"args":5,"system":1}"#), None);
    }

    /// `§Test vectors` `### fetchurls`: the ATerm of `foo-1.0.tar.gz.drv`.
    #[test]
    fn fetchurl_vector() {
        let vectors = vectors();
        let v = vectors.one("fetchurls");
        // the one entry of the stdin array
        let entry = v.bytes("stdin").strip_prefix(b"[").unwrap();
        let file = json::parse(entry.strip_suffix(b"]").unwrap()).unwrap();
        assert_eq!(
            String::from_utf8(fetchurl_derivation(&file, v.bytes("out"))).unwrap(),
            v.str("aterm")
        );
        // `outputHashAlgo` is `algo` when `sri` does not start with `algo-`; absent fields are ""
        let file = json::parse(br#"{"algo":"sha512","sri":"sha256-x"}"#).unwrap();
        let got = String::from_utf8(fetchurl_derivation(&file, b"/o")).unwrap();
        assert!(got.contains(r#"("outputHashAlgo","sha512")"#));
        assert!(got.contains(r#"("name",""),"#));
    }
}
