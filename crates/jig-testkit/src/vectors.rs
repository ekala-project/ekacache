//! The L1 spec-vector files (`implementation/12-testing.md §L1: spec vectors`).
//!
//! Each behaviour doc's test vectors are extracted by hand into `tests/vectors/<doc>.toml`, one
//! `[[vector]]` table per entry, and the tests read them from there. Every entry names the doc
//! section it comes from in `section`, a heading path such as `"Test vectors > Nix32"`; the
//! `vector_files` test checks those headings exist and that every vector heading of
//! `behaviors/` is named by some entry. An entry whose `kind` is `doc-only` is not checked
//! mechanically, and its `note` says why.
//!
//! No TOML crate is in the vetted set (`implementation/01-workspace.md §Dependencies`), so this
//! module reads the subset the files use:
//!
//! - `# comments`, blank lines, top-level `key = value` lines, and `[[vector]]` headers;
//! - bare keys (`A-Z a-z 0-9 _ -`);
//! - basic strings `"…"` with the escapes `\b \t \n \f \r \" \\ \uXXXX \UXXXXXXXX`, and literal
//!   strings `'…'` (no escapes);
//! - decimal or `0x` integers (with `_` separators), `true` and `false`;
//! - arrays of these, which may span lines and end in a trailing comma.
//!
//! Anything else is a parse error naming the line, so a file in a richer TOML dialect is
//! rejected instead of being misread.

use std::fmt;
use std::path::{Path, PathBuf};

/// One TOML value of the subset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<Value>),
}

/// A parse error: the file, the 1-based line and what was wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// One `[[vector]]` table (or the top-level table), keys in file order.
#[derive(Clone, Debug, Default)]
pub struct Vector {
    /// Where it is, for messages: `<file>:<line of the header>`.
    pub origin: String,
    entries: Vec<(String, Value)>,
}

impl Vector {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    fn missing(&self, key: &str, what: &str) -> ! {
        panic!("{}: `{key}` is missing or not {what}", self.origin)
    }

    pub fn opt_str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            None => None,
            Some(Value::Str(s)) => Some(s),
            Some(_) => self.missing(key, "a string"),
        }
    }

    pub fn str(&self, key: &str) -> &str {
        self.opt_str(key)
            .unwrap_or_else(|| self.missing(key, "a string"))
    }

    /// A string as bytes (paths and data are bytes to the code under test).
    pub fn bytes(&self, key: &str) -> &[u8] {
        self.str(key).as_bytes()
    }

    pub fn opt_bytes(&self, key: &str) -> Option<&[u8]> {
        self.opt_str(key).map(str::as_bytes)
    }

    /// A string of hex digits, whitespace ignored, decoded.
    pub fn hex(&self, key: &str) -> Vec<u8> {
        decode_hex(self.str(key)).unwrap_or_else(|e| panic!("{}: `{key}`: {e}", self.origin))
    }

    pub fn opt_int(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            None => None,
            Some(Value::Int(n)) => Some(*n),
            Some(_) => self.missing(key, "an integer"),
        }
    }

    pub fn int(&self, key: &str) -> i64 {
        self.opt_int(key)
            .unwrap_or_else(|| self.missing(key, "an integer"))
    }

    /// A non-negative integer as a `usize`.
    pub fn usize(&self, key: &str) -> usize {
        usize::try_from(self.int(key)).unwrap_or_else(|_| self.missing(key, "non-negative"))
    }

    pub fn opt_bool(&self, key: &str) -> Option<bool> {
        match self.get(key) {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => self.missing(key, "a boolean"),
        }
    }

    pub fn bool(&self, key: &str) -> bool {
        self.opt_bool(key)
            .unwrap_or_else(|| self.missing(key, "a boolean"))
    }

    pub fn opt_strs(&self, key: &str) -> Option<Vec<&str>> {
        match self.get(key) {
            None => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|v| match v {
                        Value::Str(s) => s.as_str(),
                        _ => self.missing(key, "an array of strings"),
                    })
                    .collect(),
            ),
            Some(_) => self.missing(key, "an array of strings"),
        }
    }

    pub fn strs(&self, key: &str) -> Vec<&str> {
        self.opt_strs(key)
            .unwrap_or_else(|| self.missing(key, "an array of strings"))
    }

    /// An array of strings as byte strings.
    pub fn byte_strs(&self, key: &str) -> Vec<Vec<u8>> {
        self.strs(key)
            .into_iter()
            .map(|s| s.as_bytes().to_vec())
            .collect()
    }

    pub fn ints(&self, key: &str) -> Vec<i64> {
        match self.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::Int(n) => *n,
                    _ => self.missing(key, "an array of integers"),
                })
                .collect(),
            _ => self.missing(key, "an array of integers"),
        }
    }

    /// `kind`: which test consumes the entry.
    pub fn kind(&self) -> &str {
        self.str("kind")
    }

    /// `section`: the heading path in the doc.
    pub fn section(&self) -> &str {
        self.str("section")
    }

    /// `id` if given (e.g. `TV-6`), else the origin; for assertion messages.
    pub fn label(&self) -> String {
        match self.opt_str("id") {
            Some(id) => format!("{id} ({})", self.origin),
            None => self.origin.clone(),
        }
    }
}

