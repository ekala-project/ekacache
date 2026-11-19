//! The JSON reader behind `etc/jig.json` and `$PKGS_CC` (`behaviors/jig-driver-policy.md
//! §jig.json schema`): the semantics of `nlohmann::json::parse(text, nullptr,
//! allow_exceptions=false)`, which both legacy readers use.
//!
//! The plan (`implementation/06-jig-cc.md §JSON parity with nlohmann`) proposed `serde_json`
//! behind a BOM strip. Probing the legacy binary showed three more differences that a shim
//! cannot cover, so this is a small hand-written parser instead:
//!
//! - nlohmann's lexer treats a NUL byte where a token starts as the end of input, so
//!   `{"cc":"/c"}\0anything` parses;
//! - nesting depth is unlimited (nlohmann's parser keeps an explicit stack; `serde_json` stops
//!   at 128);
//! - a number whose value overflows a double is an error, one that underflows is not
//!   (`serde_json` agrees, but this keeps the rule in one place).
//!
//! Everything else is RFC 8259: no comments, no trailing commas, nothing but white space
//! (space, tab, LF, CR) after the value, one leading UTF-8 BOM skipped, UTF-8 checked inside
//! strings, surrogate pairs combined, a lone surrogate escape rejected, raw control characters
//! in strings rejected, `\u0000` an embedded NUL. Numbers are checked but not kept: the readers
//! only ever ask whether a value is a string or an array of strings. With duplicate object keys
//! the last one wins (`get`).
//!
//! Parsing and dropping are iterative, so a deeply nested document cannot overflow the stack.

/// A parsed JSON value. Object members keep their order and their duplicates.
#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// A number; its value is not needed.
    Number,
    String(Vec<u8>),
    Array(Vec<Value>),
    Object(Vec<(Vec<u8>, Value)>),
}

impl Value {
    /// `find(key)` on an object: the last member named `key`. `None` for a non-object.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_string(&self) -> Option<&[u8]> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    /// `get<std::vector<std::string>>()`: `None` unless this is an array of strings only
    /// (nlohmann throws there, which aborts the process under `-fno-exceptions`).
    #[must_use]
    pub fn string_list(&self) -> Option<Vec<Vec<u8>>> {
        self.as_array()?
            .iter()
            .map(|v| v.as_string().map(<[u8]>::to_vec))
            .collect()
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        // flatten the children so the compiler's recursive drop only ever sees empty containers
        let mut stack = Vec::new();
        take_children(self, &mut stack);
        while let Some(mut v) = stack.pop() {
            take_children(&mut v, &mut stack);
        }
    }
}

fn take_children(v: &mut Value, into: &mut Vec<Value>) {
    match v {
        Value::Array(items) => into.append(items),
        Value::Object(members) => into.extend(members.drain(..).map(|(_, v)| v)),
        _ => {}
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Eof,
    BeginObject,
    EndObject,
    BeginArray,
    EndArray,
    Colon,
    Comma,
    /// A string: a value or an object key.
    Str(Vec<u8>),
    /// Any other scalar.
    Scalar(Value),
}

struct Lexer<'a> {
    text: &'a [u8],
    pos: usize,
}

