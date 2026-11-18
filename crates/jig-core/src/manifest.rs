//! Manifests: the stored list of inputs that ties k1 to k2 (`behaviors/jig-keys.md §Manifest`,
//! `manifest.cc:94-170`).
//!
//! A manifest is a sequence of `\n`-terminated lines, `<Key(path)>\t<InputId>` for each input
//! and `!<Key(path)>` for each path looked up and not found. k2 is
//! `Hasher(Field(k1-text), Field(line)…)` over the lines without their `\n`.
//!
//! rustc manifests (k1 text `rs/…`) also carry `$env:<NAME>\t<id>` lines between the input
//! and the `!` lines, one per `env!`/`option_env!` dependency (ADOPTED Q-RS-3,
//! `behaviors/jig-rustc-mode.md §Q-RS-3`): `build_with_env`, `validate_with_env`.
//!
//! The daemon's `IDS` request is abstracted as `ask`: given the paths the daemon may identify,
//! it returns one answer per path (`""` = no answer), or an empty vector when the request
//! failed. `ask` is never called with an empty list.

use std::collections::BTreeSet;

use crate::bytes::{find_byte_from, os_path, split};
use crate::hasher::Hasher;
use crate::keys::{RequestKey, ResultKey};
use crate::store_path::{Store, is_store_path};

/// The daemon's `IDS` request: one answer per path, or empty on failure.
pub type AskIdentities<'a> = dyn FnMut(&[&[u8]]) -> Vec<Vec<u8>> + 'a;

/// A storable manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub text: Vec<u8>,
    pub result_key: ResultKey,
}

/// Why `build` refused to make a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unstorable {
    /// ADOPTED Q-KEY-3: an input had no identity (deleted after the compile, unreadable, a
    /// directory). Carries `Key(path)` of the first such input, for the
    /// `<source> not-cached:unreadable:<Key(path)>` log subject.
    Unreadable(Vec<u8>),
}

/// `std::filesystem::exists(path, error)`: follows symlinks, relative to the cwd; any error
/// counts as absent.
#[must_use]
pub fn exists(path: &[u8]) -> bool {
    !path.is_empty() && std::fs::metadata(os_path(path)).is_ok()
}

/// `PrefetchIdentities` (`manifest.cc:94-108`): one `IDS` request for the paths the daemon may
/// identify; each non-empty answer is remembered.
pub fn prefetch_identities<P: AsRef<[u8]>>(
    store: &Store,
    paths: &[P],
    ask: &mut AskIdentities<'_>,
) {
    let wanted: Vec<&[u8]> = paths
        .iter()
        .map(AsRef::as_ref)
        .filter(|p| store.daemon_may_identify(p))
        .collect();
    if wanted.is_empty() {
        return;
    }
    let answers = ask(&wanted);
    for (path, id) in wanted.iter().zip(answers) {
        if !id.is_empty() {
            store.remember_identity(path, id);
        }
    }
}

/// k2 over already formatted lines (without `\n`).
#[must_use]
pub fn result_key_of<L: AsRef<[u8]>>(k1_text: &[u8], lines: &[L]) -> ResultKey {
    let mut h = Hasher::new();
    h.field(k1_text);
    for l in lines {
        h.field(l.as_ref());
    }
    ResultKey::new(&h.finish())
}

/// `BuildManifest` (`manifest.cc:110-140`), with adopted Q-KEY-3.
///
/// `inputs` in order, not deduplicated, minus `primary_source` (exact compare); then each
/// `absent` path that is non-empty, not a store path and does not exist now, once per line.
/// The same as `build_with_env` with no env lines.
pub fn build<I: AsRef<[u8]>, A: AsRef<[u8]>>(
    store: &Store,
    k1: &RequestKey,
    inputs: &[I],
    primary_source: &[u8],
    absent: &[A],
    ask: &mut AskIdentities<'_>,
) -> Result<Manifest, Unstorable> {
    build_with_env(
        store,
        k1,
        inputs,
        primary_source,
        absent,
        &[] as &[&[u8]],
        ask,
    )
}

