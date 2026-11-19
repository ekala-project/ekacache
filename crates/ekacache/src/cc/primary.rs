//! The primary identity and the toolchain ids (`behaviors/jig-cc-mode.md §Primary identity`,
//! `cc_mode.cc:131-190`).

use ekacache_core::bytes::{contains, os_path};
use ekacache_core::fspath;
use ekacache_core::store_path::Store;

use super::Invocation;
use crate::client::Client;
use crate::process::ProcEnv;
use crate::twokey::prefetch_identities;
use crate::util::{on_path, read_file};

/// What k1 hashes besides the arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Primary {
    pub text: Vec<u8>,
    /// How many leading bytes of `text` are the source file (0 for a link).
    source_len: usize,
}

impl Primary {
    /// The source file's bytes (empty for a link).
    #[must_use]
    pub fn source_bytes(&self) -> &[u8] {
        &self.text[..self.source_len]
    }
}

/// `PrimaryIdentity(inv)` (`:167-190`): for a compile the source bytes, then `\npch=<InputId>`
/// per PCH (a build-tree binary tied to the paths it was made under); for a link
/// `<input>=<InputId>\n` per input. `None` when one is unreadable.
#[must_use]
pub fn primary_identity(inv: &Invocation, store: &Store) -> Option<Primary> {
    if !inv.link {
        let mut text = read_file(&inv.source)?;
        let source_len = text.len();
        for pch in &inv.pch {
            let id = store.input_id(pch)?;
            text.extend_from_slice(b"\npch=");
            text.extend_from_slice(&id);
        }
        return Some(Primary { text, source_len });
    }
    let mut ids = Vec::new();
    for input in &inv.inputs {
        let id = store.input_id(input)?;
        ids.extend_from_slice(input);
        ids.push(b'=');
        ids.extend_from_slice(&id);
        ids.push(b'\n');
    }
    Some(Primary {
        text: ids,
        source_len: 0,
    })
}

/// `MentionsBuildTime`: `__DATE__`, `__TIME__` and `__TIMESTAMP__` expand to the build's time.
#[must_use]
pub fn mentions_build_time(text: &[u8]) -> bool {
    contains(text, b"__DATE__") || contains(text, b"__TIME__") || contains(text, b"__TIMESTAMP__")
}