impl Lexer<'_> {
    fn byte(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    /// The next token; `None` on a lexical error.
    fn token(&mut self) -> Option<Token> {
        while matches!(self.byte(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
        let Some(c) = self.byte() else {
            return Some(Token::Eof);
        };
        // nlohmann's lexer: "the null byte is needed when parsing from string literals"
        if c == 0 {
            return Some(Token::Eof);
        }
        self.pos += 1;
        Some(match c {
            b'{' => Token::BeginObject,
            b'}' => Token::EndObject,
            b'[' => Token::BeginArray,
            b']' => Token::EndArray,
            b':' => Token::Colon,
            b',' => Token::Comma,
            b'"' => Token::Str(self.string()?),
            b't' => self.literal(b"rue", Value::Bool(true))?,
            b'f' => self.literal(b"alse", Value::Bool(false))?,
            b'n' => self.literal(b"ull", Value::Null)?,
            b'-' | b'0'..=b'9' => {
                self.pos -= 1;
                self.number()?;
                Token::Scalar(Value::Number)
            }
            _ => return None,
        })
    }

    fn literal(&mut self, rest: &[u8], v: Value) -> Option<Token> {
        if self.text[self.pos..].starts_with(rest) {
            self.pos += rest.len();
            Some(Token::Scalar(v))
        } else {
            None
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.byte(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, and the value must be finite
    /// (nlohmann: "number overflow"). Integers that fit 64 bits are finite anyway.
    fn number(&mut self) -> Option<()> {
        let start = self.pos;
        if self.byte() == Some(b'-') {
            self.pos += 1;
        }
        match self.byte() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return None,
        }
        if self.byte() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return None;
            }
        }
        if matches!(self.byte(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.byte(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return None;
            }
        }
        // the token is ASCII; Rust's parse is correctly rounded like glibc's strtod
        let token = std::str::from_utf8(&self.text[start..self.pos]).ok()?;
        token
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|_| ())
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0;
        for _ in 0..4 {
            let d = char::from(self.byte()?).to_digit(16)?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Some(v)
    }

    /// The rest of a string after its opening quote.
    fn string(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let c = self.byte()?;
            self.pos += 1;
            match c {
                b'"' => return Some(out),
                b'\\' => {
                    let e = self.byte()?;
                    self.pos += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) {
                                if !self.text[self.pos..].starts_with(b"\\u") {
                                    return None;
                                }
                                self.pos += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return None;
                                }
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return None;
                            }
                            let ch = char::from_u32(cp)?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return None,
                    }
                }
                0x00..=0x1f => return None,
                0x20..=0x7f => out.push(c),
                _ => {
                    // one well-formed UTF-8 sequence (RFC 3629, as nlohmann's lexer checks it)
                    let len = match c {
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return None,
                    };
                    let start = self.pos - 1;
                    let seq = self.text.get(start..start + len)?;
                    std::str::from_utf8(seq).ok()?;
                    out.extend_from_slice(seq);
                    self.pos = start + len;
                }
            }
        }
    }
}

enum Frame {
    Array(Vec<Value>),
    /// The members so far and the key of the value being parsed.
    Object(Vec<(Vec<u8>, Value)>, Vec<u8>),
}