/// `build` with ADOPTED Q-RS-3 env-dep lines (`behaviors/jig-rustc-mode.md §Q-RS-3`): each of
/// `env_lines` (made by `env_line`, without `\n`) is emitted after the input lines and before
/// the `!` lines, in the order given, and hashed into k2 like every other line.
pub fn build_with_env<I: AsRef<[u8]>, A: AsRef<[u8]>, L: AsRef<[u8]>>(
    store: &Store,
    k1: &RequestKey,
    inputs: &[I],
    primary_source: &[u8],
    absent: &[A],
    env_lines: &[L],
    ask: &mut AskIdentities<'_>,
) -> Result<Manifest, Unstorable> {
    prefetch_identities(store, inputs, ask);
    let mut text = Vec::new();
    let mut hasher = Hasher::new();
    hasher.field(k1.text());
    let mut add = |line: &[u8]| {
        text.extend_from_slice(line);
        text.push(b'\n');
        hasher.field(line);
    };
    for path in inputs {
        let path = path.as_ref();
        if path == primary_source {
            continue;
        }
        let Some(id) = store.input_id(path) else {
            // ADOPTED Q-KEY-3: legacy drops the line silently
            return Err(Unstorable::Unreadable(store.key(path)));
        };
        let mut line = store.key(path);
        line.push(b'\t');
        line.extend_from_slice(&id);
        add(&line);
    }
    for line in env_lines {
        add(line.as_ref()); // ADOPTED Q-RS-3
    }
    // store paths stay absent; what exists by now the compiler wrote itself (-o, -MF)
    let mut seen = BTreeSet::new();
    for path in absent {
        let path = path.as_ref();
        if path.is_empty() || is_store_path(path) || exists(path) {
            continue;
        }
        let mut line = b"!".to_vec();
        line.extend_from_slice(&store.key(path));
        if seen.insert(line.clone()) {
            add(&line);
        }
    }
    Ok(Manifest {
        text,
        result_key: ResultKey::new(&hasher.finish()),
    })
}

/// One parsed manifest line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// The path in this build's store roots; `""` when the build lacks the root.
    pub path: Vec<u8>,
    /// The line as stored.
    pub line: &'a [u8],
    /// Index of the first `\t`; `None` for a `!` line.
    pub tab: Option<usize>,
}

impl Entry<'_> {
    /// The stored (masked) path: after `!`, or before the tab.
    #[must_use]
    pub fn stored_path(&self) -> &[u8] {
        match self.tab {
            None => &self.line[1..],
            Some(t) => &self.line[..t],
        }
    }

    /// The stored identity of an input line.
    #[must_use]
    pub fn stored_id(&self) -> Option<&[u8]> {
        self.tab.map(|t| &self.line[t + 1..])
    }
}

/// `ParseManifest`: split on `\n` (empty pieces dropped); `!…` is absent, a piece with a tab is
/// an input, anything else is ignored (also for k2).
#[must_use]
pub fn parse<'a>(store: &Store, text: &'a [u8]) -> Vec<Entry<'a>> {
    let mut entries = Vec::new();
    for line in split(text, b'\n', false) {
        if line.first() == Some(&b'!') {
            let path = store
                .resolve(&line[1..])
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default();
            entries.push(Entry {
                path,
                line,
                tab: None,
            });
        } else if let Some(tab) = find_byte_from(line, b'\t', 0) {
            let path = store
                .resolve(&line[..tab])
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default();
            entries.push(Entry {
                path,
                line,
                tab: Some(tab),
            });
        }
    }
    entries
}

/// `ValidateManifest` (`manifest.cc:142-170`): k2 recomputed from the stored lines, or the
/// first failure, `appeared:<stored path>` or `inputs-changed:<stored path>`.
///
/// `$env:` lines are not interpreted (a cc manifest never has them); see `validate_with_env`.
pub fn validate(
    store: &Store,
    k1: &RequestKey,
    text: &[u8],
    ask: &mut AskIdentities<'_>,
) -> Result<ResultKey, Vec<u8>> {
    validate_impl(store, k1, text, None, ask)
}

