//! The ecd line protocol, shared by client and server (`behaviors/ecd-protocol.md §Framing`,
//! `§Tokenization`, `§Dispatch`, `§Number parsing`, `§HELLO`).

/// Content bytes in one request line; the 65 536th byte without a `\n` is a hang-up.
pub const MAX_LINE: usize = 65_535;

/// The highest protocol version this implementation speaks (`§HELLO`). Version 0 is the
/// protocol without HELLO; version 1 adds HELLO and nothing else.
pub const PROTOCOL_MAX: u32 = 1;

/// `IDS <n>` accepts `0 ..= 1<<20`.
pub const MAX_IDS: usize = 1 << 20;

/// Length of the white-space sequence starting at `s[0]`, or 0.
///
/// Go `strings.Fields` splits on `unicode.IsSpace` while decoding UTF-8 one rune at a time; an
/// invalid byte decodes as `RuneError` (width 1, not a space). Matching the exact encodings at
/// every byte offset gives the same split: every multi-byte separator starts with a lead byte
/// (`C2`, `E1`, `E2`, `E3`), which can never be a continuation byte inside another valid rune,
/// and Go never consumes an invalid sequence more than one byte at a time (Q-PROTO-3).
#[must_use]
fn space_len(s: &[u8]) -> usize {
    match s {
        [b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ', ..] => 1,
        // U+0085 NEL, U+00A0 NBSP
        [0xc2, 0x85 | 0xa0, ..] => 2,
        // U+1680 OGHAM SPACE MARK; U+2000..=U+200A, U+2028, U+2029, U+202F;
        // U+205F MEDIUM MATHEMATICAL SPACE; U+3000 IDEOGRAPHIC SPACE
        [0xe1, 0x9a, 0x80, ..]
        | [0xe2, 0x80, 0x80..=0x8a | 0xa8 | 0xa9 | 0xaf, ..]
        | [0xe2, 0x81, 0x9f, ..]
        | [0xe3, 0x80, 0x80, ..] => 3,
        _ => 0,
    }
}

/// An iterator over the fields of a line, as Go's `strings.Fields`.
#[derive(Clone, Debug)]
pub struct FieldIter<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for FieldIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            if self.rest.is_empty() {
                return None;
            }
            let n = space_len(self.rest);
            if n == 0 {
                break;
            }
            self.rest = &self.rest[n..];
        }
        let mut i = 0;
        while i < self.rest.len() && space_len(&self.rest[i..]) == 0 {
            i += 1;
        }
        let (field, rest) = self.rest.split_at(i);
        self.rest = rest;
        Some(field)
    }
}

/// Every field of `line` (Go `strings.Fields` on raw bytes).
#[must_use]
pub fn field_iter(line: &[u8]) -> FieldIter<'_> {
    FieldIter { rest: line }
}

/// The first four fields of a line and the total count; dispatch never needs more.
#[derive(Clone, Copy, Debug)]
pub struct Fields<'a> {
    first: [&'a [u8]; 4],
    count: usize,
}

impl<'a> Fields<'a> {
    /// The number of fields in the whole line.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Field `i`, for `i < 4`.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&'a [u8]> {
        if i < self.count && i < self.first.len() {
            Some(self.first[i])
        } else {
            None
        }
    }
}

/// `strings.Fields(line)`, keeping the first four fields and counting the rest. No allocation.
#[must_use]
pub fn fields(line: &[u8]) -> Fields<'_> {
    let mut f = Fields {
        first: [&[]; 4],
        count: 0,
    };
    for field in field_iter(line) {
        if f.count < f.first.len() {
            f.first[f.count] = field;
        }
        f.count += 1;
    }
    f
}

/// Go `strconv.ParseInt(s, 10, 64)`: an optional `+` or `-`, then one or more ASCII digits,
/// leading zeros allowed, the value within `i64` (Q-PROTO-4).
#[must_use]
pub fn parse_int64(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add(u64::from(c - b'0'))?;
    }
    if neg {
        if n > 1 << 63 {
            return None;
        }
        Some(0i64.wrapping_sub_unsigned(n))
    } else {
        i64::try_from(n).ok()
    }
}

/// `strconv.Atoi(s)` and the range check `0 <= n <= 1<<20` of `IDS <n>`.
#[must_use]
pub fn parse_ids_count(s: &[u8]) -> Option<usize> {
    let n = parse_int64(s)?;
    usize::try_from(n).ok().filter(|&n| n <= MAX_IDS)
}

/// `HELLO <maxver>`: the number rules of `§Number parsing`, in `1 ..= 2147483647`.
#[must_use]
pub fn parse_hello_ver(s: &[u8]) -> Option<u32> {
    let n = parse_int64(s)?;
    if (1..=i64::from(i32::MAX)).contains(&n) {
        u32::try_from(n).ok()
    } else {
        None
    }
}