/// Parse `text`; `None` for anything nlohmann's non-throwing parse would discard.
#[must_use]
pub fn parse(text: &[u8]) -> Option<Value> {
    let mut lx = Lexer {
        text: text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text),
        pos: 0,
    };
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        // a value starts here
        let mut value = match lx.token()? {
            Token::Str(s) => Value::String(s),
            Token::Scalar(v) => v,
            Token::BeginArray => {
                let save = lx.pos;
                if lx.token()? == Token::EndArray {
                    Value::Array(Vec::new())
                } else {
                    lx.pos = save;
                    stack.push(Frame::Array(Vec::new()));
                    continue;
                }
            }
            Token::BeginObject => match lx.token()? {
                Token::EndObject => Value::Object(Vec::new()),
                Token::Str(key) => {
                    if lx.token()? != Token::Colon {
                        return None;
                    }
                    stack.push(Frame::Object(Vec::new(), key));
                    continue;
                }
                _ => return None,
            },
            _ => return None,
        };
        // the value is complete: hand it to its container, closing containers as they end
        loop {
            match stack.last_mut() {
                None => return (lx.token()? == Token::Eof).then_some(value),
                Some(Frame::Array(items)) => {
                    items.push(value);
                    match lx.token()? {
                        Token::Comma => break,
                        Token::EndArray => {
                            let Some(Frame::Array(items)) = stack.pop() else {
                                unreachable!("the last frame is an array")
                            };
                            value = Value::Array(items);
                        }
                        _ => return None,
                    }
                }
                Some(Frame::Object(members, key)) => {
                    members.push((std::mem::take(key), value));
                    match lx.token()? {
                        Token::Comma => {
                            let Token::Str(next) = lx.token()? else {
                                return None;
                            };
                            if lx.token()? != Token::Colon {
                                return None;
                            }
                            *key = next;
                            break;
                        }
                        Token::EndObject => {
                            let Some(Frame::Object(members, _)) = stack.pop() else {
                                unreachable!("the last frame is an object")
                            };
                            value = Value::Object(members);
                        }
                        _ => return None,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(t: &[u8]) -> bool {
        parse(t).is_some()
    }

    #[test]
    fn accepted_and_rejected_like_legacy() {
        // every row was checked against the C++ jig (`jig.json: not a JSON object` or not)
        for t in [
            &br#"{"cc":"/c"}"#[..],
            b"\xef\xbb\xbf{}",
            b"{} \t\r\n",
            b"{}\0garbage",
            br#"{"x":-1e-400}"#,
            br#"{"x":0e400}"#,
            br#"{"x":123456789012345678901234567890}"#,
            br#"{"x":1.7976931348623158e308}"#,
            br#"{"x":-0}"#,
            br#"{"x":1E+2}"#,
            b"{\"x\":\"\x7f\"}",
            br#"{"x":"\uD800\udc00"}"#,
            br#"{"x":"\u00E9"}"#,
            br#"{"x":[true,false,null,{"a":[]}]}"#,
        ] {
            assert!(ok(t), "{}", String::from_utf8_lossy(t));
        }
        for t in [
            &b""[..],
            b"[]",
            b"\0{}",
            br#"{"cc":"/c"} x"#,
            br#"{"cc":"/c", /*c*/ "x":1}"#,
            br#"{"cc":"/c",}"#,
            b"{\"cc\":\"\xff\"}",
            b"{\"x\":\"\xc0\x80\"}",
            b"{\"x\":\"\xed\xa0\x80\"}",
            b"{\"x\":\"a\0b\"}",
            b"{\"x\":\"a\tb\"}",
            b"\xef\xbb\xbf\xef\xbb\xbf{}",
            b" \xef\xbb\xbf{}",
            b"{}\x0b",
            b"{}\x0c",
            br#"{"x":1e400}"#,
            br#"{"x":-1e400}"#,
            br#"{"x":1.7976931348623159e308}"#,
            br#"{"x":01}"#,
            br#"{"x":1.}"#,
            br#"{"x":.5}"#,
            br#"{"x":+1}"#,
            br#"{"x":-}"#,
            br#"{"x":1e}"#,
            br#"{"x":TRUE}"#,
            br#"{"x":"\ud800"}"#,
            br#"{"x":"\uDC00"}"#,
            br#"{"x":"\x"}"#,
            br#"{"x":tru}"#,
            br#"{"x" 1}"#,
            br"{1:1}",
        ] {
            let ok = ok(t) && parse(t).is_some_and(|v| v.is_object());
            assert!(!ok, "{}", String::from_utf8_lossy(t));
        }
    }

    #[test]
    fn values() {
        let v = parse(br#"{"a":"1","a":"2","s":"x\u0000y\n","l":["p",1],"o":{"k":[]}}"#).unwrap();
        assert_eq!(v.get(b"a").and_then(Value::as_string), Some(&b"2"[..]));
        assert_eq!(v.get(b"s").and_then(Value::as_string), Some(&b"x\0y\n"[..]));
        assert_eq!(v.get(b"l").unwrap().string_list(), None);
        assert_eq!(
            v.get(b"o").unwrap().get(b"k").unwrap().string_list(),
            Some(vec![])
        );
        assert_eq!(v.get(b"missing"), None);
        assert_eq!(
            parse(br#"{"c\u0063":1}"#).unwrap().get(b"cc"),
            Some(&Value::Number)
        );
        assert_eq!(
            parse("{\"\u{e9}\":[\"\u{1f600}\"]}".as_bytes())
                .unwrap()
                .get("\u{e9}".as_bytes())
                .unwrap()
                .string_list(),
            Some(vec!["\u{1f600}".as_bytes().to_vec()])
        );
    }

    #[test]
    fn deep_nesting_is_fine() {
        let n = 200_000;
        let mut t = b"{\"x\":".to_vec();
        t.extend(std::iter::repeat_n(b'[', n));
        t.extend(std::iter::repeat_n(b']', n));
        t.push(b'}');
        assert!(ok(&t));
        let mut t = b"{\"x\":".to_vec();
        t.extend(std::iter::repeat_n(&b"{\"a\":"[..], n).flatten());
        t.push(b'1');
        t.extend(std::iter::repeat_n(b'}', n + 1));
        assert!(ok(&t));
        // and a truncated one fails without building anything that overflows on drop
        t.truncate(t.len() - 10);
        assert!(!ok(&t));
    }
}