/// `validate` with ADOPTED Q-RS-3 env-dep entries (`behaviors/jig-rustc-mode.md §Q-RS-3`).
///
/// Only when the k1 text starts with `rs/`: an input-shaped line (one with a tab) that starts
/// with `$env:` is an env entry. `NAME` is the text between `$env:` and the first tab; its id is
/// recomputed from `env(NAME)` (`env_id`) and compared with the stored id as a byte string, and
/// a mismatch fails with `env-changed:<NAME>`. In any other manifest such a line is an ordinary
/// input line, as in `validate`.
pub fn validate_with_env(
    store: &Store,
    k1: &RequestKey,
    text: &[u8],
    env: &EnvLookup<'_>,
    ask: &mut AskIdentities<'_>,
) -> Result<ResultKey, Vec<u8>> {
    validate_impl(store, k1, text, Some(env), ask)
}

fn validate_impl(
    store: &Store,
    k1: &RequestKey,
    text: &[u8],
    env: Option<&EnvLookup<'_>>,
    ask: &mut AskIdentities<'_>,
) -> Result<ResultKey, Vec<u8>> {
    let env = env.filter(|_| k1.text().starts_with(RUSTC_KEY_PREFIX));
    let is_env = |e: &Entry<'_>| env.is_some() && e.tab.is_some() && e.line.starts_with(ENV_PREFIX);
    let entries = parse(store, text);
    let paths: Vec<&[u8]> = entries
        .iter()
        .filter(|e| e.tab.is_some() && !is_env(e))
        .map(|e| &e.path[..])
        .collect();
    prefetch_identities(store, &paths, ask);
    let mut hasher = Hasher::new();
    hasher.field(k1.text());
    for e in &entries {
        match (e.tab, env) {
            (None, _) => {
                if exists(&e.path) {
                    let mut why = b"appeared:".to_vec();
                    why.extend_from_slice(e.stored_path());
                    return Err(why);
                }
            }
            (Some(tab), Some(env)) if is_env(e) => {
                // ADOPTED Q-RS-3
                let name = &e.line[ENV_PREFIX.len()..tab];
                if env_id(env(name).as_deref()).as_slice() != &e.line[tab + 1..] {
                    let mut why = b"env-changed:".to_vec();
                    why.extend_from_slice(name);
                    return Err(why);
                }
            }
            (Some(_), _) => {
                if store.input_id(&e.path).as_deref() != e.stored_id() {
                    let mut why = b"inputs-changed:".to_vec();
                    why.extend_from_slice(e.stored_path());
                    return Err(why);
                }
            }
        }
        hasher.field(e.line);
    }
    Ok(ResultKey::new(&hasher.finish()))
}

/// `ParseDepfile(text)` (`manifest.cc:23-62`): the prerequisites of the **first** rule of a
/// make-style depfile (compiler `-MD` output, lld's `--dependency-file`). `\`+newline continues a
/// line, `\`+space is a space inside a path; the target ends at a `:` followed by space, newline
/// or `\` (or at a token ending in `:`); the first newline after the target ends the rule.
#[must_use]
pub fn parse_depfile(text: &[u8]) -> Vec<Vec<u8>> {
    let mut deps = Vec::new();
    let mut token = Vec::new();
    let mut after_colon = false;
    let flush = |token: &mut Vec<u8>, after_colon: &mut bool, deps: &mut Vec<Vec<u8>>| {
        if token.is_empty() {
            return;
        }
        if *after_colon {
            deps.push(std::mem::take(token));
        } else if token.last() == Some(&b':') {
            *after_colon = true;
        }
        token.clear();
    };
    let mut i = 0;
    while i < text.len() {
        let cur = text[i];
        let next = text.get(i + 1).copied().unwrap_or(b'\n');
        if cur == b'\\' && next == b'\n' {
            i += 1; // line continuation
        } else if cur == b'\\' && next == b' ' {
            token.push(b' '); // escaped space in a path
            i += 1;
        } else if cur == b'\n' {
            flush(&mut token, &mut after_colon, &mut deps);
            if after_colon {
                break; // first rule complete
            }
        } else if cur == b' ' || cur == b'\t' {
            flush(&mut token, &mut after_colon, &mut deps);
        } else if cur == b':' && !after_colon && matches!(next, b' ' | b'\n' | b'\\') {
            after_colon = true;
            token.clear();
        } else {
            token.push(cur);
        }
        i += 1;
    }
    flush(&mut token, &mut after_colon, &mut deps);
    deps
}

