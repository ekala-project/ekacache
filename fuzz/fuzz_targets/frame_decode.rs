//! `frame::decode`: no panic, and no allocation beyond what the input can decompress to (the
//! fuzzer's -malloc_limit_mb catches a header that reserves gigabytes). A decoded value
//! re-encodes to a frame that decodes to the same bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|wire: &[u8]| {
    if let Some(raw) = jig_core::frame::decode(wire)
        && let Ok(again) = jig_core::frame::encode(&raw)
    {
        assert_eq!(jig_core::frame::decode(&again).as_deref(), Some(&raw[..]));
    }
});