/// The kind of entry no test checks; it must carry a `note`.
pub const DOC_ONLY: &str = "doc-only";

/// A parsed `tests/vectors/<doc>.toml`.
#[derive(Clone, Debug)]
pub struct VectorFile {
    /// `<doc>`, the stem of the behaviour doc.
    pub stem: String,
    /// The top-level keys before the first `[[vector]]`.
    pub top: Vector,
    pub vectors: Vec<Vector>,
}

impl VectorFile {
    /// Reads `tests/vectors/<stem>.toml`. Panics on an unreadable or malformed file, or an
    /// entry without `kind` or `section`.
    pub fn load(stem: &str) -> Self {
        let path = vectors_dir().join(format!("{stem}.toml"));
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let file = Self::parse(stem, &format!("tests/vectors/{stem}.toml"), &text)
            .unwrap_or_else(|e| panic!("{e}"));
        for v in &file.vectors {
            let _ = (v.kind(), v.section());
        }
        file
    }

    /// Parses `text`; `name` is used in messages.
    pub fn parse(stem: &str, name: &str, text: &str) -> Result<Self, ParseError> {
        let (top, vectors) = parse_tables(name, text)?;
        Ok(Self {
            stem: stem.to_owned(),
            top,
            vectors,
        })
    }

    /// The entries of `kind`, at least `min` of them (so a consumer never silently checks
    /// fewer vectors than it was written for).
    pub fn of_kind(&self, kind: &str, min: usize) -> Vec<&Vector> {
        let found: Vec<&Vector> = self.vectors.iter().filter(|v| v.kind() == kind).collect();
        assert!(
            found.len() >= min,
            "tests/vectors/{}.toml: {} `{kind}` vectors, want at least {min}",
            self.stem,
            found.len()
        );
        found
    }

    /// The single entry of `kind`.
    pub fn one(&self, kind: &str) -> &Vector {
        let found = self.of_kind(kind, 1);
        assert_eq!(
            found.len(),
            1,
            "tests/vectors/{}.toml: more than one `{kind}`",
            self.stem
        );
        found[0]
    }
}

/// The workspace root (two levels above this crate's manifest).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// `tests/vectors/`.
pub fn vectors_dir() -> PathBuf {
    workspace_root().join("tests/vectors")
}

/// `behaviors/`.
pub fn behaviors_dir() -> PathBuf {
    workspace_root().join("behaviors")
}

/// Lowercase or uppercase hex digits, whitespace ignored.
pub fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err(format!("odd number of hex digits ({})", digits.len()));
    }
    digits
        .chunks(2)
        .map(|p| {
            let hi = (p[0] as char).to_digit(16);
            let lo = (p[1] as char).to_digit(16);
            match (hi, lo) {
                (Some(h), Some(l)) => Ok((h * 16 + l) as u8),
                _ => Err(format!("not hex: {:?}", String::from_utf8_lossy(p))),
            }
        })
        .collect()
}

