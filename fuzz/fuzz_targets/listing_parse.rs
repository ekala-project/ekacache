//! `cache get-dir`'s listing parser: no panic; never more entries than lines.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &[u8]| {
    let entries = ekacache::cache_cmd::parse_listing(text);
    assert!(entries.len() <= text.iter().filter(|&&c| c == b'\n').count() + 1);
});
