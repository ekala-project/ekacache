//! JSON as nlohmann::json 3.12 (built with `JSON_NOEXCEPTION`) reads and writes it
//! (`behaviors/jig-nix-store.md §JSON decoding`, `§fetchurls output`, Q-NIX-4).
//!
//! Values are `serde_json::Value` with the default `BTreeMap` map, so objects iterate in sorted
//! key order exactly as nlohmann's `std::map` does (`String` order is bytewise order). The parser
//! is our own rather than `serde_json::from_slice`, because two properties of nlohmann's parser
//! decide outcomes and serde_json differs on both:
//!
//! - **no nesting limit.** nlohmann's SAX parser keeps an explicit stack; serde_json stops at
//!   128 levels (its `unbounded_depth` feature is off). `[[[…200 deep…]]]` is an array to
//!   nlohmann, and a parse error to serde_json. The parser here is iterative, and `Doc` drops
//!   deep trees iteratively too.
//! - **a NUL byte outside a string is the end of the input** (nlohmann's lexer returns
//!   `end_of_input` for `'\0'`), so `[]\0junk` parses as `[]`. Inside a string a NUL is an
//!   unescaped control character, a parse error either way. Cutting the input at its first NUL
//!   is therefore equivalent.
//!
//! Everything else is RFC 8259 strict, as both libraries are: one leading UTF-8 BOM is skipped
//! (a partial one is an error), no comments, whitespace is space/TAB/LF/CR only, trailing
//! non-whitespace is an error, strings must be valid UTF-8 (RFC 3629) without raw control
//! bytes, `\u` surrogates must pair, `\u0000` gives a NUL, the last duplicate key wins, and a
//! number that does not fit a finite `double` is an error (nlohmann's `out_of_range.406`).
//!
//! Field access emulates `value(key, default)` and `get<T>()`: a present key of the wrong type,
//! `null` included, and `value()` on a non-object call `abort()` (Q-NIX-4).

use std::borrow::Cow;
use std::ops::Deref;

use serde_json::{Map, Number, Value};

/// nlohmann's `JSON_THROW` under `JSON_NOEXCEPTION`: `std::abort()`, no message (Q-NIX-4).
fn type_error() -> ! {
    // Q-NIX-4: a JSON type error aborts the process
    std::process::abort()
}

/// A parsed document. It drops its tree iteratively: the parser accepts any depth, and
/// `Value`'s own drop recurses once per level.
pub struct Doc(Value);

impl Deref for Doc {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        dismantle(std::mem::take(&mut self.0));
    }
}

/// Drop `v` without recursion.
fn dismantle(v: Value) {
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.into_iter().map(|(_, v)| v)),
            _ => {}
        }
    }
}

/// `nlohmann::json::parse(text, nullptr, /*allow_exceptions=*/false)`: `None` where nlohmann
/// returns a discarded value.
#[must_use]
pub fn parse(input: &[u8]) -> Option<Doc> {
    // nlohmann's lexer reads '\0' as the end of the input
    let end = input.iter().position(|&b| b == 0).unwrap_or(input.len());
    let mut p = Parser {
        s: &input[..end],
        i: 0,
    };
    // `skip_bom`: EF must be followed by BB BF
    if p.s.first() == Some(&0xEF) {
        if p.s.get(1..3) != Some(&[0xBB, 0xBF][..]) {
            return None;
        }
        p.i = 3;
    }
    let doc = Doc(p.document()?);
    p.skip_ws();
    (p.i == p.s.len()).then_some(doc)
}

/// A container being filled, with the key its next value goes under.
enum Open {
    Array(Vec<Value>),
    Object(Map<String, Value>, String),
}

/// The parser's container stack; dropped iteratively when a parse fails part way.
struct Stack(Vec<Open>);