/// A request line, after dispatch on verb **and** field count (`§Dispatch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    /// `HELLO <maxver>` (adopted, Rust only). Valid only as the first request; the server
    /// decides that.
    Hello(u32),
    Get(&'a [u8]),
    Has(&'a [u8]),
    /// `PUT <key> <len>`: any `i64` parses; the store rejects a negative length.
    Put(&'a [u8], i64),
    Ids(usize),
    Slot(&'a [u8]),
    Done(&'a [u8]),
    Stats,
}

/// `None` means hang up: an empty line, an unknown verb, a known verb with the wrong number of
/// fields, or a number that does not parse.
#[must_use]
pub fn parse_request(line: &[u8]) -> Option<Request<'_>> {
    let f = fields(line);
    let verb = f.get(0)?;
    match (f.len(), verb) {
        (2, b"HELLO") => parse_hello_ver(f.get(1)?).map(Request::Hello),
        (2, b"GET") => Some(Request::Get(f.get(1)?)),
        (2, b"HAS") => Some(Request::Has(f.get(1)?)),
        (3, b"PUT") => Some(Request::Put(f.get(1)?, parse_int64(f.get(2)?)?)),
        (2, b"IDS") => parse_ids_count(f.get(1)?).map(Request::Ids),
        (2, b"SLOT") => Some(Request::Slot(f.get(1)?)),
        (2, b"DONE") => Some(Request::Done(f.get(1)?)),
        (1, b"STATS") => Some(Request::Stats),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(line: &[u8]) -> Vec<&[u8]> {
        field_iter(line).collect()
    }

    #[test]
    fn tokenization_examples() {
        assert_eq!(all(b"GET k"), vec![&b"GET"[..], b"k"]);
        assert_eq!(all(b"GET\x85k"), vec![&b"GET\x85k"[..]]);
        assert_eq!(all("GET\u{200b}k".as_bytes()).len(), 1);
        assert_eq!(all("GET\u{3000}k".as_bytes()), vec![&b"GET"[..], b"k"]);
        assert_eq!(all("GET\u{a0}k\u{85}".as_bytes()), vec![&b"GET"[..], b"k"]);
        assert_eq!(all(b"  \t\r\n\x0b\x0c "), Vec::<&[u8]>::new());
        assert_eq!(all(b"PUT k 3\r"), vec![&b"PUT"[..], b"k", b"3"]);
        assert_eq!(all(b"a\x1cb\x00c"), vec![&b"a\x1cb\x00c"[..]]);
        assert_eq!(all(b"\xe2\x80 \x80"), vec![&b"\xe2\x80"[..], b"\x80"]);
        let f = fields(b"a b c d e f");
        assert_eq!(f.len(), 6);
        assert_eq!(f.get(3), Some(&b"d"[..]));
        assert_eq!(f.get(4), None);
    }

    #[test]
    fn number_parsing() {
        for (s, v) in [
            ("0", Some(0)),
            ("+3", Some(3)),
            ("003", Some(3)),
            ("-0", Some(0)),
            ("-1", Some(-1)),
            ("9223372036854775807", Some(i64::MAX)),
            ("9223372036854775808", None),
            ("-9223372036854775808", Some(i64::MIN)),
            ("-9223372036854775809", None),
            ("", None),
            ("+", None),
            ("-", None),
            ("0x10", None),
            ("1_0", None),
            ("1 ", None),
            ("١", None),
            ("99999999999999999999999", None),
        ] {
            assert_eq!(parse_int64(s.as_bytes()), v, "{s:?}");
        }
        assert_eq!(parse_ids_count(b"1048576"), Some(1 << 20));
        assert_eq!(parse_ids_count(b"1048577"), None);
        assert_eq!(parse_ids_count(b"-0"), Some(0));
        assert_eq!(parse_ids_count(b"-1"), None);
        assert_eq!(parse_hello_ver(b"0"), None);
        assert_eq!(parse_hello_ver(b"+1"), Some(1));
        assert_eq!(parse_hello_ver(b"2147483647"), Some(i32::MAX as u32));
        assert_eq!(parse_hello_ver(b"2147483648"), None);
    }

    #[test]
    fn dispatch() {
        assert_eq!(parse_request(b"GET k"), Some(Request::Get(b"k")));
        assert_eq!(parse_request(b" HAS  k "), Some(Request::Has(b"k")));
        assert_eq!(parse_request(b"PUT k +3"), Some(Request::Put(b"k", 3)));
        assert_eq!(parse_request(b"PUT k -1"), Some(Request::Put(b"k", -1)));
        assert_eq!(parse_request(b"IDS 0"), Some(Request::Ids(0)));
        assert_eq!(
            parse_request(b"SLOT /build"),
            Some(Request::Slot(b"/build"))
        );
        assert_eq!(
            parse_request(b"DONE /build"),
            Some(Request::Done(b"/build"))
        );
        assert_eq!(parse_request(b"STATS"), Some(Request::Stats));
        assert_eq!(parse_request(b"HELLO 7"), Some(Request::Hello(7)));
        for bad in [
            &b""[..],
            b"   ",
            b"GET",
            b"GET a b",
            b"STATS x",
            b"get k",
            b"PUT k x",
            b"PUT k",
            b"IDS 1048577",
            b"IDS x",
            b"HELLO",
            b"HELLO 0",
            b"HELLO 1 2",
            b"NOPE",
        ] {
            assert_eq!(
                parse_request(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}
