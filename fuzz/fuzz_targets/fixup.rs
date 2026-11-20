//! Shared by `elf_fixup` and `macho_fixup`: one file under a prefix, run through
//! `ec reloc-fixup <prefix> --dest <store path>`. No panic.

use std::path::Path;

#[path = "common.rs"]
mod common;

const DEST: &[u8] = b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo";

/// Run ec over a fresh tree holding `image` as `bin/a`.
fn once(bin: &Path, root: &Path, image: &[u8]) -> common::Outcome {
    let prefix = root.join("p");
    let _ = std::fs::remove_dir_all(&prefix);
    std::fs::create_dir_all(prefix.join("bin")).expect("tree");
    let file = prefix.join("bin/a");
    std::fs::write(&file, image).expect("image");
    let p = prefix.as_os_str().as_encoded_bytes();
    common::run(bin, "ec", &[b"reloc-fixup", p, b"--dest", DEST], &[], b"")
}

pub fn check(magic: &[u8], data: &[u8]) {
    let mut image = magic.to_vec();
    image.extend_from_slice(data);
    let rust = common::binary();
    // a fixed path per fuzzing process
    let root = std::env::temp_dir().join(format!("ekacache-fuzz-fixup-{}", std::process::id()));
    common::assert_no_panic(&once(&rust, &root, &image));
}