impl Drop for Stack {
    fn drop(&mut self) {
        for open in self.0.drain(..) {
            match open {
                Open::Array(items) => dismantle(Value::Array(items)),
                Open::Object(map, _) => dismantle(Value::Object(map)),
            }
        }
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    /// One value, iteratively.
    fn document(&mut self) -> Option<Value> {
        let mut stack = Stack(Vec::new());
        loop {
            self.skip_ws();
            let mut value = match self.bump()? {
                b'[' => {
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        Value::Array(Vec::new())
                    } else {
                        stack.0.push(Open::Array(Vec::new()));
                        continue;
                    }
                }
                b'{' => {
                    self.skip_ws();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        Value::Object(Map::new())
                    } else {
                        let key = self.key()?;
                        stack.0.push(Open::Object(Map::new(), key));
                        continue;
                    }
                }
                b'"' => Value::String(self.string()?),
                b't' => self.literal(b"rue", Value::Bool(true))?,
                b'f' => self.literal(b"alse", Value::Bool(false))?,
                b'n' => self.literal(b"ull", Value::Null)?,
                c @ (b'-' | b'0'..=b'9') => self.number(c)?,
                _ => return None,
            };
            // attach `value` to its container, closing containers that end here
            loop {
                let Some(top) = stack.0.last_mut() else {
                    return Some(value);
                };
                self.skip_ws();
                match top {
                    Open::Array(items) => {
                        items.push(value);
                        match self.bump()? {
                            b',' => break,
                            b']' => match stack.0.pop() {
                                Some(Open::Array(items)) => value = Value::Array(items),
                                _ => unreachable!("the top of the stack is an array"),
                            },
                            _ => return None,
                        }
                    }
                    Open::Object(map, key) => {
                        // the last duplicate wins
                        if let Some(old) = map.insert(std::mem::take(key), value) {
                            dismantle(old);
                        }
                        match self.bump()? {
                            b',' => {
                                self.skip_ws();
                                *key = self.key()?;
                                break;
                            }
                            b'}' => match stack.0.pop() {
                                Some(Open::Object(map, _)) => value = Value::Object(map),
                                _ => unreachable!("the top of the stack is an object"),
                            },
                            _ => return None,
                        }
                    }
                }
            }
        }
    }

    /// `"key"` then `:`.
    fn key(&mut self) -> Option<String> {
        if self.bump()? != b'"' {
            return None;
        }
        let key = self.string()?;
        self.skip_ws();
        (self.bump()? == b':').then_some(key)
    }

    /// The rest of a literal whose first byte was consumed.
    fn literal(&mut self, rest: &[u8], v: Value) -> Option<Value> {
        let end = self.i + rest.len();
        if self.s.get(self.i..end) != Some(rest) {
            return None;
        }
        self.i = end;
        Some(v)
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, whose first byte was consumed. An
    /// integer is `int64`/`uint64` when it fits (`strtoll`/`strtoull`), else a `double`
    /// (`strtod`, correctly rounded as Rust's parser is), which must be finite.
    fn number(&mut self, first: u8) -> Option<Value> {
        let start = self.i - 1;
        let mut c = first;
        if c == b'-' {
            c = self.bump()?;
        }
        match c {
            b'0' => {}
            b'1'..=b'9' => self.digits(),
            _ => return None,
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.i += 1;
            float = true;
            if !self.bump()?.is_ascii_digit() {
                return None;
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            float = true;
            let mut c = self.bump()?;
            if c == b'+' || c == b'-' {
                c = self.bump()?;
            }
            if !c.is_ascii_digit() {
                return None;
            }
            self.digits();
        }
        // the grammar above admits ASCII only
        let text = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        if !float {
            if first == b'-' {
                if let Ok(n) = text.parse::<i64>() {
                    return Some(Value::Number(n.into()));
                }
            } else if let Ok(n) = text.parse::<u64>() {
                return Some(Value::Number(n.into()));
            }
        }
        let f: f64 = text.parse().ok()?;
        Number::from_f64(f).map(Value::Number)
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = char::from(self.bump()?).to_digit(16)?;
            v = v * 16 + d;
        }
        Some(v)
    }

    /// The rest of a string whose opening `"` was consumed.
    fn string(&mut self) -> Option<String> {
        let mut out = Vec::new();
        loop {
            let c = self.bump()?;
            match c {
                b'"' => break,
                b'\\' => match self.bump()? {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0C),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let mut cp = self.hex4()?;
                        if (0xD800..=0xDBFF).contains(&cp) {
                            if self.bump()? != b'\\' || self.bump()? != b'u' {
                                return None;
                            }
                            let low = self.hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return None;
                            }
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                        } else if (0xDC00..=0xDFFF).contains(&cp) {
                            return None;
                        }
                        let ch = char::from_u32(cp)?;
                        out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                    _ => return None,
                },
                0x00..=0x1F => return None,
                0x20..=0x7F => out.push(c),
                _ => {
                    // RFC 3629 well-formed sequences only (nlohmann's lexer table)
                    let (more, second) = match c {
                        0xC2..=0xDF => (1, 0x80..=0xBF),
                        0xE0 => (2, 0xA0..=0xBF),
                        0xED => (2, 0x80..=0x9F),
                        0xE1..=0xEF => (2, 0x80..=0xBF),
                        0xF0 => (3, 0x90..=0xBF),
                        0xF1..=0xF3 => (3, 0x80..=0xBF),
                        0xF4 => (3, 0x80..=0x8F),
                        _ => return None,
                    };
                    out.push(c);
                    for k in 0..more {
                        let b = self.bump()?;
                        let ok = if k == 0 {
                            second.contains(&b)
                        } else {
                            (0x80..=0xBF).contains(&b)
                        };
                        if !ok {
                            return None;
                        }
                        out.push(b);
                    }
                }
            }
        }
        String::from_utf8(out).ok()
    }
}

