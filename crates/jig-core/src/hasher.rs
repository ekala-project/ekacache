//! The length-framed BLAKE3 hasher (`behaviors/jig-keys.md §The Hasher`).
//!
//! `Hasher` wraps one unkeyed BLAKE3 hasher. `field(d)` feeds `itoa(len(d)) ":" d`; `update(d)`
//! feeds `d` raw. `finish()` is non-consuming and returns the first 16 bytes of the standard
//! BLAKE3 hash, shown as 32 lowercase hex characters.

use std::fmt;
use std::io::Read;

/// A 128-bit BLAKE3 prefix. Only `Hasher` makes these.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; Digest::BYTES]);

impl Digest {
    /// A cache key, not a signature.
    pub const BYTES: usize = 16;

    /// 32 lowercase hex characters.
    #[must_use]
    pub fn to_hex(&self) -> [u8; 32] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 32];
        for (i, b) in self.0.iter().enumerate() {
            out[2 * i] = HEX[usize::from(b >> 4)];
            out[2 * i + 1] = HEX[usize::from(b & 15)];
        }
        out
    }

    /// The hex text as a `Vec<u8>`.
    #[must_use]
    pub fn hex(&self) -> Vec<u8> {
        self.to_hex().to_vec()
    }

    /// The raw 16 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Digest::BYTES] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = self.to_hex();
        // hex digits are ASCII
        f.write_str(std::str::from_utf8(&hex).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({self})")
    }
}

/// BLAKE3 over a sequence of fields.
#[derive(Clone, Default)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    #[must_use]
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }

    /// Raw bytes (`Update`).
    pub fn update(&mut self, d: &[u8]) -> &mut Self {
        self.0.update(d);
        self
    }

    /// `itoa(len) ":" d` (`Field`): no byte sequence inside a field can fake a boundary.
    pub fn field(&mut self, d: &[u8]) -> &mut Self {
        let mut buf = itoa_buf(d.len());
        buf.push(b':');
        self.0.update(&buf);
        self.0.update(d);
        self
    }

    /// The first 128 bits, without consuming the state (`Finish`).
    #[must_use]
    pub fn finish(&self) -> Digest {
        let full = self.0.finalize();
        let mut out = [0u8; Digest::BYTES];
        out.copy_from_slice(&full.as_bytes()[..Digest::BYTES]);
        Digest(out)
    }
}

fn itoa_buf(n: usize) -> Vec<u8> {
    n.to_string().into_bytes()
}

/// `HashOf(d)`: plain BLAKE3-128 with no framing.
#[must_use]
pub fn hash_of(d: &[u8]) -> Digest {
    Hasher::new().update(d).finish()
}

/// `HashOf` over a reader, streamed in 64 KiB chunks. Returns the digest and the number of
/// bytes hashed (callers that need an exact size, such as jigd's IDS, compare it).
///
/// # Errors
/// Any read error from `r`.
pub fn hash_reader(r: &mut impl Read) -> std::io::Result<(Digest, u64)> {
    let mut h = Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok((h.finish(), total)),
            Ok(n) => {
                h.update(&buf[..n]);
                total += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_is_not_consuming() {
        let mut h = Hasher::new();
        h.field(b"a");
        let first = h.finish();
        assert_eq!(first, h.finish());
        h.field(b"bc");
        assert_eq!(h.finish().to_string(), "a41516ff4f6e8e7b4f0534c112fc79e3");
    }

    #[test]
    fn streaming_matches() {
        let data = vec![7u8; 200_000];
        let (d, n) = hash_reader(&mut &data[..]).unwrap();
        assert_eq!(n, 200_000);
        assert_eq!(d, hash_of(&data));
    }

    proptest::proptest! {
        #[test]
        fn field_framing_is_injective(ab in proptest::collection::vec(proptest::num::u8::ANY, 0..40), i in 0usize..40, j in 0usize..40) {
            let i = i.min(ab.len());
            let j = j.min(ab.len());
            proptest::prop_assume!(i != j);
            let x = Hasher::new().field(&ab[..i]).field(&ab[i..]).finish();
            let y = Hasher::new().field(&ab[..j]).field(&ab[j..]).finish();
            proptest::prop_assert_ne!(x, y);
        }
    }
}