/// The start of an env-dep line (ADOPTED Q-RS-3). rustc inputs are absolute paths, so no input
/// line can start with it.
pub const ENV_PREFIX: &[u8] = b"$env:";

/// `$env:` lines are interpreted only in manifests whose k1 text starts with this.
const RUSTC_KEY_PREFIX: &[u8] = b"rs/";

/// jig's environment as an env-dep sees it: `getenv(name)`, `None` when unset.
pub type EnvLookup<'a> = dyn Fn(&[u8]) -> Option<Vec<u8>> + 'a;

/// The `<id>` of an env-dep line (ADOPTED Q-RS-3): `unset`, or the lowercase hex digest of
/// `Hasher(Field("set"), Field(value))`. Set-but-empty and unset differ.
#[must_use]
pub fn env_id(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        None => b"unset".to_vec(),
        Some(v) => Hasher::new().field(b"set").field(v).finish().hex(),
    }
}

/// `$env:<NAME>\t<id>`, without the `\n` (ADOPTED Q-RS-3).
#[must_use]
pub fn env_line(name: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let id = env_id(value);
    let mut line = Vec::with_capacity(ENV_PREFIX.len() + name.len() + 1 + id.len());
    line.extend_from_slice(ENV_PREFIX);
    line.extend_from_slice(name);
    line.push(b'\t');
    line.extend_from_slice(&id);
    line
}

#[cfg(test)]
mod tests {
    #[test]
    fn depfiles() {
        // jig_test.cc TestDepfile
        let v = |items: &[&str]| -> Vec<Vec<u8>> {
            items.iter().map(|s| s.as_bytes().to_vec()).collect()
        };
        let text = b"out/foo.o: foo.c \\\n  /inc/a.h /inc/b.h \\\n /inc/sp\\ ace.h\n/inc/a.h:\n/inc/b.h:\n";
        assert_eq!(
            parse_depfile(text),
            v(&["foo.c", "/inc/a.h", "/inc/b.h", "/inc/sp ace.h"])
        );
        assert_eq!(parse_depfile(b"x: \\\n a.c\n"), v(&["a.c"]));
        assert!(parse_depfile(b"").is_empty());
        assert_eq!(
            parse_depfile(b"conftest: \\\n /s/lib/Scrt1.o \\\n /tmp/conftest-1.o \\\n /s/lib/libc.so\n\n/s/lib/libc.so:\n"),
            v(&["/s/lib/Scrt1.o", "/tmp/conftest-1.o", "/s/lib/libc.so"])
        );
        // the plain-assembly pseudo depfile, a drive-letter-like colon, a target ending in ':'
        assert_eq!(parse_depfile(b"o: a.s"), v(&["a.s"]));
        assert_eq!(parse_depfile(b"c:x.o: a.c"), v(&["a.c"]));
        assert_eq!(parse_depfile(b"t:\ta b\nc"), v(&["a", "b"]));
        assert_eq!(parse_depfile(b"t:"), Vec::<Vec<u8>>::new());
        assert_eq!(parse_depfile(b"no colon here\n"), Vec::<Vec<u8>>::new());
    }

    use super::*;
    use crate::env::Env;
    use crate::hasher::hash_of;
    use crate::keys::Tool;
    use crate::store_path::STORE_DIR;

    fn no_daemon() -> impl FnMut(&[&[u8]]) -> Vec<Vec<u8>> {
        |_: &[&[u8]]| Vec::new()
    }

    #[test]
    fn pure_hasher_vectors() {
        // behaviors/jig-keys.md §Manifest test vectors (pure Hasher)
        let k1 = b"0123456789abcdef0123456789abcdef";
        let none: [&[u8]; 0] = [];
        assert_eq!(
            result_key_of(k1, &none).text(),
            b"5573430e23fe9fc1644dd647cc911c2f"
        );
        let lines = [
            &b"include/a.h\tC:0d937065826369d51c16019b838d8932"[..],
            b"!early/a.h",
        ];
        assert_eq!(
            result_key_of(k1, &lines).text(),
            b"f445848e6f30d9745cdb88c0744c7859"
        );
        assert_eq!(
            result_key_of(b"rs/0123456789abcdef0123456789abcdef", &lines).text(),
            b"11c6c31720976272d3ac6ab556847c54"
        );
    }

