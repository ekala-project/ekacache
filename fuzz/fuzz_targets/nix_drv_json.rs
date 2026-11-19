//! `jig nix-store add-drv` offline on arbitrary derivation JSON: no panic in
//! jig, and with JIG_LEGACY_JIG set, the same stdout, stderr and exit status as
//! the legacy jig (built with P2718, see default.nix; a GCC 14 build crashes here, Q-NIX-1).
#![no_main]

#[path = "common.rs"]
mod common;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|json: &[u8]| {
    // Not called in-process: most type errors abort by design (Q-NIX-4), as in legacy, so
    // `derivation_to_aterm` runs only inside the jig process.
    let (rust, legacy) = common::binaries();
    let env = [("JIG_NIX_STORE_OFFLINE", "1")];
    let args: [&[u8]; 2] = [b"add-drv", b"fuzz.drv"];
    let ours = common::run(&rust, "jig", &[b"nix-store", args[0], args[1]], &env, json);
    common::assert_no_panic(&ours);
    if let Some(legacy) = legacy {
        let theirs = common::run(
            &legacy,
            "jig",
            &[b"nix-store", args[0], args[1]],
            &env,
            json,
        );
        assert_eq!(ours, theirs, "legacy differs");
    }
});