// ---- markdown headings -------------------------------------------------------------------

/// One ATX heading of a markdown doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    /// The number of `#`.
    pub level: usize,
    /// The text after the `#`s, trimmed.
    pub text: String,
}

/// The ATX headings of `markdown`, in order, skipping fenced code blocks.
pub fn headings(markdown: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let level = line.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&level) && line[level..].starts_with(' ') {
            out.push(Heading {
                level,
                text: line[level..].trim().to_owned(),
            });
        }
    }
    out
}

/// Whether `section` (`"A > B > C"`) names a chain of nested headings: `A` anywhere, each
/// later component a heading inside the previous one's section.
pub fn section_exists(headings: &[Heading], section: &str) -> bool {
    let parts: Vec<&str> = section.split(" > ").map(str::trim).collect();
    let first = parts[0];
    'start: for (i, h) in headings.iter().enumerate() {
        if h.text != first {
            continue;
        }
        let (mut at, mut level) = (i, h.level);
        for part in &parts[1..] {
            let mut found = None;
            for (j, g) in headings.iter().enumerate().skip(at + 1) {
                if g.level <= level {
                    break;
                }
                if g.text == *part {
                    found = Some((j, g.level));
                    break;
                }
            }
            match found {
                Some((j, l)) => (at, level) = (j, l),
                None => continue 'start,
            }
        }
        return true;
    }
    false
}

/// Whether a heading is a vector heading (`/vector/i`).
pub fn is_vector_heading(h: &Heading) -> bool {
    h.text.to_ascii_lowercase().contains("vector")
}

// ---- the parser --------------------------------------------------------------------------

struct Cursor<'a> {
    name: &'a str,
    text: &'a [u8],
    at: usize,
    line: usize,
}