    #[test]
    fn composite_k1_vector() {
        // behaviors/jig-keys.md §Composite test vector
        let mut h = Hasher::new();
        for f in [
            &b"cc=/nix/store/*-gcc-14.3.0/bin/gcc"[..],
            b"cwd=/build/source",
            b"src=a.c",
            b"mode=compile",
            b"env=",
            b"-c",
            b"-O2",
            b"int main(){}\n\ntoolchain=C:00000000000000000000000000000000,",
        ] {
            h.field(f);
        }
        let d = h.finish();
        assert_eq!(
            RequestKey::new(Tool::Cc, &d).text(),
            b"544ade1ae7d1ab515b12eaba8c82aca5"
        );
        assert_eq!(
            RequestKey::new(Tool::Rustc, &d).text(),
            b"rs/544ade1ae7d1ab515b12eaba8c82aca5"
        );
    }

    /// The `/tmp/jigprobe/t` scenario. The expected k2 hashes these literal paths, so the test
    /// must use exactly this directory.
    #[test]
    fn manifest_vectors() {
        let root = std::path::Path::new("/tmp/jigprobe");
        let t = root.join("t");
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("a.h"), b"int a;\n").unwrap();
        std::fs::write(t.join("b.h"), b"int b;\n").unwrap();
        std::fs::write(t.join("src.c"), b"").unwrap();
        let p = |s: &str| format!("/tmp/jigprobe/{s}").into_bytes();
        let inputs = [
            p("t/src.c"),
            p("t/a.h"),
            p("t/./b.h"),
            p("t/missing.h"),
            p("t/a.h"),
        ];
        let store_abs = [
            STORE_DIR,
            b"/1123456789abcdfghijklmnpqrsvwxyz-glibc-2.40/include/x.h",
        ]
        .concat();
        let absent = [
            p("t/early/a.h"),
            Vec::new(),
            p("t/early/a.h"),
            p("t/a.h"),
            store_abs,
            b"rel/q.h".to_vec(),
        ];
        let k1 = RequestKey::new(Tool::Cc, &hash_of(b"k1"));
        assert_eq!(k1.text(), b"ffb4332755be52674d663ce9586917c9");
        let expected = "/tmp/jigprobe/t/a.h\tC:0d937065826369d51c16019b838d8932\n\
                        /tmp/jigprobe/t/b.h\tC:709be0706c9c4004a3ae4d9c20050e55\n\
                        /tmp/jigprobe/t/a.h\tC:0d937065826369d51c16019b838d8932\n\
                        !/tmp/jigprobe/t/early/a.h\n\
                        !rel/q.h\n";
        let content = Env::from_pairs(&[("JIG_STORE_IDENTITY", "content")]);
        for env in [Env::default(), content] {
            let store = Store::from_env(&env);
            // ADOPTED Q-KEY-3: the missing input makes the manifest unstorable
            assert_eq!(
                build(
                    &store,
                    &k1,
                    &inputs,
                    &p("t/src.c"),
                    &absent,
                    &mut no_daemon()
                ),
                Err(Unstorable::Unreadable(p("t/missing.h")))
            );
            let readable: Vec<_> = inputs
                .iter()
                .filter(|i| !i.ends_with(b"missing.h"))
                .cloned()
                .collect();
            let m = build(
                &store,
                &k1,
                &readable,
                &p("t/src.c"),
                &absent,
                &mut no_daemon(),
            )
            .unwrap();
            assert_eq!(String::from_utf8_lossy(&m.text), expected);
            assert_eq!(m.result_key.text(), b"ca06f7cce6259fa171f39edb0e29b4a8");
            assert_eq!(
                validate(&store, &k1, &m.text, &mut no_daemon()),
                Ok(m.result_key.clone())
            );
            let junk = [&m.text[..], b"junk line\n\n"].concat();
            assert_eq!(
                validate(&store, &k1, &junk, &mut no_daemon()),
                Ok(m.result_key.clone())
            );
        }
        let store = Store::from_env(&Env::default());
        let m = {
            let readable: Vec<_> = inputs
                .iter()
                .filter(|i| !i.ends_with(b"missing.h"))
                .cloned()
                .collect();
            build(
                &store,
                &k1,
                &readable,
                &p("t/src.c"),
                &absent,
                &mut no_daemon(),
            )
            .unwrap()
        };
        std::fs::create_dir_all(t.join("early")).unwrap();
        std::fs::write(t.join("early/a.h"), b"").unwrap();
        assert_eq!(
            validate(&store, &k1, &m.text, &mut no_daemon()),
            Err(b"appeared:/tmp/jigprobe/t/early/a.h".to_vec())
        );
        std::fs::remove_dir_all(t.join("early")).unwrap();
        std::fs::write(t.join("b.h"), b"int b2;\n").unwrap();
        assert_eq!(
            validate(&store, &k1, &m.text, &mut no_daemon()),
            Err(b"inputs-changed:/tmp/jigprobe/t/b.h".to_vec())
        );
        let _ = std::fs::remove_dir_all(&t);

