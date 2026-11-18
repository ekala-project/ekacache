//! `manifest::parse_depfile` (cc and rustc depfiles): no panic, no empty path.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &[u8]| {
    let paths = jig_core::manifest::parse_depfile(text);
    assert!(paths.iter().all(|p| !p.is_empty()));
});
