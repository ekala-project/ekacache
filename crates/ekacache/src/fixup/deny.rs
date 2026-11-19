//! A4: the deny check (`behaviors/jig-reloc-fixup.md §A4`; `fixup_mode.cc` `CheckDenied`).

use ekacache_core::bytes::{concat, find};

use super::Fixup;

/// Bytes of a hit shown in the message (`kLeakContext`).
const SHOWN: usize = 100;

/// The C locale's `iscntrl`: 0x00–0x1F and 0x7F; bytes ≥ 0x80 are not control bytes.
fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

/// The text shown for a hit at `pos`: up to 100 bytes, cut before the first control byte.
fn shown(data: &[u8], pos: usize) -> &[u8] {
    let window = data.get(pos..).unwrap_or_default();
    let window = &window[..window.len().min(SHOWN)];
    let cut = window
        .iter()
        .position(|&c| is_cntrl(c))
        .unwrap_or(window.len());
    &window[..cut]
}

/// For each denied needle, in order, its first occurrence in `data` is one error. An empty
/// needle matches at offset 0 of every file (Q-FIX-3).
pub fn check(fx: &mut Fixup, rel: &[u8], data: &[u8]) {
    for i in 0..fx.denied.len() {
        let Some(pos) = find(data, &fx.denied[i]) else {
            continue;
        };
        let line = concat(&[
            b"reloc-fixup: ",
            rel,
            b" refers to build-platform ",
            shown(data, pos),
        ]);
        fx.error(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_stops_at_control_bytes() {
        assert_eq!(shown(b"abc\ndef", 0), b"abc");
        assert_eq!(shown(b"\x01xyz", 0), b"");
        assert_eq!(shown(b"a\x7fb", 0), b"a");
        assert_eq!(shown(b"a\x80b", 0), b"a\x80b");
        let long = [b'x'; 150];
        assert_eq!(shown(&long, 10).len(), 100);
        assert_eq!(shown(b"", 0), b"");
    }
}
