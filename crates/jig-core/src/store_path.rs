//! Store paths, identity modes, masking and lexical keys (`behaviors/jig-keys.md §Store paths`,
//! `§Key: lexical normalisation`, `§InputId`).
//!
//! `STORE_DIR` is the build-time constant `JIG_STORE_DIR` (see `build.rs`). `Store` is built from
//! an explicit `Env` snapshot; it never reads `std::env` itself.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;

use crate::bytes::{find_byte_from, find_from, os_path, replace_all, split};
use crate::env::Env;
use crate::fspath;
use crate::hasher::hash_of;

include!(concat!(env!("OUT_DIR"), "/store_dir.rs"));

/// Base-32 characters before the `-` in a store path name.
pub const STORE_HASH_LENGTH: usize = 32;

/// Nix base-32 (no `e o u t`).
const NIX_BASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Nix store hashes never contain `e`: no real path collides with the placeholder.
const OUT_PLACEHOLDER: &[u8; STORE_HASH_LENGTH] = b"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

/// What may follow a store root in a depfile, response file or config text.
const PATH_ENDS: &[u8] = b" \t\n\\:\"';,)]}>";

/// `IsStorePath(p)` ⇔ `len(p) > len(STORE)` ∧ `p` starts with `STORE` ∧ `p[len(STORE)] == '/'`.
#[must_use]
pub fn is_store_path(p: &[u8]) -> bool {
    p.len() > STORE_DIR.len() && p.starts_with(STORE_DIR) && p[STORE_DIR.len()] == b'/'
}

/// `StateDir()`: `STORE` up to its last `/`, + `/var/nix` (`/nix/store` → `/nix/var/nix`).
#[must_use]
pub fn state_dir() -> Vec<u8> {
    let cut = STORE_DIR.iter().rposition(|&c| c == b'/').unwrap_or(0);
    let mut out = STORE_DIR[..cut].to_vec();
    out.extend_from_slice(b"/var/nix");
    out
}

/// The default jigd socket: `StateDir() + "/jigd/socket"`.
#[must_use]
pub fn default_socket() -> Vec<u8> {
    let mut out = state_dir();
    out.extend_from_slice(b"/jigd/socket");
    out
}

fn is_store_hash(t: &[u8]) -> bool {
    t.len() == STORE_HASH_LENGTH && t.iter().all(|c| NIX_BASE32.contains(c))
}

/// `MaskHashes(text)` (`store.cc:61-72`): every `STORE/<32 base-32 chars>-` becomes
/// `STORE/*-`; scanning resumes at `h + 1` in the modified text.
#[must_use]
pub fn mask_hashes(text: &[u8]) -> Vec<u8> {
    let mut prefix = STORE_DIR.to_vec();
    prefix.push(b'/');
    let mut t = text.to_vec();
    let mut pos = 0;
    while let Some(hit) = find_from(&t, &prefix, pos) {
        let h = hit + prefix.len();
        if t.len() > h + STORE_HASH_LENGTH
            && t[h + STORE_HASH_LENGTH] == b'-'
            && is_store_hash(&t[h..h + STORE_HASH_LENGTH])
        {
            t.splice(h..h + STORE_HASH_LENGTH, [b'*']);
        }
        pos = h + 1;
    }
    t
}

/// `MaskRandomNames`: cmake's `TryCompile-XXXXXX` and `cmTC_xxxxx` scratch names get one fixed
/// spelling each.
#[must_use]
pub fn mask_random_names(text: &[u8]) -> Vec<u8> {
    let mut t = text.to_vec();
    for (prefix, width) in [(&b"TryCompile-"[..], 6usize), (&b"cmTC_"[..], 5usize)] {
        let mut pos = 0;
        while let Some(hit) = find_from(&t, prefix, pos) {
            let start = hit + prefix.len();
            if t.len() < start + width {
                break;
            }
            if t[start..start + width]
                .iter()
                .all(u8::is_ascii_alphanumeric)
            {
                t[start..start + width].fill(b'#');
            }
            pos = hit + prefix.len();
        }
    }
    t
}