/// `get<std::string>()`.
#[must_use]
pub fn get_str(v: &Value) -> &[u8] {
    match v {
        Value::String(s) => s.as_bytes(),
        _ => type_error(),
    }
}

/// `get<std::vector<std::string>>()`: an array whose elements are all strings.
#[must_use]
pub fn get_str_vec(v: &Value) -> Vec<&[u8]> {
    match v {
        Value::Array(items) => items.iter().map(get_str).collect(),
        _ => type_error(),
    }
}

/// The member `key` of `obj`, which must be an object (`value()`'s `type_error.306`).
fn member<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    match obj {
        Value::Object(map) => map.get(key),
        _ => type_error(),
    }
}

/// `obj.value(key, "")`.
#[must_use]
pub fn value_str<'a>(obj: &'a Value, key: &str) -> &'a [u8] {
    member(obj, key).map_or(b"", get_str)
}

/// `obj.value(key, std::vector<std::string>{})`.
#[must_use]
pub fn value_str_vec<'a>(obj: &'a Value, key: &str) -> Vec<&'a [u8]> {
    member(obj, key).map_or_else(Vec::new, get_str_vec)
}

/// `obj.value(key, json::object()).items()`: whatever value is present, iterated.
#[must_use]
pub fn value_items<'a>(obj: &'a Value, key: &str) -> Vec<(Cow<'a, str>, &'a Value)> {
    member(obj, key).map_or_else(Vec::new, items)
}

/// nlohmann `items()` (`§Derivations` "Iteration"): an object's members; an array's elements
/// keyed by their decimal index; nothing for `null`; one entry keyed `""` for a scalar.
#[must_use]
pub fn items(v: &Value) -> Vec<(Cow<'_, str>, &Value)> {
    match v {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (Cow::Borrowed(k.as_str()), v))
            .collect(),
        Value::Array(elems) => elems
            .iter()
            .enumerate()
            .map(|(i, v)| (Cow::Owned(i.to_string()), v))
            .collect(),
        Value::Null => Vec::new(),
        _ => vec![(Cow::Borrowed(""), v)],
    }
}

