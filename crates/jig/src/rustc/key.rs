//! k1 of rustc mode (`behaviors/jig-rustc-mode.md §Request key (k1)`, `rustc_mode.cc:243-318`;
//! framing in `behaviors/jig-keys.md §The Hasher`).
//!
//! Q-RS-2 kept: no field passes through `MaskOut`.

use jig_core::env::Env;
use jig_core::fspath;
use jig_core::hasher::Hasher;
use jig_core::keys::{RequestKey, Tool};
use jig_core::store_path::Store;

use super::parse::RustInvocation;
use crate::client::Client;
use crate::twokey;

/// Field 1. ADOPTED Q-RS-3: the C++ jig writes `rs-schema=7`; entries made under it lack the
/// env-dep lines, so neither jig reads the other's.
pub const SCHEMA: &[u8] = b"rs-schema=8";

/// The variables hashed into k1 as `NAME=value` (empty when unset), in this order.
pub const KEY_ENV: [&str; 5] = [
    "CARGO_PKG_NAME",
    "CARGO_PKG_VERSION",
    "CARGO_CFG_TARGET_FEATURE",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
];

/// What k1 is computed from, besides the parsed invocation.
pub struct KeyInputs<'a> {
    /// `OnPath(rustc)`.
    pub tool_path: &'a [u8],
    /// `StdlibIds(…)` (`stdlib_ids`).
    pub stdlib: &'a [u8],
    /// `current_path()`.
    pub cwd: &'a [u8],
    /// jig's environment.
    pub env: &'a Env,
    /// The crate root's bytes.
    pub source: &'a [u8],
}

fn field(prefix: &[u8], value: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(prefix.len() + value.len());
    f.extend_from_slice(prefix);
    f.extend_from_slice(value);
    f
}

/// The k1 fields, in order (`rustc_mode.cc:297-318`), each one `Field`-framed by
/// `request_key`.
#[must_use]
pub fn key_fields(store: &Store, inv: &RustInvocation, k: &KeyInputs<'_>) -> Vec<Vec<u8>> {
    let mut fields = vec![
        SCHEMA.to_vec(),
        // cargo may hand us a bare `rustc`: resolved on PATH, or two toolchains share a key
        field(b"rustc=", &store.tool_id(k.tool_path)),
        field(b"stdlib=", k.stdlib),
        field(b"cwd=", &store.key(k.cwd)),
        // same bytes under vendor/foo and vendor/foo-1.0: file!(), DWARF and dep-info name it
        field(b"src=", &store.key(&inv.source)),
    ];
    fields.extend(inv.key_args.iter().map(|a| store.key(a)));
    for rlib in &inv.externs {
        let id = store.input_id(rlib).unwrap_or_else(|| b"?".to_vec());
        fields.push(field(b"extern:", &id));
    }
    for var in KEY_ENV {
        let mut f = var.as_bytes().to_vec();
        f.push(b'=');
        f.extend_from_slice(k.env.get(var).unwrap_or_default());
        fields.push(f);
    }
    fields.push(k.source.to_vec());
    fields
}

/// `RequestKey(Tool::kRustc, H(fields))`: `rs/<hex>`.
#[must_use]
pub fn request_key<F: AsRef<[u8]>>(fields: &[F]) -> RequestKey {
    let mut h = Hasher::new();
    for f in fields {
        h.field(f.as_ref());
    }
    RequestKey::new(Tool::Rustc, &h.finish())
}

/// The libstd/libcore rlibs `StdlibIds` identifies, sorted bytewise with duplicates kept; `None`
/// when `canonical(tool_path)` fails.
///
/// Roots are `canonical(tool_path)/../..` and, if given, `sysroot`; for each root, each entry
/// `T` of `root/lib/rustlib` and each entry of `T/lib` named `libstd-*.rlib` or
/// `libcore-*.rlib`, `weakly_canonical(entry)` (`""` on error: the cross sysroot is a directory
/// of symlinks, and the real file is hashed). A directory that cannot be read contributes
/// nothing.
#[must_use]
pub fn stdlib_libs(tool_path: &[u8], sysroot: &[u8]) -> Option<Vec<Vec<u8>>> {
    let real = std::fs::canonicalize(jig_core::bytes::os_path(tool_path)).ok()?;
    let real = real.into_os_string().into_encoded_bytes();
    let mut roots = vec![fspath::parent_path(&fspath::parent_path(&real))];
    if !sysroot.is_empty() {
        roots.push(sysroot.to_vec());
    }
    let mut libs = Vec::new();
    for root in &roots {
        let rustlib = fspath::join(&fspath::join(root, b"lib"), b"rustlib");
        for triple in read_dir_names(&rustlib) {
            let lib_dir = fspath::join(&fspath::join(&rustlib, &triple), b"lib");
            for name in read_dir_names(&lib_dir) {
                if (name.starts_with(b"libstd-") || name.starts_with(b"libcore-"))
                    && name.ends_with(b".rlib")
                {
                    let entry = fspath::join(&lib_dir, &name);
                    libs.push(fspath::weakly_canonical(&entry).unwrap_or_default());
                }
            }
        }
    }
    libs.sort();
    Some(libs)
}

