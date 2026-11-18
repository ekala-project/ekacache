//! The nlohmann-compatible JSON reader behind `etc/jig.json` and `$PKGS_CC`, and the driver conf
//! on top of it: no panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &[u8]| {
    let _ = jig::driver::json::parse(text);
    let _ = jig::driver::parse_driver_conf(text, b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-cc");
});