/// `dump()` of a string (`ensure_ascii = false`, `error_handler_t::strict`): `"` `\` and the
/// control bytes escaped, everything else verbatim. Invalid UTF-8 is `type_error.316`, an abort.
pub fn dump_string(out: &mut Vec<u8>, s: &[u8]) {
    if std::str::from_utf8(s).is_err() {
        // Q-NIX-4: the strict error handler throws, which is abort() here
        type_error();
    }
    out.push(b'"');
    for &c in s {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0C => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x00..=0x1F => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(b"\\u00");
                out.push(HEX[usize::from(c >> 4)]);
                out.push(HEX[usize::from(c & 15)]);
            }
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &[u8]) -> Value {
        parse(s).map(|d| d.0.clone()).expect("parses")
    }

    fn bad(s: &[u8]) {
        assert!(
            parse(s).is_none(),
            "{:?} should not parse",
            String::from_utf8_lossy(s)
        );
    }

    #[test]
    fn grammar() {
        assert_eq!(
            ok(b" [1, -2, 3.5e1, true, false, null, \"x\"] "),
            serde_json::json!([1, -2, 35.0, true, false, null, "x"])
        );
        assert_eq!(
            ok(b"{\"a\":{},\"b\":[ ]}"),
            serde_json::json!({"a": {}, "b": []})
        );
        assert_eq!(ok(b"\t\r\n0"), serde_json::json!(0));
        for s in [
            &b""[..],
            b" ",
            b"[",
            b"]",
            b"[1,]",
            b"{\"a\":1,}",
            b"{a:1}",
            b"{\"a\" 1}",
            b"01",
            b"-",
            b"1.",
            b".5",
            b"1e",
            b"1e+",
            b"+1",
            b"[1 2]",
            b"tru",
            b"truex",
            b"nul",
            b"[] x",
            b"/* c */ []",
            b"[] // c",
            b"\x0c[]",
            b"\"\\x\"",
            b"\"\\u12\"",
            b"'a'",
        ] {
            bad(s);
        }
    }

    #[test]
    fn numbers() {
        assert_eq!(ok(b"18446744073709551615"), serde_json::json!(u64::MAX));
        assert_eq!(ok(b"-9223372036854775808"), serde_json::json!(i64::MIN));
        assert!(ok(b"18446744073709551616").is_f64());
        assert!(ok(b"-9223372036854775809").is_f64());
        assert!(ok(b"1e-400").is_f64());
        assert!(ok(b"0e99999999999999999999").is_f64());
        bad(b"1e400");
        bad(b"-1e400");
        bad(format!("1{}", "0".repeat(400)).as_bytes());
    }

    #[test]
    fn bom_and_nul() {
        assert_eq!(ok(b"\xEF\xBB\xBF[]"), serde_json::json!([]));
        bad(b"\xEF\xBB[]");
        bad(b"\xEF[]");
        bad(b" \xEF\xBB\xBF[]");
        bad(b"\xEF\xBB\xBF\xEF\xBB\xBF[]");
        // nlohmann: '\0' outside a string is the end of the input
        assert_eq!(ok(b"[]\0junk"), serde_json::json!([]));
        assert_eq!(ok(b"1\0x"), serde_json::json!(1));
        bad(b"\0[]");
        bad(b"[\0]");
        bad(b"\"a\0\"");
    }

    #[test]
    fn strings() {
        assert_eq!(
            ok(br#""\"\\\/\b\f\n\r\t""#),
            serde_json::json!("\"\\/\u{8}\u{c}\n\r\t")
        );
        assert_eq!(ok(br#""\u0000""#), serde_json::json!("\0"));
        assert_eq!(
            ok(br#""\u00e9\uD83D\uDE00""#),
            serde_json::json!("\u{e9}\u{1F600}")
        );
        assert_eq!(
            ok("\"é😀\u{7f}\"".as_bytes()),
            serde_json::json!("é😀\u{7f}")
        );
        bad(br#""\uD800""#);
        bad(br#""\uDC00""#);
        bad(br#""\uD800\u0041""#);
        bad(br#""\uD800x""#);
        bad(b"\"\x1f\"");
        bad(b"\"\xff\"");
        bad(b"\"\xc0\x80\"");
        bad(b"\"\xed\xa0\x80\"");
        bad(b"\"\xf4\x90\x80\x80\"");
        bad(b"\"\xe2\x80\"");
        bad(b"\"abc");
    }

    #[test]
    fn duplicates_last_wins_and_sorted() {
        let v = ok(br#"{"b":1,"a":[1],"a":2}"#);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["a", "b"]);
        assert_eq!(v["a"], serde_json::json!(2));
    }

    #[test]
    fn deep_nesting() {
        let n = 200_000;
        let deep = format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(deep.as_bytes()).is_some_and(|d| d.is_array()));
        let deep = format!("{{\"a\":{}0{}}}", "[{\"b\":".repeat(n), "}]".repeat(n));
        assert!(parse(deep.as_bytes()).is_some());
        // a failure deep inside drops the partial tree without recursion
        bad(format!("{}{}", "[".repeat(n), "]".repeat(n - 1)).as_bytes());
        bad(format!("{}{} x", "[".repeat(n), "]".repeat(n)).as_bytes());
    }

    #[test]
    fn iteration() {
        let v = ok(br#"{"o":{"b":1,"a":2},"a":["x","y"],"n":null,"s":"v","i":3}"#);
        let keys = |k: &str| -> Vec<String> {
            value_items(&v, k)
                .into_iter()
                .map(|(k, _)| k.into_owned())
                .collect()
        };
        assert_eq!(keys("o"), ["a", "b"]);
        assert_eq!(keys("a"), ["0", "1"]);
        assert!(keys("n").is_empty());
        assert_eq!(keys("s"), [""]);
        assert_eq!(keys("i"), [""]);
        assert!(keys("absent").is_empty());
        assert_eq!(value_str(&v, "s"), b"v");
        assert_eq!(value_str(&v, "absent"), b"");
        assert_eq!(value_str_vec(&v, "a"), [&b"x"[..], b"y"]);
        assert!(value_str_vec(&v, "absent").is_empty());
    }

    #[test]
    fn dump_escaping() {
        let mut out = Vec::new();
        dump_string(&mut out, b"a\"\\/\x08\t\n\x0c\r\x00\x1f\x7f\xc3\xa9");
        assert_eq!(
            out,
            b"\"a\\\"\\\\/\\b\\t\\n\\f\\r\\u0000\\u001f\x7f\xc3\xa9\""
        );
    }
}