/// The toolchain files whose identities key a compile: the compiler (and for links every
/// `--ld-path=` linker) resolved, plus the `libLLVM*`, `libclang-cpp*` and `liblld*` regular
/// files (not symlinks) in `<real>/../../lib`; sorted bytewise, deduplicated.
fn toolchain_files(compiler: &[u8], inv: &Invocation, env: &ProcEnv) -> Vec<Vec<u8>> {
    let mut tools = vec![on_path(compiler, env)];
    if inv.links() {
        tools.extend(
            inv.args
                .iter()
                .filter_map(|a| a.strip_prefix(b"--ld-path=").map(<[u8]>::to_vec)),
        );
    }
    let mut files = Vec::new();
    for tool in &tools {
        let Ok(real) = std::fs::canonicalize(os_path(tool)) else {
            continue;
        };
        let real = real.into_os_string().into_encoded_bytes();
        let lib = fspath::join(&fspath::parent_path(&fspath::parent_path(&real)), b"lib");
        files.push(real);
        let Ok(entries) = std::fs::read_dir(os_path(&lib)) else {
            continue;
        };
        for entry in entries.map_while(Result::ok) {
            let name = entry.file_name().into_encoded_bytes();
            let wanted = name.starts_with(b"libLLVM")
                || name.starts_with(b"libclang-cpp")
                || name.starts_with(b"liblld");
            if wanted && entry.file_type().is_ok_and(|t| t.is_file()) {
                files.push(fspath::join(&lib, &name));
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// `ToolchainIds(cache, compiler, inv)` (`:131-164`): one `IDS` round trip, then `InputId` (or
/// `?`) of each toolchain file, each followed by `,`. `ToolId` masks the store hash, so a
/// toolchain patched at the same version must still miss.
pub fn toolchain_ids(
    client: &mut Client,
    store: &Store,
    compiler: &[u8],
    inv: &Invocation,
    env: &ProcEnv,
) -> Vec<u8> {
    let files = toolchain_files(compiler, inv, env);
    prefetch_identities(client, store, &files);
    let mut ids = Vec::new();
    for file in &files {
        ids.extend_from_slice(&store.input_id(file).unwrap_or_else(|| b"?".to_vec()));
        ids.push(b',');
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekacache_core::env::Env;

    fn bytes(p: &std::path::Path) -> Vec<u8> {
        p.as_os_str().as_encoded_bytes().to_vec()
    }

    #[test]
    fn build_time() {
        // jig_test.cc TestKeyEnvironment
        assert!(mentions_build_time(b"const char* d = __DATE__;"));
        assert!(mentions_build_time(b"__TIMESTAMP__") && mentions_build_time(b"x__TIME__x"));
        assert!(!mentions_build_time(b"int x;"));
    }

    #[test]
    fn identities() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.c");
        std::fs::write(&src, "int x;\n").unwrap();
        let pch = dir.path().join("x.pch");
        std::fs::write(&pch, "PCH").unwrap();
        let store = Store::from_env(&Env::default());
        let mut inv = Invocation {
            source: bytes(&src),
            pch: vec![bytes(&pch)],
            ..Invocation::default()
        };
        let p = primary_identity(&inv, &store).unwrap();
        let mut want = b"int x;\n\npch=".to_vec();
        want.extend_from_slice(&store.input_id(&bytes(&pch)).unwrap());
        assert_eq!(p.text, want);
        assert_eq!(p.source_bytes(), b"int x;\n");
        inv.pch.push(b"/nonexistent/y.pch".to_vec());
        assert!(primary_identity(&inv, &store).is_none());

        let link = Invocation {
            link: true,
            inputs: vec![bytes(&src), bytes(&pch)],
            ..Invocation::default()
        };
        let p = primary_identity(&link, &store).unwrap();
        let mut want = bytes(&src);
        want.push(b'=');
        want.extend_from_slice(&store.input_id(&bytes(&src)).unwrap());
        want.push(b'\n');
        want.extend_from_slice(&bytes(&pch));
        want.push(b'=');
        want.extend_from_slice(&store.input_id(&bytes(&pch)).unwrap());
        want.push(b'\n');
        assert_eq!(p.text, want);
        assert!(p.source_bytes().is_empty());
    }

    #[test]
    fn toolchain() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("tc/bin")).unwrap();
        std::fs::create_dir_all(root.join("tc/lib")).unwrap();
        std::fs::create_dir_all(root.join("ld/bin")).unwrap();
        std::fs::write(root.join("tc/bin/clang-19"), "").unwrap();
        std::os::unix::fs::symlink("clang-19", root.join("tc/bin/clang")).unwrap();
        std::fs::write(root.join("tc/lib/libLLVM.so.19.1"), "").unwrap();
        std::os::unix::fs::symlink("libLLVM.so.19.1", root.join("tc/lib/libLLVM.so")).unwrap();
        std::fs::write(root.join("tc/lib/libclang-cpp.so.19"), "").unwrap();
        std::fs::write(root.join("tc/lib/libc.so"), "").unwrap();
        std::fs::create_dir(root.join("tc/lib/liblld-dir")).unwrap();
        std::fs::write(root.join("ld/bin/ld.lld"), "").unwrap();
        let r = |p: &str| bytes(&root.join(p));
        let env = ProcEnv::new(Env::from_pairs(&[("PATH", bytes(&root.join("tc/bin")))]));
        let compile = Invocation {
            args: vec![[b"--ld-path=".as_slice(), &r("ld/bin/ld.lld")].concat()],
            ..Invocation::default()
        };
        let want = vec![
            r("tc/bin/clang-19"),
            r("tc/lib/libLLVM.so.19.1"),
            r("tc/lib/libclang-cpp.so.19"),
        ];
        assert_eq!(toolchain_files(b"clang", &compile, &env), want);
        let link = Invocation {
            link: true,
            ..compile
        };
        let mut want_link = want.clone();
        want_link.insert(0, r("ld/bin/ld.lld"));
        want_link.sort();
        assert_eq!(toolchain_files(b"clang", &link, &env), want_link);
        // an unresolvable compiler contributes nothing
        assert!(toolchain_files(b"/nonexistent/cc", &Invocation::default(), &env).is_empty());

        let store = Store::from_env(&Env::default());
        let mut client = Client::new();
        let ids = toolchain_ids(&mut client, &store, b"clang", &Invocation::default(), &env);
        let mut expect = Vec::new();
        for f in &want {
            expect.extend_from_slice(&store.input_id(f).unwrap());
            expect.push(b',');
        }
        assert_eq!(ids, expect);
    }
}
