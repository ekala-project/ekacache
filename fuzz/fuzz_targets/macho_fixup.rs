//! reloc-fixup on mutated Mach-O images (64-bit, little-endian `MH_MAGIC_64`).
#![no_main]

#[path = "fixup.rs"]
mod fixup;

libfuzzer_sys::fuzz_target!(|data: &[u8]| fixup::check(b"\xcf\xfa\xed\xfe", data));
