//! Record and hint-entry layouts (`ecd-store.md` §Pack file format, §Hint file format).
//!
//! A record is `u32 LE keylen, u32 LE vallen, key, value`; a hint entry is `u32 LE keylen,
//! u32 LE vallen, u64 LE offset of the record, key`. No headers, checksums or padding.

/// Bytes of record header.
pub const REC_HEADER: usize = 8;
/// Bytes of hint entry header.
pub const HINT_HEADER: usize = 16;

/// The header and key of a record (the value follows).
pub(crate) fn record_head(key: &[u8], vlen: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(REC_HEADER + key.len());
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(&vlen.to_le_bytes());
    out.extend_from_slice(key);
    out
}

/// Append one hint entry.
pub(crate) fn push_hint_entry(out: &mut Vec<u8>, key: &[u8], vlen: u32, rec_off: u64) {
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(&vlen.to_le_bytes());
    out.extend_from_slice(&rec_off.to_le_bytes());
    out.extend_from_slice(key);
}

/// `(keylen, vallen)` of a record header.
pub(crate) fn parse_record_header(h: [u8; REC_HEADER]) -> (u32, u32) {
    (
        u32::from_le_bytes([h[0], h[1], h[2], h[3]]),
        u32::from_le_bytes([h[4], h[5], h[6], h[7]]),
    )
}

/// `(keylen, vallen, record offset)` of a hint entry header.
pub(crate) fn parse_hint_header(h: &[u8; HINT_HEADER]) -> (u32, u32, u64) {
    let (klen, vlen) = parse_record_header([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]);
    let off = u64::from_le_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
    (klen, vlen, off)
}

#[cfg(test)]
mod tests {
    use super::*;

    // the worked examples in ecd-store.md
    #[test]
    fn examples_from_the_spec() {
        let mut rec = record_head(b"k", 3);
        rec.extend_from_slice(b"abc");
        assert_eq!(rec, [0x01, 0, 0, 0, 0x03, 0, 0, 0, 0x6B, 0x61, 0x62, 0x63]);
        let mut hint = Vec::new();
        push_hint_entry(&mut hint, b"k", 3, 0);
        assert_eq!(
            hint,
            [0x01, 0, 0, 0, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x6B]
        );
        let h: [u8; HINT_HEADER] = hint[..HINT_HEADER].try_into().unwrap();
        assert_eq!(parse_hint_header(&h), (1, 3, 0));
    }
}
