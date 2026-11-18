//! Manifests: the stored list of inputs that ties k1 to k2 (`behaviors/jig-keys.md §Manifest`,
//! `manifest.cc:94-170`).
//!
//! A manifest is a sequence of `\n`-terminated lines, `<Key(path)>\t<InputId>` for each input
//! and `!<Key(path)>` for each path looked up and not found. k2 is
//! `Hasher(Field(k1-text), Field(line)…)` over the lines without their `\n`.
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
pub fn build<I: AsRef<[u8]>, A: AsRef<[u8]>>(
    store: &Store,
    k1: &RequestKey,
    inputs: &[I],
    primary_source: &[u8],
    absent: &[A],
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
pub fn validate(
    store: &Store,
    k1: &RequestKey,
    text: &[u8],
    ask: &mut AskIdentities<'_>,
) -> Result<ResultKey, Vec<u8>> {
    let entries = parse(store, text);
    let paths: Vec<&[u8]> = entries
        .iter()
        .filter(|e| e.tab.is_some())
        .map(|e| &e.path[..])
        .collect();
    prefetch_identities(store, &paths, ask);
    let mut hasher = Hasher::new();
    hasher.field(k1.text());
    for e in &entries {
        match e.tab {
            None => {
                if exists(&e.path) {
                    let mut why = b"appeared:".to_vec();
                    why.extend_from_slice(e.stored_path());
                    return Err(why);
                }
            }
            Some(_) => {
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
}