/// `NormalizePathArg(arg)` (`store.cc`): the value of a path-like argument, lexically
/// normalised. `-D`/`-U` are program text and stay as they are.
#[must_use]
pub fn normalize_path_arg(arg: &[u8]) -> Vec<u8> {
    if arg.starts_with(b"-D") || arg.starts_with(b"-U") {
        return arg.to_vec();
    }
    let start;
    if arg.starts_with(b"--") {
        match arg.iter().position(|&c| c == b'=') {
            None => return arg.to_vec(),
            Some(eq) => start = eq + 1,
        }
    } else if arg.first() == Some(&b'-') && arg.len() > 2 && arg[1] != b'-' {
        // single-dash option glued to its value (Q-KEY-9: split after two bytes)
        let mut s = 2;
        if let Some(eq) = arg.iter().position(|&c| c == b'=')
            && !arg[..eq].contains(&b'/')
        {
            s = eq + 1;
        }
        start = s;
    } else if arg.first() == Some(&b'-') {
        return arg.to_vec();
    } else {
        start = 0;
    }
    let value = &arg[start..];
    if !value.contains(&b'/') && !value.starts_with(b".") {
        return arg.to_vec();
    }
    let mut normal = fspath::normalize(value);
    if normal.len() > 1 && normal.last() == Some(&b'/') {
        normal.pop();
    }
    if normal.is_empty() {
        normal.push(b'.');
    }
    let mut out = arg[..start].to_vec();
    out.extend_from_slice(&normal);
    out
}

/// `JIG_STORE_IDENTITY`: exactly `content` selects content mode; anything else is path mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityMode {
    Path,
    Content,
}

/// The `Store` singleton of legacy, constructed once from the environment.
#[derive(Debug)]
pub struct Store {
    mode: IdentityMode,
    /// `$out`, verbatim ("" when unset).
    out: Vec<u8>,
    /// Its 32 hash characters; empty outside a build and in path mode.
    out_hash: Vec<u8>,
    /// Masked root → real root, in construction order; the first of duplicates wins.
    roots: Vec<(Vec<u8>, Vec<u8>)>,
    /// Identities the daemon answered for (`RememberIdentity`).
    known_ids: RefCell<HashMap<Vec<u8>, Vec<u8>>>,
}

impl Store {
    /// `Store::Store()` (`store.cc:29-44`).
    #[must_use]
    pub fn from_env(env: &Env) -> Self {
        let mode = if env.get("JIG_STORE_IDENTITY") == Some(b"content") {
            IdentityMode::Content
        } else {
            IdentityMode::Path
        };
        let out = env.get("out").unwrap_or_default().to_vec();
        let mut s = Self {
            mode,
            out,
            out_hash: Vec::new(),
            roots: Vec::new(),
            known_ids: RefCell::new(HashMap::new()),
        };
        if mode == IdentityMode::Path {
            return s;
        }
        let n = STORE_DIR.len();
        if is_store_path(&s.out)
            && s.out.len() > n + 1 + STORE_HASH_LENGTH
            && s.out[n + 1 + STORE_HASH_LENGTH] == b'-'
        {
            s.out_hash = s.out[n + 1..n + 1 + STORE_HASH_LENGTH].to_vec();
        }
        let roots_env = env.get("JIG_STORE_ROOTS").unwrap_or_default();
        let mut roots: Vec<Vec<u8>> = split(roots_env, b' ', false)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();
        roots.push(s.out.clone());
        for mut root in roots {
            // <store>/<hash-name>[/…]
            if let Some(cut) = find_byte_from(&root, b'/', n + 1) {
                root.truncate(cut);
            }
            let masked = mask_hashes(&root);
            if masked != root && !s.roots.iter().any(|(m, _)| *m == masked) {
                s.roots.push((masked, root));
            }
        }
        s
    }

    #[must_use]
    pub fn mode(&self) -> IdentityMode {
        self.mode
    }

    #[must_use]
    pub fn by_content(&self) -> bool {
        self.mode == IdentityMode::Content
    }

    /// `$out` as read at construction.
    #[must_use]
    pub fn out(&self) -> &[u8] {
        &self.out
    }

    /// `Key(arg)` = `MaskForReplay(MaskRandomNames(NormalizePathArg(arg)))`.
    #[must_use]
    pub fn key(&self, arg: &[u8]) -> Vec<u8> {
        let k = mask_random_names(&normalize_path_arg(arg));
        if self.by_content() {
            mask_hashes(&k)
        } else {
            k
        }
    }

