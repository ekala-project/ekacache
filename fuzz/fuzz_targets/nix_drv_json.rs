//! `ec nix-store add-drv` offline on arbitrary derivation JSON: no panic in ec.
#![no_main]

#[path = "common.rs"]
mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|json: &[u8]| {
    // Not called in-process: most type errors abort by design (Q-NIX-4), so
    // `derivation_to_aterm` runs only inside the ec process.
    let rust = common::binary();
    let env = [("EKACACHE_NIX_STORE_OFFLINE", "1")];
    let ours = common::run(
        &rust,
        "ec",
        &[b"nix-store", b"add-drv", b"fuzz.drv"],
        &env,
        json,
    );
    common::assert_no_panic(&ours);
});
