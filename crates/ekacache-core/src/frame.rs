//! Value framing (`behaviors/ec-cache-client.md §Value encoding`).
//!
//! Every value ec sends with `PUT` and receives with `GET` is a 4-byte raw length followed by
//! zstd data. ecd stores the bytes opaquely and never calls into this module.
//!
//! ADOPTED Q-CLIENT-1: the length header is little-endian on every host (`u32::to_le_bytes`),
//! byte-identical to the C++ client on all supported hosts.

use zstd::zstd_safe;

/// The header size.
pub const HEADER: usize = 4;

/// `ZSTD_compress` reported an error; the caller sends nothing (legacy returns an empty value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameError;

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("zstd compression failed")
    }
}

impl std::error::Error for FrameError {}

/// `Compress` (`cache_client.cc:105-125`): `u32 LE (len mod 2^32)` + one `ZSTD_compress(level 1)`
/// frame (content size in the header, no checksum, no dictionary).
pub fn encode(raw: &[u8]) -> Result<Vec<u8>, FrameError> {
    // len mod 2^32, as the C++ static_cast<uint32_t> does (Q-CLIENT-8)
    let header = (raw.len() as u64 & u64::from(u32::MAX)) as u32;
    let mut out = Vec::with_capacity(HEADER + zstd_safe::compress_bound(raw.len()));
    out.extend_from_slice(&header.to_le_bytes());
    let mut body = Vec::with_capacity(zstd_safe::compress_bound(raw.len()));
    zstd_safe::compress(&mut body, raw, 1).map_err(|_| FrameError)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// The sum of the content sizes of the frames in `src`: `Ok(None)` when a frame does not say,
/// `Err` when `src` is not a sequence of whole frames (which `ZSTD_decompress` rejects too).
fn content_size(mut src: &[u8]) -> Result<Option<u64>, ()> {
    let mut total: u64 = 0;
    while !src.is_empty() {
        let n = zstd_safe::find_frame_compressed_size(src).map_err(|_| ())?;
        if n == 0 || n > src.len() {
            return Err(());
        }
        match zstd_safe::get_frame_content_size(&src[..n]) {
            Ok(Some(size)) => total = total.checked_add(size).ok_or(())?,
            Ok(None) => return Ok(None),
            Err(_) => return Err(()),
        }
        src = &src[n..];
    }
    Ok(Some(total))
}

/// `Decompress` (`cache_client.cc:127-139`): fewer than 4 bytes → `None`; otherwise the header's
/// size, and the zstd data (any number of concatenated frames) must decompress to exactly that
/// many bytes.
///
/// Allocation guard (Q-CLIENT-7 stays observable-as-is): when every frame states its content
/// size, a total different from the header is refused before anything is allocated. Legacy
/// fails on the same input after allocating. Frames without a content size are decoded as a
/// stream that stops one byte past the header's size.
#[must_use]
pub fn decode(wire: &[u8]) -> Option<Vec<u8>> {
    let header: [u8; HEADER] = wire.get(..HEADER)?.try_into().ok()?;
    let size = u32::from_le_bytes(header) as usize;
    let src = &wire[HEADER..];
    match content_size(src) {
        Err(()) => None,
        Ok(Some(total)) => {
            if total != size as u64 {
                return None;
            }
            // exactly ZSTD_decompress(dst, size, src), the call legacy makes
            let mut out = Vec::with_capacity(size);
            match zstd_safe::decompress(&mut out, src) {
                Ok(n) if n == size && out.len() == size => Some(out),
                _ => None,
            }
        }
        Ok(None) => decode_stream(src, size),
    }
}

fn decode_stream(src: &[u8], size: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let decoder = zstd::stream::read::Decoder::with_buffer(src).ok()?;
    let mut out = Vec::new();
    decoder.take(size as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() == size).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_mismatched() {
        assert_eq!(decode(b""), None);
        assert_eq!(decode(&[5, 0, 0]), None);
        // an empty body decodes to the empty value when the header says 0
        assert_eq!(decode(&[0, 0, 0, 0]).as_deref(), Some(&b""[..]));
        assert_eq!(decode(&[1, 0, 0, 0]), None);
        let mut wire = encode(b"hello").unwrap();
        wire[0] = 6;
        assert_eq!(decode(&wire), None);
        wire[0] = 4;
        assert_eq!(decode(&wire), None);
        // truncated body
        let wire = encode(b"hello world hello world").unwrap();
        assert_eq!(decode(&wire[..wire.len() - 1]), None);
        // trailing garbage
        let mut wire = encode(b"hello").unwrap();
        wire.extend_from_slice(b"xyzzy!");
        assert_eq!(decode(&wire), None);
        // 1..4 trailing bytes
        let mut wire = encode(b"hello").unwrap();
        wire.push(0);
        assert_eq!(decode(&wire), None);
    }

    #[test]
    fn concatenated_frames() {
        let a = zstd::bulk::compress(b"hello ", 1).unwrap();
        let b = zstd::bulk::compress(b"world", 3).unwrap();
        let mut wire = 11u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&a);
        wire.extend_from_slice(&b);
        assert_eq!(decode(&wire).as_deref(), Some(&b"hello world"[..]));
        wire[0] = 10;
        assert_eq!(decode(&wire), None);
    }

    #[test]
    fn frames_without_content_size() {
        use std::io::Write;
        let mut streamed = Vec::new();
        {
            let mut e = zstd::stream::write::Encoder::new(&mut streamed, 1).unwrap();
            e.write_all(b"hello ").unwrap();
            e.finish().unwrap();
        }
        assert!(matches!(
            zstd_safe::get_frame_content_size(&streamed),
            Ok(None)
        ));
        let sized = zstd::bulk::compress(b"world", 1).unwrap();
        for (body, raw) in [
            ([&streamed[..]].concat(), &b"hello "[..]),
            ([&streamed[..], &sized[..]].concat(), &b"hello world"[..]),
            ([&sized[..], &streamed[..]].concat(), &b"worldhello "[..]),
        ] {
            let mut wire = (raw.len() as u32).to_le_bytes().to_vec();
            wire.extend_from_slice(&body);
            assert_eq!(decode(&wire).as_deref(), Some(raw));
            wire[0] -= 1;
            assert_eq!(decode(&wire), None);
            wire[0] += 2;
            assert_eq!(decode(&wire), None);
            wire[0] -= 1;
            assert_eq!(decode(&wire[..wire.len() - 1]), None);
            wire.extend_from_slice(b"junk!");
            assert_eq!(decode(&wire), None);
        }
    }

    #[test]
    fn skippable_frame_is_skipped() {
        // magic 0x184D2A50, 4-byte length, payload
        let mut wire = 5u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&0x184D_2A50u32.to_le_bytes());
        wire.extend_from_slice(&3u32.to_le_bytes());
        wire.extend_from_slice(b"abc");
        wire.extend_from_slice(&zstd::bulk::compress(b"hello", 1).unwrap());
        assert_eq!(decode(&wire).as_deref(), Some(&b"hello"[..]));
    }

    proptest::proptest! {
        #[test]
        fn round_trip(raw in proptest::collection::vec(proptest::num::u8::ANY, 0..5000)) {
            let wire = encode(&raw).unwrap();
            proptest::prop_assert_eq!(&wire[..4], &(raw.len() as u32).to_le_bytes()[..]);
            proptest::prop_assert_eq!(decode(&wire), Some(raw));
        }

        #[test]
        fn garbage_never_panics(wire in proptest::collection::vec(proptest::num::u8::ANY, 0..64)) {
            let _ = decode(&wire);
        }
    }
}