impl Cursor<'_> {
    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            file: self.name.to_owned(),
            line: self.line,
            message: message.into(),
        })
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.at += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn eat(&mut self, s: &[u8]) -> bool {
        if self.text[self.at..].starts_with(s) {
            for _ in 0..s.len() {
                self.bump();
            }
            true
        } else {
            false
        }
    }

    /// Spaces and tabs.
    fn skip_blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.bump();
        }
    }

    /// Spaces, tabs, newlines and comments (inside arrays and between lines).
    fn skip_space(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => {
                    self.bump();
                }
                Some(b'#') => self.skip_comment(),
                _ => return,
            }
        }
    }

    fn skip_comment(&mut self) {
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.bump();
        }
    }

    /// The rest of the line must be blank or a comment.
    fn end_of_line(&mut self) -> Result<(), ParseError> {
        self.skip_blank();
        if self.peek() == Some(b'#') {
            self.skip_comment();
        }
        self.eat(b"\r");
        match self.peek() {
            None => Ok(()),
            Some(b'\n') => {
                self.bump();
                Ok(())
            }
            Some(c) => self.err(format!("unexpected {:?} after the value", c as char)),
        }
    }

    fn key(&mut self) -> Result<String, ParseError> {
        let start = self.at;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            self.bump();
        }
        if start == self.at {
            return self.err("expected a bare key");
        }
        Ok(String::from_utf8_lossy(&self.text[start..self.at]).into_owned())
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            Some(b'"') => self.basic_string().map(Value::Str),
            Some(b'\'') => self.literal_string().map(Value::Str),
            Some(b'[') => self.array(),
            Some(b't') if self.eat(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Value::Bool(false)),
            Some(c) if c.is_ascii_digit() || c == b'-' || c == b'+' => self.integer(),
            Some(c) => self.err(format!("unsupported value starting with {:?}", c as char)),
            None => self.err("expected a value"),
        }
    }

    fn basic_string(&mut self) -> Result<String, ParseError> {
        if self.eat(b"\"\"\"") {
            return self.err("multi-line strings are not in the subset");
        }
        self.bump();
        let mut out = String::new();
        let mut run = Vec::new();
        loop {
            if matches!(self.peek(), None | Some(b'\n')) {
                return self.err("unterminated string");
            }
            match self.bump() {
                None => return self.err("unterminated string"),
                Some(b'"') => break,
                Some(b'\\') => {
                    let c = match self.bump() {
                        Some(b'b') => '\u{8}',
                        Some(b't') => '\t',
                        Some(b'n') => '\n',
                        Some(b'f') => '\u{c}',
                        Some(b'r') => '\r',
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'u') => self.unicode(4)?,
                        Some(b'U') => self.unicode(8)?,
                        Some(c) => return self.err(format!("unknown escape \\{}", c as char)),
                        None => return self.err("unterminated string"),
                    };
                    flush(&mut out, &mut run, self)?;
                    out.push(c);
                }
                Some(c) if c < 0x20 && c != b'\t' || c == 0x7f => {
                    return self.err(format!("control byte {c:#04x} in a string"));
                }
                Some(c) => run.push(c),
            }
        }
        flush(&mut out, &mut run, self)?;
        Ok(out)
    }

    fn unicode(&mut self, n: usize) -> Result<char, ParseError> {
        let mut v = 0u32;
        for _ in 0..n {
            let Some(d) = self.bump().and_then(|c| (c as char).to_digit(16)) else {
                return self.err("bad \\u escape");
            };
            v = v * 16 + d;
        }
        match char::from_u32(v) {
            Some(c) => Ok(c),
            None => self.err(format!("\\u escape {v:#x} is not a scalar value")),
        }
    }

    fn literal_string(&mut self) -> Result<String, ParseError> {
        if self.eat(b"'''") {
            return self.err("multi-line strings are not in the subset");
        }
        self.bump();
        let start = self.at;
        loop {
            if matches!(self.peek(), None | Some(b'\n')) {
                return self.err("unterminated string");
            }
            match self.bump() {
                None => return self.err("unterminated string"),
                Some(b'\'') => break,
                Some(c) if c < 0x20 && c != b'\t' || c == 0x7f => {
                    return self.err(format!("control byte {c:#04x} in a string"));
                }
                Some(_) => {}
            }
        }
        match std::str::from_utf8(&self.text[start..self.at - 1]) {
            Ok(s) => Ok(s.to_owned()),
            Err(_) => self.err("a string that is not UTF-8"),
        }
    }

    fn integer(&mut self) -> Result<Value, ParseError> {
        let start = self.at;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+'))
        {
            self.bump();
        }
        let raw = String::from_utf8_lossy(&self.text[start..self.at]).replace('_', "");
        let parsed = if let Some(hex) = raw.strip_prefix("0x") {
            i64::from_str_radix(hex, 16).ok()
        } else if raw.len() > 1 && raw.trim_start_matches(['-', '+']).starts_with('0') {
            None
        } else {
            raw.parse::<i64>().ok()
        };
        match parsed {
            Some(n) => Ok(Value::Int(n)),
            None => self.err(format!("not an integer of the subset: {raw}")),
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b"]") {
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.skip_space();
            if self.eat(b",") {
                continue;
            }
            self.skip_space();
            if self.eat(b"]") {
                return Ok(Value::Array(items));
            }
            return self.err("expected `,` or `]` in an array");
        }
    }
}

fn flush(out: &mut String, run: &mut Vec<u8>, c: &Cursor<'_>) -> Result<(), ParseError> {
    match std::str::from_utf8(run) {
        Ok(s) => out.push_str(s),
        Err(_) => return c.err("a string that is not UTF-8"),
    }
    run.clear();
    Ok(())
}