    /// `MaskForReplay(text)`: `MaskHashes` in content mode, the identity in path mode.
    #[must_use]
    pub fn mask_for_replay<'a>(&self, text: &'a [u8]) -> Cow<'a, [u8]> {
        if self.by_content() {
            Cow::Owned(mask_hashes(text))
        } else {
            Cow::Borrowed(text)
        }
    }

    fn swap_out_hash<'a>(&self, bytes: &'a [u8], back: bool) -> Cow<'a, [u8]> {
        if self.out_hash.is_empty() {
            return Cow::Borrowed(bytes);
        }
        let mut real = STORE_DIR.to_vec();
        real.push(b'/');
        real.extend_from_slice(&self.out_hash);
        real.push(b'-');
        let mut placeholder = STORE_DIR.to_vec();
        placeholder.push(b'/');
        placeholder.extend_from_slice(OUT_PLACEHOLDER);
        placeholder.push(b'-');
        let (from, into) = if back {
            (&placeholder, &real)
        } else {
            (&real, &placeholder)
        };
        Cow::Owned(replace_all(bytes, from, into))
    }

    /// `MaskOut(bytes)`: `STORE/<out_hash>-` → `STORE/eeee…e-`. Identity without an out hash.
    #[must_use]
    pub fn mask_out<'a>(&self, b: &'a [u8]) -> Cow<'a, [u8]> {
        self.swap_out_hash(b, false)
    }

    /// `UnmaskOut(bytes)`: the reverse of `mask_out`.
    #[must_use]
    pub fn unmask_out<'a>(&self, b: &'a [u8]) -> Cow<'a, [u8]> {
        self.swap_out_hash(b, true)
    }

    /// `Resolve(p)` (`store.cc:158-168`): a masked store path back to this build's root; `None`
    /// for a root the build lacks. Paths not starting with `STORE/*-` are returned unchanged.
    #[must_use]
    pub fn resolve<'a>(&self, p: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        let mut masked_prefix = STORE_DIR.to_vec();
        masked_prefix.extend_from_slice(b"/*-");
        if !p.starts_with(&masked_prefix) {
            return Some(Cow::Borrowed(p));
        }
        let slash = find_byte_from(p, b'/', STORE_DIR.len() + 3);
        let root = &p[..slash.unwrap_or(p.len())];
        let (_, real) = self.roots.iter().find(|(m, _)| m == root)?;
        let mut out = real.clone();
        if let Some(s) = slash {
            out.extend_from_slice(&p[s..]);
        }
        Some(Cow::Owned(out))
    }

    /// `ResolveAll(text)` (`store.cc:170-185`), used on replayed depfiles and `get-dir` text.
    ///
    /// ADOPTED Q-KEY-1: in path mode this is the identity (legacy masks in both modes). In
    /// content mode: mask, then put each known root back where it is followed by end of text,
    /// `/`, or one of `` \t\n\\:"';,)]}>``.
    #[must_use]
    pub fn resolve_all<'a>(&self, text: &'a [u8]) -> Cow<'a, [u8]> {
        if !self.by_content() {
            return Cow::Borrowed(text); // ADOPTED Q-KEY-1
        }
        let mut t = mask_hashes(text);
        for (masked, real) in &self.roots {
            let mut pos = 0;
            while let Some(hit) = find_from(&t, masked, pos) {
                let end = hit + masked.len();
                if end < t.len() && t[end] != b'/' && !PATH_ENDS.contains(&t[end]) {
                    pos = end;
                    continue;
                }
                t.splice(hit..end, real.iter().copied());
                pos = hit + real.len();
            }
        }
        Cow::Owned(t)
    }

    /// `Stored(bytes)` (`dir_mode.cc`): the form a `put-dir` file takes as a blob.
    ///
    /// ADOPTED Q-KEY-1 / Q-DIR-1: path mode stores bytes unchanged; content mode masks store
    /// hashes in NUL-free (text) files.
    #[must_use]
    pub fn stored<'a>(&self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if self.by_content() && !bytes.contains(&0) {
            Cow::Owned(mask_hashes(bytes))
        } else {
            Cow::Borrowed(bytes)
        }
    }

    /// `Restored(blob)` (`dir_mode.cc`): `ResolveAll` for NUL-free blobs (the identity in path
    /// mode, adopted Q-KEY-1).
    #[must_use]
    pub fn restored<'a>(&self, blob: &'a [u8]) -> Cow<'a, [u8]> {
        if blob.contains(&0) {
            Cow::Borrowed(blob)
        } else {
            self.resolve_all(blob)
        }
    }

    /// `ToolId(path)` (`store.cc:187-192`): `Key(canonical(dirname) / basename)`, or `Key(path)`
    /// when `canonical` fails (including the empty dirname of a bare name).
    #[must_use]
    pub fn tool_id(&self, path: &[u8]) -> Vec<u8> {
        let dir = fspath::parent_path(path);
        if dir.is_empty() {
            return self.key(path);
        }
        match std::fs::canonicalize(os_path(&dir)) {
            Ok(real) => self.key(&fspath::join(
                real.as_os_str().as_bytes(),
                fspath::filename(path),
            )),
            Err(_) => self.key(path),
        }
    }

    /// `DaemonMayIdentify(path)`: content mode, a store path, and not `$out` or under it.
    #[must_use]
    pub fn daemon_may_identify(&self, path: &[u8]) -> bool {
        let under_out = !self.out.is_empty()
            && path.starts_with(&self.out)
            && (path.len() == self.out.len() || path[self.out.len()] == b'/');
        self.by_content() && is_store_path(path) && !under_out
    }

    /// `RememberIdentity(path, id)`.
    pub fn remember_identity(&self, path: &[u8], id: Vec<u8>) {
        self.known_ids.borrow_mut().insert(path.to_vec(), id);
    }

    /// `InputId(path)` (`store.cc:204-217`): `S:<path>` for a store path in path mode (never
    /// opened); a remembered daemon answer; else `C:<HashOf(bytes or MaskOut(bytes))>`; `None`
    /// when the file cannot be read.
    #[must_use]
    pub fn input_id(&self, path: &[u8]) -> Option<Vec<u8>> {
        if is_store_path(path) && !self.by_content() {
            let mut id = b"S:".to_vec();
            id.extend_from_slice(path);
            return Some(id);
        }
        if let Some(known) = self.known_ids.borrow().get(path) {
            return Some(known.clone());
        }
        if path.is_empty() {
            return None;
        }
        let content = std::fs::read(os_path(path)).ok()?;
        let digest = if is_store_path(path) {
            hash_of(&content)
        } else {
            hash_of(&self.mask_out(&content))
        };
        let mut id = b"C:".to_vec();
        id.extend_from_slice(&digest.to_hex());
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: impl AsRef<[u8]>) -> String {
        String::from_utf8(v.as_ref().to_vec()).unwrap()
    }

    fn path_store() -> Store {
        Store::from_env(&Env::default())
    }

    fn content_store(pairs: &[(&str, &str)]) -> Store {
        let mut v = vec![("JIG_STORE_IDENTITY", "content")];
        v.extend_from_slice(pairs);
        Store::from_env(&Env::from_pairs(&v))
    }

    /// The vectors below spell `/nix/store`; they hold only for the default `JIG_STORE_DIR`.
    fn default_store_dir() -> bool {
        STORE_DIR == b"/nix/store"
    }

    #[test]
    fn mask_hashes_vectors() {
        if !default_store_dir() {
            return;
        }
        assert_eq!(
            s(mask_hashes(
                b"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h"
            )),
            "/nix/store/*-zlib-1.3/include/zlib.h"
        );
        for t in [
            "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x",
            "/nix/store/short-name",
            "/nix/store/*-name/x",
        ] {
            assert_eq!(s(mask_hashes(t.as_bytes())), t);
        }
        // every occurrence, anywhere
        assert_eq!(
            s(mask_hashes(
                b"-I/nix/store/0123456789abcdfghijklmnpqrsvwxyz-a/x:\"/nix/store/1123456789abcdfghijklmnpqrsvwxyz-b\"x"
            )),
            "-I/nix/store/*-a/x:\"/nix/store/*-b\"x"
        );
    }

    /// Masking follows the build-time `STORE_DIR` (implementation/01-workspace.md §Build
    /// configuration); run with `JIG_STORE_DIR=/gnu/store` to exercise a non-default store.
    #[test]
    fn masking_follows_store_dir() {
        let real = [
            STORE_DIR,
            b"/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h",
        ]
        .concat();
        let masked = [STORE_DIR, b"/*-zlib-1.3/include/zlib.h"].concat();
        assert_eq!(mask_hashes(&real), masked);
        let arg = [b"-I", &real[..]].concat();
        assert_eq!(path_store().key(&arg), arg);
        assert_eq!(content_store(&[]).key(&arg), [b"-I", &masked[..]].concat());
        assert!(is_store_path(&real));
        let id = [b"S:", &real[..]].concat();
        assert_eq!(path_store().input_id(&real), Some(id));
        if !default_store_dir() {
            let nix = b"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h";
            assert_eq!(mask_hashes(nix), nix);
            assert!(!is_store_path(nix));
        }
    }

    #[test]
    fn key_vectors() {
        // behaviors/jig-keys.md §Key test vectors (path mode unless noted)
        let st = path_store();
        for (i, o) in [
            ("-I./include//sub/../", "-Iinclude"),
            ("./src/../src/a.c", "src/a.c"),
            ("--sysroot=/a/b/../c/", "--sysroot=/a/c"),
            ("-DFOO=./a//b", "-DFOO=./a//b"),
            ("-Ufoo/../bar", "-Ufoo/../bar"),
            ("-O2", "-O2"),
            ("-I", "-I"),
            ("-D", "-D"),
            ("--param=x", "--param=x"),
            ("foo.c", "foo.c"),
            (".hidden", ".hidden"),
            (".", "."),
            ("./", "."),
            ("a/", "a"),
            ("/", "/"),
            ("//x", "/x"),
            ("x/..", "."),
            ("a/b/../..", "."),
            ("../..", "../.."),
            ("a/../../b", "../b"),
            ("/..", "/"),
            ("/../a", "/a"),
            ("-I=/x/../y", "-I=/y"),
            ("-Ia/b=c/../d", "-Ia/d"),
            ("-mfoo=a/b", "-mfoo=a/b"),
            ("--x=", "--x="),
            ("--x=./", "--x=."),
            ("-L./lib", "-Llib"),
            ("-l./x", "-lx"),
            ("-o./a/../b.o", "-ob.o"),
            ("-fprofile-use=./p/../q", "-fprofile-use=q"),
            ("-Wl,-rpath,/a/../b", "-Wl,-rpath,/b"),
            ("-Wp,-MD,./x.d", "-Wp,-MD,./x.d"),
            ("-include./x.h", "-include./x.h"),
            ("-iquote./a/../b", "-iquote./b"),
            ("@./rsp", "@./rsp"),
            ("cwd=/a/../b", "cwd=/b"),
            ("/b/TryCompile-teSZ4z/src.c", "/b/TryCompile-######/src.c"),
            (
                "CMakeFiles/cmTC_7c7e5.dir/x.c.o",
                "CMakeFiles/cmTC_#####.dir/x.c.o",
            ),
            ("TryCompile-abcdefgh", "TryCompile-######gh"),
            ("cmTC_abcde_cmTC_12345", "cmTC_#####_cmTC_#####"),
        ] {
            assert_eq!(s(st.key(i.as_bytes())), o, "Key({i:?})");
        }
        if default_store_dir() {
            let arg = b"-I/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include";
            assert_eq!(st.key(arg), arg);
            assert_eq!(
                s(content_store(&[]).key(arg)),
                "-I/nix/store/*-zlib-1.3/include"
            );
        }
    }

    #[test]
    fn modes() {
        assert_eq!(path_store().mode(), IdentityMode::Path);
        for v in ["", "Content", "path", "content "] {
            let st = Store::from_env(&Env::from_pairs(&[("JIG_STORE_IDENTITY", v)]));
            assert_eq!(st.mode(), IdentityMode::Path, "{v:?}");
        }
        assert_eq!(content_store(&[]).mode(), IdentityMode::Content);
    }

    #[test]
    fn mask_out_and_resolve() {
        if !default_store_dir() {
            return;
        }
        let out = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-pkg-1.0";
        let st = content_store(&[
            ("out", out),
            (
                "JIG_STORE_ROOTS",
                " /nix/store/1123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include /nix/store/2123456789abcdfghijklmnpqrsvwxyz-zlib-1.3",
            ),
        ]);
        let text = format!("x {out}/lib y");
        let masked = st.mask_out(text.as_bytes()).into_owned();
        assert_eq!(
            s(&masked),
            "x /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-pkg-1.0/lib y"
        );
        assert_eq!(s(st.unmask_out(&masked)), text);
        // path mode: identity
        let p = Store::from_env(&Env::from_pairs(&[("out", out)]));
        assert_eq!(s(p.mask_out(text.as_bytes())), text);
        // first root wins (Q-KEY-12)
        assert_eq!(
            s(st.resolve(b"/nix/store/*-zlib-1.3/include/zlib.h").unwrap()),
            "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h"
        );
        assert!(st.resolve(b"/nix/store/*-other/y.h").is_none());
        assert_eq!(s(st.resolve(b"rel/x.h").unwrap()), "rel/x.h");
        assert_eq!(s(st.resolve(b"/nix/store/*-pkg-1.0").unwrap()), out);
        // ResolveAll: whole roots only
        let dep = b"x.o: /nix/store/3123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h /nix/store/*-zlib-1.3-ng/a.h\n";
        assert_eq!(
            s(st.resolve_all(dep)),
            "x.o: /nix/store/1123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h /nix/store/*-zlib-1.3-ng/a.h\n"
        );
    }

    #[test]
    fn adopted_q_key_1_path_mode_resolve_all_is_identity() {
        let st = path_store();
        let dep = b"x.o: x.c /nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h\n";
        assert_eq!(st.resolve_all(dep), &dep[..]);
        assert_eq!(st.stored(dep), &dep[..]);
        assert_eq!(st.restored(dep), &dep[..]);
    }

    #[test]
    fn input_ids() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.h");
        std::fs::write(&f, b"int a;\n").unwrap();
        let fp = f.as_os_str().as_bytes();
        for st in [path_store(), content_store(&[])] {
            assert_eq!(
                s(st.input_id(fp).unwrap()),
                "C:0d937065826369d51c16019b838d8932"
            );
            assert_eq!(st.input_id(dir.path().as_os_str().as_bytes()), None);
            assert_eq!(st.input_id(b""), None);
        }
        if default_store_dir() {
            let sp = b"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h";
            assert_eq!(
                s(path_store().input_id(sp).unwrap()),
                "S:/nix/store/0123456789abcdfghijklmnpqrsvwxyz-zlib-1.3/include/zlib.h"
            );
            assert_eq!(content_store(&[]).input_id(sp), None);
            let st = content_store(&[]);
            st.remember_identity(sp, b"C:00".to_vec());
            assert_eq!(st.input_id(sp).unwrap(), b"C:00");
        }
    }

    #[test]
    fn store_dir_helpers() {
        if !default_store_dir() {
            return;
        }
        assert_eq!(s(state_dir()), "/nix/var/nix");
        assert_eq!(s(default_socket()), "/nix/var/nix/jigd/socket");
        assert!(is_store_path(b"/nix/store/"));
        assert!(!is_store_path(b"/nix/store"));
        assert!(!is_store_path(b"/nix/storex/a"));
        let st = content_store(&[("out", "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-o")]);
        assert!(st.daemon_may_identify(b"/nix/store/x-zlib/a.h"));
        assert!(!st.daemon_may_identify(b"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-o"));
        assert!(!st.daemon_may_identify(b"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-o/x"));
        assert!(st.daemon_may_identify(b"/nix/store/0123456789abcdfghijklmnpqrsvwxyz-ox"));
        assert!(!path_store().daemon_may_identify(b"/nix/store/x-zlib/a.h"));
    }

    #[test]
    fn tool_ids() {
        let st = path_store();
        assert_eq!(st.tool_id(b"gcc"), b"gcc");
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let p = dir.path().join("sub/../cc");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let got = st.tool_id(p.as_os_str().as_bytes());
        assert_eq!(s(got), format!("{}/cc", real.display()));
        assert_eq!(s(st.tool_id(b"/does/not/exist/cc")), "/does/not/exist/cc");
    }
}
