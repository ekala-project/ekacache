//! reloc-fixup on mutated ELF images (64-bit little-endian, the only class it rewrites).
#![no_main]

#[path = "fixup.rs"]
mod fixup;

libfuzzer_sys::fuzz_target!(|data: &[u8]| fixup::check(b"\x7fELF\x02\x01", data));
