//! rustc's dep-info file (`behaviors/ec-rustc-mode.md §Dep-info parsing`, `rustc_mode.cc:58-78`,
//! `ParseDepfile` in `manifest.cc:23-62`), with ADOPTED Q-RS-3 `# env-dep:` tracking.

use ekacache_core::bytes::{find, split};
use ekacache_core::fspath;

/// What a dep-info file says (`DepInfo`, `rustc_mode.h`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DepInfo {
    /// Every rule head, verbatim, in file order (the `.d` itself included).
    pub outputs: Vec<Vec<u8>>,
    /// The first rule's prerequisites, `absolute(p).lexically_normal()` against `cwd`.
    pub inputs: Vec<Vec<u8>>,
    /// ADOPTED Q-RS-3: the variables of `# env-dep:NAME[=value]` lines, deduplicated, in
    /// first-seen order.
    pub env_deps: Vec<Vec<u8>>,
}

const ENV_DEP: &[u8] = b"# env-dep:";

/// `ParseDepInfo(text)`; relative inputs are resolved against `cwd` (rustc's, which is ec's).
#[must_use]
pub fn parse_dep_info(text: &[u8], cwd: &[u8]) -> DepInfo {
    let mut info = DepInfo::default();
    for line in split(text, b'\n', false) {
        if let Some(rest) = line.strip_prefix(ENV_DEP) {
            // ADOPTED Q-RS-3: legacy skips every `#` line
            let name = &rest[..rest.iter().position(|&c| c == b'=').unwrap_or(rest.len())];
            if !info.env_deps.iter().any(|n| n == name) {
                info.env_deps.push(name.to_vec());
            }
            continue;
        }
        if line.starts_with(b"#") {
            continue;
        }
        let Some(colon) = find(line, b": ") else {
            continue; // an `input:` phony rule (nothing after the colon)
        };
        info.outputs.push(line[..colon].to_vec());
        if info.inputs.is_empty() {
            // relative to rustc's cwd (cargo runs it from the workspace root): absolute, so the
            // manifest can find, hash and later re-check them
            info.inputs = parse_depfile(line)
                .iter()
                .filter_map(|p| fspath::absolute_in(p, cwd))
                .map(|p| fspath::normalize(&p))
                .collect();
        }
    }
    info
}

/// `ParseDepfile(text)` (`manifest.cc:23-62`), shared with cc mode.
pub use ekacache_core::manifest::parse_depfile;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    /// `jig_test.cc` `TestDepfile`.
    #[test]
    fn depfile_vectors() {
        let text = b"out/foo.o: foo.c \\\n  /inc/a.h /inc/b.h \\\n /inc/sp\\ ace.h\n/inc/a.h:\n/inc/b.h:\n";
        assert_eq!(
            parse_depfile(text),
            v(&["foo.c", "/inc/a.h", "/inc/b.h", "/inc/sp ace.h"])
        );
        assert_eq!(parse_depfile(b"x: \\\n a.c\n"), v(&["a.c"]));
        assert!(parse_depfile(b"").is_empty());
        assert_eq!(
            parse_depfile(
                b"conftest: \\\n /s/lib/Scrt1.o \\\n /tmp/conftest-1.o \\\n /s/lib/libc.so\n\n/s/lib/libc.so:\n"
            ),
            v(&["/s/lib/Scrt1.o", "/tmp/conftest-1.o", "/s/lib/libc.so"])
        );
        // a `:` inside a token does not end the targets; a token ending in `:` does
        assert_eq!(parse_depfile(b"c:/x.o: a\n"), v(&["a"]));
        assert_eq!(parse_depfile(b"t:\ta b"), v(&["a", "b"]));
        // the end of the text counts as a newline after the last byte
        assert_eq!(parse_depfile(b"t: a"), v(&["a"]));
        assert_eq!(parse_depfile(b"t:"), Vec::<Vec<u8>>::new());
    }

    /// `jig_test.cc` `TestDepInfo`, plus the adopted env-dep names.
    #[test]
    fn dep_info_vector() {
        let info = parse_dep_info(
            b"out/t.d: src/lib.rs src/m.rs\n\nout/libt.rlib: src/lib.rs src/m.rs\n\nsrc/lib.rs:\nsrc/m.rs:\n# env-dep:X\n",
            b"/w",
        );
        assert_eq!(info.outputs, v(&["out/t.d", "out/libt.rlib"]));
        assert_eq!(info.inputs, v(&["/w/src/lib.rs", "/w/src/m.rs"]));
        assert_eq!(info.env_deps, v(&["X"]));
    }

    #[test]
    fn dep_info_rules() {
        let text = b"/o/libfoo-1.rmeta: src/lib.rs ../x/./y.rs /abs/z.rs\n\
            /o/foo-1.d: other.rs\n\
            # checksum:blake3=abc\n\
            #env-dep:NOT\n\
            # env-dep:B=1=2\n\
            # env-dep:A\n\
            # env-dep:B=3\n\
            # env-dep:\n\
            src/lib.rs:\n";
        let info = parse_dep_info(text, b"/w/c/");
        assert_eq!(info.outputs, v(&["/o/libfoo-1.rmeta", "/o/foo-1.d"]));
        // inputs from the first rule only, lexically normalised
        assert_eq!(
            info.inputs,
            v(&["/w/c/src/lib.rs", "/w/x/y.rs", "/abs/z.rs"])
        );
        assert_eq!(info.env_deps, v(&["B", "A", ""]));
        // heads are verbatim (no unescaping); a rule whose head has `: ` later is cut at the first
        let info = parse_dep_info(b"a\\ b: c: d\n", b"/w");
        assert_eq!(info.outputs, v(&["a\\ b"]));
        // a first rule without prerequisites leaves the inputs to the next rule
        let info = parse_dep_info(b"x.d: \nlib.rlib: a.rs\n", b"/w");
        assert_eq!(info.outputs, v(&["x.d", "lib.rlib"]));
        assert_eq!(info.inputs, v(&["/w/a.rs"]));
    }
}
