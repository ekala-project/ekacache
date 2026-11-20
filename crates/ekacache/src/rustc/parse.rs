//! `ParseRustInvocation` (`behaviors/ec-rustc-mode.md §Argument classification`,
//! `rustc_mode.cc:127-236`): which rustc invocations are cacheable, and what of their argv goes
//! into k1.
//!
//! ADOPTED Q-RS-5: a `--crate-type` value is split on `,` for the link check. The separate
//! `--crate-type <v>` form needs no extra case: `TakeCodegenOption` joins it into
//! `--crate-type=<v>` before the check, so both forms are classified the same way, in legacy as
//! here.

use ekacache_core::bytes::{contains, split};

/// What `ParseRustInvocation` makes of rustc's arguments (`RustInvocation`, `rustc_mode.h`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustInvocation {
    /// rustc's arguments, passed on unchanged in every path.
    pub args: Vec<Vec<u8>>,
    /// The arguments that go into k1 (`Key(a)` each), in argv order.
    pub key_args: Vec<Vec<u8>>,
    /// `--extern name=path`: the paths, in order.
    pub externs: Vec<Vec<u8>>,
    /// The (last) `.rs` argument, as spelled.
    pub source: Vec<u8>,
    pub crate_name: Vec<u8>,
    pub out_dir: Vec<u8>,
    /// `--sysroot`: where cross builds find the target's libstd.
    pub sysroot: Vec<u8>,
    /// `-C extra-filename=`: cargo's metadata stem, replaced by `@` in stored names.
    pub extra_filename: Vec<u8>,
    /// The crate links (bin, proc-macro, cdylib, dylib, or no `--crate-type`).
    pub links: bool,
    pub has_crate_type: bool,
    pub cacheable: bool,
    /// `--print`, `-vV`, `--version`, `-` as the source: cargo probing rustc.
    pub query: bool,
    pub has_dep_info: bool,
}

/// Options whose value is the next token: flag and value are kept together in the key
/// (`kTwoTokenOptions`).
const TWO_TOKEN_OPTIONS: &[&[u8]] = &[
    b"-C",
    b"--cfg",
    b"--check-cfg",
    b"--cap-lints",
    b"--edition",
    b"--target",
    b"--crate-type",
    b"-Z",
    b"-W",
    b"-A",
    b"-D",
    b"-F",
    b"--error-format",
    b"--json",
    b"--diagnostic-width",
    b"--remap-path-prefix",
    b"-l",
];

/// The crate types whose output is linked (`rustc_mode.cc:178-179`).
const LINKING_CRATE_TYPES: &[&[u8]] = &[b"bin", b"proc-macro", b"cdylib", b"dylib"];

const CRATE_TYPE: &[u8] = b"--crate-type=";

fn concat(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(a.len() + b.len());
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    v
}

/// `ParseRustInvocation(args)`; `args` are rustc's arguments (without rustc itself).
#[must_use]
pub fn parse_rust_invocation<A: AsRef<[u8]>>(args: &[A]) -> RustInvocation {
    let args: Vec<&[u8]> = args.iter().map(AsRef::as_ref).collect();
    let mut inv = RustInvocation {
        args: args.iter().map(|a| a.to_vec()).collect(),
        key_args: Vec::new(),
        externs: Vec::new(),
        source: Vec::new(),
        crate_name: Vec::new(),
        out_dir: Vec::new(),
        sysroot: Vec::new(),
        extra_filename: Vec::new(),
        links: false,
        has_crate_type: false,
        cacheable: true,
        query: false,
        has_dep_info: false,
    };
    let mut sources = 0usize;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if take_named_option(&args, &mut i, &mut inv) {
            i += 1;
            continue;
        }
        if arg == b"--print"
            || arg.starts_with(b"--print=")
            || arg == b"-"
            || arg == b"-vV"
            || arg == b"--version"
        {
            inv.query = true;
        } else if arg == b"-o" {
            // its value token is classified on its own
            inv.cacheable = false;
        } else if !arg.starts_with(b"-") && arg.ends_with(b".rs") {
            inv.source = arg.to_vec();
            sources += 1;
        } else {
            take_codegen_option(&args, &mut i, &mut inv);
        }
        i += 1;
    }
    // what links (bin, build scripts, proc-macro, cdylib; no --crate-type means bin) embeds the
    // interpreter, RUNPATH and native libraries as absolute store paths
    inv.links = inv.links || !inv.has_crate_type;
    if inv.query || inv.links || sources != 1 || inv.out_dir.is_empty() || !inv.has_dep_info {
        inv.cacheable = false;
    }
    inv
}

