//! Shared by `elf_fixup` and `macho_fixup`: one file under a prefix, run through
//! `jig reloc-fixup <prefix> --dest <store path>` at the same path for both implementations.
//! No panic; with JIG_LEGACY_JIG set, the same stdout, stderr, exit status and resulting bytes,
//! except for libstdc++'s `terminate` text when both abort.

use std::path::Path;

#[path = "common.rs"]
mod common;

const DEST: &[u8] = b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo";

/// Run one implementation over a fresh tree holding `image` as `bin/a`: its outcome and the
/// file's bytes afterwards.
fn once(bin: &Path, root: &Path, image: &[u8]) -> (common::Outcome, Option<Vec<u8>>) {
    let prefix = root.join("p");
    let _ = std::fs::remove_dir_all(&prefix);
    std::fs::create_dir_all(prefix.join("bin")).expect("tree");
    let file = prefix.join("bin/a");
    std::fs::write(&file, image).expect("image");
    let p = prefix.as_os_str().as_encoded_bytes();
    let o = common::run(bin, "jig", &[b"reloc-fixup", p, b"--dest", DEST], &[], b"");
    (o, std::fs::read(&file).ok())
}

const fn libc_sigabrt() -> i32 {
    6
}

/// Legacy aborts through `std::terminate`, and libstdc++ prints what it threw. The port aborts
/// with no text (STATUS.md decision 11, Q-FIX-10), so that tail is the one allowed difference.
fn strip_terminate(stderr: &mut Vec<u8>) {
    let marker = b"terminate called after throwing";
    if let Some(at) = stderr.windows(marker.len()).position(|w| w == marker) {
        stderr.truncate(at);
    }
}

pub fn check(magic: &[u8], data: &[u8]) {
    let mut image = magic.to_vec();
    image.extend_from_slice(data);
    let (rust, legacy) = common::binaries();
    // a fixed path per fuzzing process: paths are part of the output
    let root = std::env::temp_dir().join(format!("jig-fuzz-fixup-{}", std::process::id()));
    let ours = once(&rust, &root, &image);
    common::assert_no_panic(&ours.0);
    if let Some(legacy) = legacy {
        let mut theirs = once(&legacy, &root, &image);
        if theirs.0.signal == Some(libc_sigabrt()) {
            strip_terminate(&mut theirs.0.stderr);
        }
        assert_eq!(ours, theirs, "legacy differs");
    }
}