/// The entry names of a directory (`directory_iterator` with an `error_code`: nothing when it
/// cannot be opened; `.` and `..` are never listed).
fn read_dir_names(dir: &[u8]) -> Vec<Vec<u8>> {
    let Ok(rd) = std::fs::read_dir(jig_core::bytes::os_path(dir)) else {
        return Vec::new();
    };
    rd.map_while(Result::ok)
        .map(|e| e.file_name().into_encoded_bytes())
        .collect()
}

/// `StdlibIds(cache, rustc, sysroot)` (`rustc_mode.cc:243-273`): `?` when `rustc` cannot be
/// canonicalised; else one `PrefetchIdentities` of `stdlib_libs` and `(InputId or "?") + ","`
/// for each.
///
/// Store hashes are masked in keys, so two builds of the same rust release would otherwise
/// share a key, yet an rlib compiled against one libstd is "can't find crate" (E0463) against
/// the other.
pub fn stdlib_ids(client: &mut Client, store: &Store, tool_path: &[u8], sysroot: &[u8]) -> Vec<u8> {
    let Some(libs) = stdlib_libs(tool_path, sysroot) else {
        return b"?".to_vec();
    };
    twokey::prefetch_identities(client, store, &libs);
    let mut ids = Vec::new();
    for lib in &libs {
        ids.extend_from_slice(&store.input_id(lib).unwrap_or_else(|| b"?".to_vec()));
        ids.push(b',');
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rustc::parse::parse_rust_invocation;
    use jig_core::hasher::Digest;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn b(p: &Path) -> Vec<u8> {
        p.as_os_str().as_encoded_bytes().to_vec()
    }

    fn s(v: &[u8]) -> String {
        String::from_utf8(v.to_vec()).unwrap()
    }

    #[test]
    fn field_order() {
        let dir = tempfile::tempdir().unwrap();
        let rlib = dir.path().join("libbar-1.rlib");
        std::fs::write(&rlib, "bar").unwrap();
        let ext = format!("--extern=bar={}", rlib.display());
        let inv = parse_rust_invocation(&[
            "--crate-name",
            "foo",
            "src/lib.rs",
            "--crate-type",
            "lib",
            "-C",
            "metadata=abcd",
            "--emit=dep-info,link",
            "--out-dir",
            "/b",
            &ext,
            "--extern",
            "gone=/nonexistent/libgone.rlib",
            "-Copt-level=3",
        ]);
        let store = Store::from_env(&Env::default());
        let env = Env::from_pairs(&[("CARGO_PKG_NAME", "foo"), ("RUSTFLAGS", "")]);
        let f = key_fields(
            &store,
            &inv,
            &KeyInputs {
                tool_path: b"/no/such/rustc",
                stdlib: b"?",
                cwd: b"/w/./x/",
                env: &env,
                source: b"pub fn f() {}\n",
            },
        );
        let f: Vec<String> = f.iter().map(|x| s(x)).collect();
        let rid = s(&store.input_id(&b(&rlib)).unwrap());
        let want = vec![
            "rs-schema=8".to_owned(),
            "rustc=/no/such/rustc".to_owned(),
            "stdlib=?".to_owned(),
            "cwd=/w/x".to_owned(),
            "src=src/lib.rs".to_owned(),
            "--crate-name=foo".to_owned(),
            "--crate-type=lib".to_owned(),
            "--emit=dep-info,link".to_owned(),
            ext.clone(),
            "--extern=gone=/nonexistent/libgone.rlib".to_owned(),
            "-C=opt-level=3".to_owned(),
            format!("extern:{rid}"),
            "extern:?".to_owned(),
            "CARGO_PKG_NAME=foo".to_owned(),
            "CARGO_PKG_VERSION=".to_owned(),
            "CARGO_CFG_TARGET_FEATURE=".to_owned(),
            "RUSTFLAGS=".to_owned(),
            "CARGO_ENCODED_RUSTFLAGS=".to_owned(),
            "pub fn f() {}\n".to_owned(),
        ];
        assert_eq!(f, want);
        // k1 = rs/ + H(fields)
        let mut h = Hasher::new();
        for x in &want {
            h.field(x.as_bytes());
        }
        let digest: Digest = h.finish();
        let mut text = b"rs/".to_vec();
        text.extend_from_slice(&digest.to_hex());
        assert_eq!(request_key(&want).text(), &text[..]);
    }

    /// Q-RS-2: no MaskOut anywhere in the key, even with `$out` set in content mode.
    #[test]
    fn q_rs_2_no_mask_out() {
        let out = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-foo";
        let store = Store::from_env(&Env::from_pairs(&[
            ("JIG_STORE_IDENTITY", "content"),
            ("out", out),
        ]));
        let inv = parse_rust_invocation(&[
            "a.rs",
            "--crate-type=lib",
            "--emit=dep-info",
            "--out-dir=/b",
        ]);
        let src = format!("const P: &str = \"{out}/x\";\n");
        let f = key_fields(
            &store,
            &inv,
            &KeyInputs {
                tool_path: b"rustc",
                stdlib: b"",
                cwd: b"/w",
                env: &Env::default(),
                source: src.as_bytes(),
            },
        );
        assert_eq!(f.last().unwrap(), src.as_bytes());
    }

    #[test]
    fn stdlib_layout() {
        let dir = tempfile::tempdir().unwrap();
        let t = std::fs::canonicalize(dir.path()).unwrap();
        // <root>/bin/rustc, reached through a symlink
        let root = t.join("tc");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/rustc"), "").unwrap();
        symlink(root.join("bin/rustc"), t.join("rustc-link")).unwrap();
        let lib = root.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
        std::fs::create_dir_all(&lib).unwrap();
        for n in [
            "libstd-1.rlib",
            "libcore-2.rlib",
            "libstd-1.so",
            "liballoc-3.rlib",
            "libcore.rlib",
        ] {
            std::fs::write(lib.join(n), n).unwrap();
        }
        std::fs::create_dir_all(root.join("lib/rustlib/wasm32/lib")).unwrap();
        std::fs::write(root.join("lib/rustlib/wasm32/lib/libcore-9.rlib"), "w").unwrap();
        // a plain file in rustlib (`components`) has no lib/ to list
        std::fs::write(root.join("lib/rustlib/components"), "").unwrap();
        // a sysroot of symlinks, one dangling, and the same rlib as the toolchain's
        let sys = t.join("sys");
        let slib = sys.join("lib/rustlib/x86_64-unknown-linux-gnu/lib");
        std::fs::create_dir_all(&slib).unwrap();
        symlink(lib.join("libstd-1.rlib"), slib.join("libstd-1.rlib")).unwrap();
        symlink(t.join("nowhere"), slib.join("libcore-dangling.rlib")).unwrap();
        symlink("loop.rlib", slib.join("libstd-loop.rlib")).unwrap();
        symlink("libstd-loop.rlib", slib.join("loop.rlib")).unwrap();

        let libs = stdlib_libs(&b(&t.join("rustc-link")), b"").unwrap();
        let want: Vec<Vec<u8>> = vec![
            b(&lib.join("libcore-2.rlib")),
            b(&lib.join("libstd-1.rlib")),
            b(&root.join("lib/rustlib/wasm32/lib/libcore-9.rlib")),
        ];
        let mut want_sorted = want.clone();
        want_sorted.sort();
        assert_eq!(libs, want_sorted);

        let libs = stdlib_libs(&b(&root.join("bin/rustc")), &b(&sys)).unwrap();
        let mut want2 = want.clone();
        want2.push(b(&lib.join("libstd-1.rlib"))); // duplicates are kept
        // weakly_canonical of a dangling link is the link itself
        want2.push(b(&slib.join("libcore-dangling.rlib")));
        want2.push(Vec::new()); // ELOOP: ""
        want2.sort();
        assert_eq!(libs, want2);

        assert_eq!(stdlib_libs(b"/nonexistent/rustc", b""), None);

        // the ids: `?` for no canonical rustc, `(InputId or ?),` otherwise
        let store = Store::from_env(&Env::default());
        let mut c = Client::new();
        assert_eq!(stdlib_ids(&mut c, &store, b"/nonexistent/rustc", b""), b"?");
        let ids = stdlib_ids(&mut c, &store, &b(&root.join("bin/rustc")), &b(&sys));
        let mut expect = Vec::new();
        for l in &want2 {
            expect.extend_from_slice(&store.input_id(l).unwrap_or_else(|| b"?".to_vec()));
            expect.push(b',');
        }
        assert_eq!(ids, expect);
        assert!(ids.starts_with(b"?,?,") && !ids[4..].contains(&b'?'));
        // a toolchain without lib/rustlib has an empty id list
        assert_eq!(stdlib_ids(&mut c, &store, &b(&sys.join("lib")), b""), b"");
    }
}