/// `TakeNamedOption` (`rustc_mode.cc:127-161`): `--flag value` (when a next token exists) or
/// `--flag=value` for the named options. `true` if `args[*i]` was one; `*i` is then on its
/// last token.
fn take_named_option(args: &[&[u8]], i: &mut usize, inv: &mut RustInvocation) -> bool {
    let arg = args[*i];
    let mut take = |flag: &[u8]| -> Option<Vec<u8>> {
        if arg == flag && *i + 1 < args.len() {
            *i += 1;
            return Some(args[*i].to_vec());
        }
        if arg.len() > flag.len() && arg.starts_with(flag) && arg[flag.len()] == b'=' {
            return Some(arg[flag.len() + 1..].to_vec());
        }
        None
    };
    if let Some(value) = take(b"--out-dir") {
        // Q-RS-7: not in the key
        inv.out_dir = value;
    } else if let Some(value) = take(b"--sysroot") {
        inv.key_args.push(concat(b"--sysroot=", &value));
        inv.sysroot = value;
    } else if let Some(value) = take(b"--crate-name") {
        inv.key_args.push(concat(b"--crate-name=", &value));
        inv.crate_name = value;
    } else if let Some(value) = take(b"--extern") {
        if let Some(eq) = value.iter().position(|&c| c == b'=') {
            inv.externs.push(value[eq + 1..].to_vec());
        }
        inv.key_args.push(concat(b"--extern=", &value));
    } else if let Some(value) = take(b"--emit") {
        inv.has_dep_info = inv.has_dep_info || contains(&value, b"dep-info");
        inv.key_args.push(concat(b"--emit=", &value));
    } else if take(b"-L").is_some() {
        // Q-RS-6: only `-L x` and `-L=x` are dropped; a glued `-Lx` stays in the key
    } else {
        return false;
    }
    true
}

