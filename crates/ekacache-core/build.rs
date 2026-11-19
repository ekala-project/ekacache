//! `JIG_STORE_DIR`: the Nix store this jig serves, fixed at build time like legacy's
//! `meson -Dstore_dir` (implementation/01-workspace.md §Build configuration). Default
//! `/nix/store`. Must be absolute, without a trailing `/`, and not `/`.

use std::fmt::Write as _;

fn main() {
    println!("cargo:rerun-if-env-changed=JIG_STORE_DIR");
    let dir = std::env::var_os("JIG_STORE_DIR").unwrap_or_else(|| "/nix/store".into());
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(dir.as_os_str()).to_vec();
    assert!(
        bytes.first() == Some(&b'/') && bytes.len() > 1 && bytes.last() != Some(&b'/'),
        "JIG_STORE_DIR must be absolute, not `/`, and have no trailing `/` (got {})",
        dir.display()
    );
    let mut lit = String::new();
    for b in &bytes {
        write!(lit, "\\x{b:02x}").expect("String write");
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("store_dir.rs");
    std::fs::write(
        out,
        format!("/// The store directory (`JIG_STORE_DIR` at build time).\npub const STORE_DIR: &[u8] = b\"{lit}\";\n"),
    )
    .expect("write OUT_DIR/store_dir.rs");
}