        // content mode, no root for *-other
        let content = Store::from_env(&Env::from_pairs(&[("JIG_STORE_IDENTITY", "content")]));
        let absent_line = [STORE_DIR, b"/*-other/y.h"].concat();
        let text = [b"!", &absent_line[..], b"\n"].concat();
        let k2 = validate(&content, &k1, &text, &mut no_daemon()).unwrap();
        if STORE_DIR == b"/nix/store" {
            assert_eq!(k2.text(), b"7eb18b0a88c25e4e28595f120f4ba2da");
        }
        let input_line = [STORE_DIR, b"/*-other/x.h"].concat();
        let text = [&input_line[..], b"\tC:00\n"].concat();
        assert_eq!(
            validate(&content, &k1, &text, &mut no_daemon()),
            Err([&b"inputs-changed:"[..], &input_line].concat())
        );
    }

    #[test]
    fn prefetch_asks_once_for_daemon_paths_only() {
        let out = [STORE_DIR, b"/0123456789abcdfghijklmnpqrsvwxyz-o"].concat();
        let env = Env::from_pairs(&[
            (&b"JIG_STORE_IDENTITY"[..], &b"content"[..]),
            (&b"out"[..], &out[..]),
        ]);
        let store = Store::from_env(&env);
        let a = [STORE_DIR, b"/x-a/a.h"].concat();
        let b = [STORE_DIR, b"/x-b/b.h"].concat();
        let under_out = [&out[..], b"/x.h"].concat();
        let mut calls = Vec::new();
        let mut ask = |paths: &[&[u8]]| {
            calls.push(paths.iter().map(|p| p.to_vec()).collect::<Vec<_>>());
            vec![b"C:aa".to_vec(), Vec::new()]
        };
        prefetch_identities(
            &store,
            &[a.clone(), b"rel".to_vec(), under_out, b.clone()],
            &mut ask,
        );
        assert_eq!(calls, vec![vec![a.clone(), b.clone()]]);
        assert_eq!(store.input_id(&a), Some(b"C:aa".to_vec()));
        assert_eq!(store.input_id(&b), None);
        // nothing to ask: no request
        let mut never = |_: &[&[u8]]| -> Vec<Vec<u8>> { panic!("asked") };
        prefetch_identities(&Store::from_env(&Env::default()), &[a], &mut never);
    }

    #[test]
    fn env_dep_ids() {
        // ADOPTED Q-RS-3: behaviors/jig-rustc-mode.md §Q-RS-3
        assert_eq!(env_id(None), b"unset");
        assert_eq!(
            env_id(Some(b"a")),
            Hasher::new().field(b"set").field(b"a").finish().hex()
        );
        // bytes 3:set1:a
        assert_eq!(env_id(Some(b"a")), hash_of(b"3:set1:a").hex());
        assert_ne!(env_id(Some(b"")), env_id(None));
        assert_eq!(env_line(b"X", None), b"$env:X\tunset");
        assert_eq!(
            env_line(b"Y", Some(b"")),
            [&b"$env:Y\t"[..], &hash_of(b"3:set0:").hex()].concat()
        );
    }

    #[test]
    fn env_dep_lines_in_rustc_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("m.rs");
        std::fs::write(&a, "mod m;\n").unwrap();
        let a = a.as_os_str().as_encoded_bytes().to_vec();
        let gone = [dir.path().as_os_str().as_encoded_bytes(), b"/gone.rs"].concat();
        let store = Store::from_env(&Env::default());
        let rs = RequestKey::new(Tool::Rustc, &hash_of(b"k1"));
        let env_lines = [env_line(b"FOO", Some(b"1")), env_line(b"BAR", None)];
        let m = build_with_env(
            &store,
            &rs,
            &[&a],
            b"",
            &[&gone],
            &env_lines,
            &mut no_daemon(),
        )
        .unwrap();
        // inputs, then env lines, then `!` lines; all of them in k2
        let input_line = [&a[..], b"\tC:", &hash_of(b"mod m;\n").hex()].concat();
        let absent_line = [b"!", &gone[..]].concat();
        let lines = [
            input_line.clone(),
            env_lines[0].clone(),
            env_lines[1].clone(),
            absent_line,
        ];
        let mut text = Vec::new();
        for l in &lines {
            text.extend_from_slice(l);
            text.push(b'\n');
        }
        assert_eq!(m.text, text);
        assert_eq!(m.result_key, result_key_of(rs.text(), &lines));

        let lookup = |foo: Option<&'static [u8]>, bar: Option<&'static [u8]>| {
            move |name: &[u8]| match name {
                b"FOO" => foo.map(<[u8]>::to_vec),
                b"BAR" => bar.map(<[u8]>::to_vec),
                _ => None,
            }
        };
        let check =
            |env: &EnvLookup<'_>| validate_with_env(&store, &rs, &m.text, env, &mut no_daemon());
        assert_eq!(check(&lookup(Some(b"1"), None)), Ok(m.result_key.clone()));
        assert_eq!(
            check(&lookup(Some(b"2"), None)),
            Err(b"env-changed:FOO".to_vec())
        );
        assert_eq!(check(&lookup(None, None)), Err(b"env-changed:FOO".to_vec()));
        // set but empty is not unset
        assert_eq!(
            check(&lookup(Some(b"1"), Some(b""))),
            Err(b"env-changed:BAR".to_vec())
        );
        // the first failure wins: the input line comes first
        std::fs::write(os_path(&a), "mod n;\n").unwrap();
        let mut why = b"inputs-changed:".to_vec();
        why.extend_from_slice(&a);
        assert_eq!(check(&lookup(Some(b"2"), None)), Err(why));
        std::fs::write(os_path(&a), "mod m;\n").unwrap();

        // without an env lookup, and in a cc manifest, `$env:` is an ordinary input line
        assert_eq!(
            validate(&store, &rs, &m.text, &mut no_daemon()),
            Err(b"inputs-changed:$env:FOO".to_vec())
        );
        let cc = RequestKey::new(Tool::Cc, &hash_of(b"k1"));
        let mc = build_with_env(
            &store,
            &cc,
            &[&a],
            b"",
            &[] as &[&[u8]],
            &env_lines,
            &mut no_daemon(),
        )
        .unwrap();
        assert_eq!(
            validate_with_env(
                &store,
                &cc,
                &mc.text,
                &lookup(Some(b"1"), None),
                &mut no_daemon()
            ),
            Err(b"inputs-changed:$env:FOO".to_vec())
        );
        // with no env lines, build_with_env is build
        assert_eq!(
            build(&store, &rs, &[&a], b"", &[&gone], &mut no_daemon()),
            build_with_env(
                &store,
                &rs,
                &[&a],
                b"",
                &[&gone],
                &[] as &[&[u8]],
                &mut no_daemon()
            )
        );
        // an `$env:` piece without a tab is ignored like any other junk line
        let junk = [&m.text[..], b"$env:JUNK\n"].concat();
        assert_eq!(
            validate_with_env(
                &store,
                &rs,
                &junk,
                &lookup(Some(b"1"), None),
                &mut no_daemon()
            ),
            Ok(m.result_key.clone())
        );
    }
}