fn parse_tables(name: &str, text: &str) -> Result<(Vector, Vec<Vector>), ParseError> {
    let mut c = Cursor {
        name,
        text: text.as_bytes(),
        at: 0,
        line: 1,
    };
    let mut top = Vector {
        origin: format!("{name}:top"),
        entries: Vec::new(),
    };
    let mut vectors: Vec<Vector> = Vec::new();
    loop {
        c.skip_space();
        let Some(ch) = c.peek() else { break };
        if ch == b'[' {
            if !c.eat(b"[[vector]]") {
                return c.err("only `[[vector]]` table headers are in the subset");
            }
            vectors.push(Vector {
                origin: format!("{name}:{}", c.line),
                entries: Vec::new(),
            });
            c.end_of_line()?;
            continue;
        }
        let key_line = c.line;
        let key = c.key()?;
        c.skip_blank();
        if !c.eat(b"=") {
            return c.err(format!("expected `=` after `{key}`"));
        }
        c.skip_blank();
        let value = c.value()?;
        c.end_of_line()?;
        let table = vectors.last_mut().unwrap_or(&mut top);
        if table.get(&key).is_some() {
            return Err(ParseError {
                file: name.to_owned(),
                line: key_line,
                message: format!("duplicate key `{key}`"),
            });
        }
        table.entries.push((key, value));
    }
    Ok((top, vectors))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<VectorFile, ParseError> {
        VectorFile::parse("t", "t.toml", text)
    }

    #[test]
    fn the_subset() {
        let f = parse(
            "# c\ndoc = 'x.md'\n\n[[vector]]\nkind = \"a\" # trailing\nsection = \"S\"\n\
             s = \"q\\\"\\\\\\n\\t\\u00e9\\U0001F600\"\nl = 'a\\b\"c'\nn = -12\nh = 0xff_ff\n\
             b = true\narr = [\n  \"x\", # one\n  'y',\n]\nnested = [[1, 2], []]\n[[vector]]\nkind = \"b\"\n",
        )
        .unwrap();
        assert_eq!(f.top.str("doc"), "x.md");
        assert_eq!(f.vectors.len(), 2);
        let v = &f.vectors[0];
        assert_eq!(v.origin, "t.toml:4");
        assert_eq!(v.str("s"), "q\"\\\n\té\u{1F600}");
        assert_eq!(v.str("l"), "a\\b\"c");
        assert_eq!(v.int("n"), -12);
        assert_eq!(v.int("h"), 0xffff);
        assert!(v.bool("b"));
        assert_eq!(v.strs("arr"), ["x", "y"]);
        assert_eq!(
            v.get("nested"),
            Some(&Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![])
            ]))
        );
        assert_eq!(f.of_kind("b", 1).len(), 1);
        assert_eq!(
            v.keys().collect::<Vec<_>>(),
            ["kind", "section", "s", "l", "n", "h", "b", "arr", "nested"]
        );
    }

    #[test]
    fn rejects_outside_the_subset() {
        for (text, line) in [
            ("[table]\n", 1),
            ("a = \"\"\"x\"\"\"\n", 1),
            ("a = 1.5\n", 1),
            ("a = 012\n", 1),
            ("\n\na = \"\\x41\"\n", 3),
            ("a = \"x\n", 1),
            ("a = 1 b = 2\n", 1),
            ("a = 1\na = 2\n", 2),
            ("a.b = 1\n", 1),
            ("a = [1 2]\n", 1),
            ("a = {}\n", 1),
            ("a = 1979-05-27\n", 1),
        ] {
            let e = parse(text).unwrap_err();
            assert_eq!(e.line, line, "{text:?}: {e}");
        }
    }

    #[test]
    fn hex() {
        assert_eq!(decode_hex("05 00\n ff").unwrap(), [5, 0, 0xff]);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("zz").is_err());
    }

    #[test]
    fn heading_paths() {
        let md =
            "# T\n## Test vectors\n### A\n```\n## Fake vector\n```\n### B [x]\n## Quirks\n### C\n";
        let h = headings(md);
        assert_eq!(h.len(), 6);
        assert!(section_exists(&h, "Test vectors"));
        assert!(section_exists(&h, "Test vectors > B [x]"));
        assert!(section_exists(&h, "T > Test vectors > A"));
        assert!(!section_exists(&h, "Test vectors > C"));
        assert!(!section_exists(&h, "Fake vector"));
        assert!(!section_exists(&h, "A > B [x]"));
        assert_eq!(
            h.iter()
                .filter(|h| is_vector_heading(h))
                .map(|h| h.text.as_str())
                .collect::<Vec<_>>(),
            ["Test vectors"]
        );
    }
}