/// `TakeCodegenOption` (`rustc_mode.cc:165-182`): `-C`/`-Z`/… joined with their value (`-C`
/// `opt=x` and `-Copt=x` both become `-C=opt=x`).
fn take_codegen_option(args: &[&[u8]], i: &mut usize, inv: &mut RustInvocation) {
    let mut full = args[*i].to_vec();
    if TWO_TOKEN_OPTIONS.contains(&&full[..]) && *i + 1 < args.len() {
        *i += 1;
        full.push(b'=');
        full.extend_from_slice(args[*i]);
    } else if full.starts_with(b"-C") && !full.starts_with(b"-C=") {
        full.insert(2, b'=');
    }
    if full.starts_with(b"-C=incremental=") {
        inv.cacheable = false;
    } else if let Some(stem) = full.strip_prefix(b"-C=extra-filename=") {
        // Q-RS-7: not in the key
        inv.extra_filename = stem.to_vec();
    } else if !full.starts_with(b"-C=metadata=") {
        // Q-RS-7: `-C metadata` is dropped from the key (the branch above this one)
        if let Some(value) = full.strip_prefix(CRATE_TYPE) {
            // ADOPTED Q-RS-5: legacy compares the whole value, so `lib,cdylib` was cached
            inv.links = inv.links
                || split(value, b',', true)
                    .iter()
                    .any(|t| LINKING_CRATE_TYPES.contains(t));
            inv.has_crate_type = true;
        }
        inv.key_args.push(full);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> RustInvocation {
        let args: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        parse_rust_invocation(&args)
    }

    fn has(v: &[Vec<u8>], s: &str) -> bool {
        v.iter().any(|a| a == s.as_bytes())
    }

    /// `jig_test.cc` `TestRustInvocation`.
    #[test]
    fn legacy_vectors() {
        let inv = parse(&[
            "--crate-name",
            "foo",
            "--edition=2021",
            "src/lib.rs",
            "--crate-type",
            "lib",
            "--emit=dep-info,metadata,link",
            "-C",
            "metadata=abcd",
            "-C",
            "extra-filename=-abcd",
            "--out-dir",
            "/b/deps",
            "-L",
            "dependency=/b/deps",
            "--extern",
            "bar=/b/deps/libbar-1.rmeta",
            "--cap-lints",
            "allow",
        ]);
        assert!(inv.cacheable);
        assert_eq!(inv.source, b"src/lib.rs");
        assert_eq!(inv.crate_name, b"foo");
        assert_eq!(inv.out_dir, b"/b/deps");
        assert_eq!(inv.extra_filename, b"-abcd");
        assert_eq!(inv.externs, vec![b"/b/deps/libbar-1.rmeta".to_vec()]);
        assert!(has(&inv.key_args, "--crate-type=lib"));
        assert!(has(&inv.key_args, "--cap-lints=allow"));
        assert!(!has(&inv.key_args, "-C=metadata=abcd"));
        assert!(!inv.links);
        // the whole key_args, in argv order
        let want: Vec<Vec<u8>> = [
            "--crate-name=foo",
            "--edition=2021",
            "--crate-type=lib",
            "--emit=dep-info,metadata,link",
            "--extern=bar=/b/deps/libbar-1.rmeta",
            "--cap-lints=allow",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
        assert_eq!(inv.key_args, want);
        assert_eq!(inv.args.len(), 19);

        let inv = parse(&[
            "--crate-name",
            "foo",
            "src/main.rs",
            "--crate-type",
            "bin",
            "--emit=dep-info,link",
            "--out-dir",
            "/b",
            "-C",
            "linker=clang",
            "-L",
            "native=/b/build/x/out",
        ]);
        assert!(inv.links && !inv.cacheable);

        let inv = parse(&["-", "--crate-type", "lib"]);
        assert!(!inv.cacheable && inv.query);
        let inv = parse(&[
            "-",
            "--crate-name",
            "___",
            "--print=file-names",
            "--crate-type",
            "bin",
        ]);
        assert!(inv.query);
    }

    fn lib(extra: &[&str]) -> RustInvocation {
        let mut args = vec![
            "--crate-name",
            "foo",
            "src/lib.rs",
            "--emit=dep-info,link",
            "--out-dir",
            "/b",
        ];
        args.extend_from_slice(extra);
        parse(&args)
    }

    #[test]
    fn cacheability() {
        assert!(lib(&["--crate-type", "lib"]).cacheable);
        assert!(lib(&["--crate-type=rlib"]).cacheable);
        assert!(lib(&["--crate-type=staticlib"]).cacheable);
        // no --crate-type means bin
        let inv = lib(&[]);
        assert!(inv.links && !inv.cacheable);
        for t in ["bin", "proc-macro", "cdylib", "dylib"] {
            let inv = lib(&["--crate-type", t]);
            assert!(inv.links && !inv.cacheable, "{t}");
        }
        // -o, incremental, no dep-info, no out-dir, two sources, a query
        assert!(!lib(&["--crate-type=lib", "-o", "x"]).cacheable);
        let inv = lib(&["--crate-type=lib", "-C", "incremental=/tmp/i"]);
        assert!(!inv.cacheable && !inv.links);
        assert!(!has(&inv.key_args, "-C=incremental=/tmp/i"));
        assert!(!lib(&["--crate-type=lib", "-Cincremental=/tmp/i"]).cacheable);
        assert!(!parse(&["--crate-type=lib", "a.rs", "--emit=link", "--out-dir=/b"]).cacheable);
        assert!(
            parse(&[
                "--crate-type=lib",
                "a.rs",
                "--emit=dep-info",
                "--out-dir=/b"
            ])
            .cacheable
        );
        assert!(!parse(&["--crate-type=lib", "a.rs", "--emit=dep-info"]).cacheable);
        assert!(!lib(&["--crate-type=lib", "src/m.rs"]).cacheable);
        assert!(!lib(&["--crate-type=lib", "--print", "cfg"]).cacheable);
        assert!(lib(&["--crate-type=lib", "-vV"]).query);
        assert!(lib(&["--crate-type=lib", "--version"]).query);
        // `-o`'s value is classified on its own: here it is a second source
        let inv = parse(&[
            "--crate-type=lib",
            "-o",
            "x.rs",
            "a.rs",
            "--emit=dep-info",
            "--out-dir=/b",
        ]);
        assert_eq!(inv.source, b"a.rs");
        assert!(!inv.cacheable);
        // a `.rs` that starts with `-` is not a source
        let inv = lib(&["--crate-type=lib", "-x.rs"]);
        assert!(has(&inv.key_args, "-x.rs") && inv.cacheable);
    }

    #[test]
    fn named_options() {
        let inv = lib(&[
            "--crate-type=lib",
            "--sysroot",
            "/sys",
            "--extern=noeq",
            "--extern",
            "a=b=c",
            "--out-dir=/c",
            "-L=x",
        ]);
        assert_eq!(inv.sysroot, b"/sys");
        assert!(has(&inv.key_args, "--sysroot=/sys"));
        assert!(has(&inv.key_args, "--extern=noeq"));
        assert!(has(&inv.key_args, "--extern=a=b=c"));
        assert_eq!(inv.externs, vec![b"b=c".to_vec()]);
        assert_eq!(inv.out_dir, b"/c");
        assert!(
            !inv.key_args
                .iter()
                .any(|a| a.starts_with(b"-L") || a.starts_with(b"--out-dir"))
        );
        // a flag without its value token falls through to TakeCodegenOption
        let inv = parse(&["--crate-type=lib", "a.rs", "--emit=dep-info", "--out-dir"]);
        assert!(inv.out_dir.is_empty() && has(&inv.key_args, "--out-dir") && !inv.cacheable);
        // `--extern-location` is not `--extern`
        let inv = lib(&["--crate-type=lib", "--extern-location=x"]);
        assert!(inv.externs.is_empty() && has(&inv.key_args, "--extern-location=x"));
        // `--emit` values accumulate dep-info
        let inv = parse(&[
            "--crate-type=lib",
            "a.rs",
            "--emit",
            "dep-info",
            "--emit=link",
            "--out-dir=/b",
        ]);
        assert!(inv.has_dep_info && inv.cacheable);
    }

    #[test]
    fn codegen_options() {
        let inv = lib(&[
            "--crate-type=lib",
            "-Copt-level=3",
            "-C=debuginfo=2",
            "-C",
            "panic=abort",
            "--cfg",
            "feature=\"std\"",
            "-Zthreads=1",
            "-Z",
            "x",
            "--target=x86_64",
        ]);
        for a in [
            "-C=opt-level=3",
            "-C=debuginfo=2",
            "-C=panic=abort",
            "--cfg=feature=\"std\"",
            "-Zthreads=1",
            "-Z=x",
            "--target=x86_64",
        ] {
            assert!(has(&inv.key_args, a), "{a}");
        }
        // a trailing two-token flag without a value
        let inv = lib(&["--crate-type=lib", "-C"]);
        assert!(has(&inv.key_args, "-C="));
        let inv = lib(&["--crate-type=lib", "--crate-type"]);
        assert!(has(&inv.key_args, "--crate-type"));
        // the glued extra-filename and metadata forms
        let inv = lib(&["--crate-type=lib", "-Cextra-filename=-x", "-Cmetadata=y"]);
        assert_eq!(inv.extra_filename, b"-x");
        assert!(!inv.key_args.iter().any(|a| a.starts_with(b"-C=")));
    }

    /// ADOPTED Q-RS-5, in both spellings.
    #[test]
    fn q_rs_5_crate_type_split() {
        for v in [
            "lib,cdylib",
            "cdylib,lib",
            "rlib,bin",
            "lib,,proc-macro",
            "staticlib,dylib",
        ] {
            let inv = lib(&["--crate-type", v]);
            assert!(inv.links && !inv.cacheable, "separate {v}");
            let joined = format!("--crate-type={v}");
            let inv = lib(&[&joined]);
            assert!(inv.links && !inv.cacheable, "joined {v}");
            assert!(has(&inv.key_args, &joined));
        }
        for v in ["lib,rlib", "lib,staticlib", "lib"] {
            assert!(lib(&["--crate-type", v]).cacheable, "{v}");
        }
        // an element must match whole
        assert!(lib(&["--crate-type", "libbin"]).cacheable);
        // several --crate-type flags: any linking one counts
        assert!(!lib(&["--crate-type", "lib", "--crate-type=cdylib"]).cacheable);
    }

    /// Q-RS-6: a glued `-L` stays in the key.
    #[test]
    fn q_rs_6_glued_search_path() {
        let inv = lib(&[
            "--crate-type=lib",
            "-Ldependency=/b/deps",
            "-L",
            "x",
            "-L=y",
        ]);
        assert!(has(&inv.key_args, "-Ldependency=/b/deps"));
        assert_eq!(
            inv.key_args.iter().filter(|a| a.starts_with(b"-L")).count(),
            1
        );
    }

    /// Q-RS-7: out-dir, metadata and extra-filename are not in the key.
    #[test]
    fn q_rs_7_not_in_key() {
        let a = lib(&[
            "--crate-type=lib",
            "-C",
            "metadata=1",
            "-C",
            "extra-filename=-1",
        ]);
        let b = parse(&[
            "--crate-name",
            "foo",
            "src/lib.rs",
            "--emit=dep-info,link",
            "--out-dir",
            "/elsewhere",
            "--crate-type=lib",
            "-C",
            "metadata=2",
            "-C",
            "extra-filename=-2",
        ]);
        assert_eq!(a.key_args, b.key_args);
        assert_ne!(a.extra_filename, b.extra_filename);
        assert_ne!(a.out_dir, b.out_dir);
    }
}
