//! The port of `legacy/jig/src/jig_test.cc` (`implementation/12-testing.md §L2`).
//!
//! One test per `Test<Name>` function there, named `test_<name>` in snake case. The tests of
//! private modules live next to them: `test_binary_image` (`src/fixup/image.rs`),
//! `test_mach_o_fixup` (`tests/fixup_macho.rs`), `test_go_cache` (`src/gocache.rs`) and
//! `test_nix_store` (`src/nix_store/hash.rs`). Expectations changed by an adopted row say
//! `// ADOPTED Q-…`.
//!
//! Legacy runs every test with `EKACACHE_STORE_IDENTITY=content`, `EKACACHE_STORE_ROOTS=<vendor root>` and
//! `out=<openssl root>` set before the `Store` singleton is built; here that is `store()`.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use ekacache::cc::{mentions_build_time, parse_invocation};
use ekacache::driver::{
    BinFmt, DriverConf, Language, PackageCcFlags, build_driver_args, is_shared_lib_name,
    parse_driver_conf, with_package_lib_dirs,
};
use ekacache::launcher::{Kind, classify};
use ekacache::process::{ProcEnv, RunResult, deterministic};
use ekacache::rustc::depinfo::parse_dep_info;
use ekacache::rustc::parse::parse_rust_invocation;
use ekacache::util::{expand_response_files, read_file, write_file};
use ekacache_core::bytes::{
    hex_encode, join, parse_uint, parse_uint_octal, replace_all, split, split_whitespace, trim,
};
use ekacache_core::env::Env;
use ekacache_core::hasher::{Hasher, hash_of};
use ekacache_core::keys::{RequestKey, Tool, key_environment, slot};
use ekacache_core::manifest::{self, Unstorable, parse_depfile};
use ekacache_core::store_path::{STORE_DIR, STORE_HASH_LENGTH, Store, is_store_path, mask_hashes};

fn store_dir() -> String {
    String::from_utf8(STORE_DIR.to_vec()).unwrap()
}

fn vendor_root() -> String {
    format!(
        "{}/0123456789abcdfghijklmnpqrsvwxyz-cargo-vendor",
        store_dir()
    )
}

fn out_root() -> String {
    format!("{}/9123456789abcdfghijklmnpqrsvwxyz-openssl", store_dir())
}

fn store() -> Store {
    Store::from_env(&Env::from_pairs(&[
        ("EKACACHE_STORE_IDENTITY", "content".to_string()),
        ("EKACACHE_STORE_ROOTS", vendor_root()),
        ("out", out_root()),
    ]))
}

fn v(items: &[&str]) -> Vec<Vec<u8>> {
    items.iter().map(|s| s.as_bytes().to_vec()).collect()
}

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn s(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn joined(args: &[Vec<u8>]) -> String {
    s(&join(args, b" "))
}

fn has(args: &[Vec<u8>], arg: &str) -> bool {
    args.iter().any(|a| a == arg.as_bytes())
}

fn parse(args: &[&str]) -> ekacache::cc::Invocation {
    parse_invocation(&v(args), &ProcEnv::new(Env::default()))
}

fn path_bytes(p: &Path) -> Vec<u8> {
    p.as_os_str().as_encoded_bytes().to_vec()
}

/// A fresh directory under the temp dir, removed at the end of the test.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// jig_test.cc:49
#[test]
fn test_base() {
    assert_eq!(split_whitespace(b"  a  b\tc\n"), v(&["a", "b", "c"]));
    assert_eq!(split(b"a::b:", b':', false), v(&["a", "b"]));
    assert_eq!(join(&v(&["a", "b"]), b", "), b"a, b");
    assert_eq!(trim(b"  x \t"), b"x");
    assert_eq!(replace_all(b"a-b-c", b"-", b"+"), b"a+b+c");
    assert_eq!(parse_uint(b"42"), Some(42));
    assert_eq!(parse_uint_octal(b"755"), Some(0o755));
    assert_eq!(parse_uint(b"4x"), None);
    assert_eq!(parse_uint(b""), None);
    assert_eq!(hex_encode(b"\x01\xff"), b"01ff");
    // BLAKE3 test vector: empty input
    assert_eq!(
        Hasher::new().finish().hex(),
        b"af1349b9f5f9a1a6a0404dea36dcc949"
    );
    let mut left = Hasher::new();
    let mut right = Hasher::new();
    left.field(b"ab").field(b"c");
    right.field(b"a").field(b"bc");
    assert_ne!(left.finish(), right.finish());
    assert!(!write_file(b"/nonexistent/dir/file", b"x"));
    let dir = Scratch::new("ec-test-base");
    let big = vec![b'y'; 300_000];
    let file = path_bytes(&dir.0.join("big"));
    assert!(write_file(&file, &big));
    assert_eq!(read_file(&file), Some(big));
    let run = |status, text: &str| RunResult {
        status,
        captured: b(text),
    };
    assert!(deterministic(&run(1, "x.c:1: error: foo")));
    assert!(!deterministic(&run(137, "")));
    assert!(!deterministic(&run(
        1,
        "clang: error: unable to execute command: Killed"
    )));
}

// jig_test.cc:78
#[test]
fn test_store_mask() {
    let store = store();
    let dir = store_dir();
    assert!(is_store_path(format!("{dir}/x").as_bytes()));
    assert!(!is_store_path(dir.as_bytes()));
    assert!(!is_store_path(b"/tmp/x"));
    let header = format!("{dir}/0123456789abcdfghijklmnpqrsvwxyz-glibc-2.44/include/stdio.h");
    let masked_header = s(&mask_hashes(header.as_bytes()));
    assert_eq!(masked_header, format!("{dir}/*-glibc-2.44/include/stdio.h"));
    assert_eq!(
        s(&mask_hashes(format!("-I{header} -I{header}").as_bytes())),
        format!("-I{masked_header} -I{masked_header}")
    );
    let short = format!("{dir}/short-name");
    assert_eq!(s(&mask_hashes(short.as_bytes())), short);
    // byte 32 after '*' is '-'
    let once = format!("{dir}/*-linux-headers-boot/include/asm-generic/errno.h");
    assert_eq!(s(&mask_hashes(once.as_bytes())), once);
    // the own output's hash is a fixed placeholder in what is keyed and stored, and comes back
    let define = format!("-DENGINESDIR=\"{}/lib/engines\"", out_root());
    let masked = store.mask_out(define.as_bytes()).into_owned();
    assert_eq!(
        s(&masked),
        format!("-DENGINESDIR=\"{dir}/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-openssl/lib/engines\"")
    );
    assert_eq!(s(&store.unmask_out(&masked)), define);
    assert_eq!(s(&store.mask_out(header.as_bytes())), header);
    // a literal run of 'e' that is not a store path stays
    let pad = format!("char pad[] = \"{}\";", "e".repeat(40));
    assert_eq!(s(&store.unmask_out(pad.as_bytes())), pad);
}

// jig_test.cc:103: Key(): lexical path normalisation for path-valued args, other text untouched
#[test]
fn test_store_key() {
    let store = store();
    let dir = store_dir();
    let key = |arg: &str| s(&store.key(arg.as_bytes()));
    assert_eq!(key("-I./include//sub/../"), "-Iinclude");
    assert_eq!(
        key(&format!("-I{dir}/h-x/include/.")),
        s(&mask_hashes(format!("-I{dir}/h-x/include").as_bytes()))
    );
    assert_eq!(key("./src/../src/a.c"), "src/a.c");
    assert_eq!(key("--sysroot=/a/b/../c/"), "--sysroot=/a/c");
    // cmake's random probe names collapse to one key
    assert_eq!(
        key("/b/CMakeFiles/CMakeScratch/TryCompile-teSZ4z/src.c"),
        key("/b/CMakeFiles/CMakeScratch/TryCompile-EaIIUC/src.c")
    );
    assert_eq!(
        key("CMakeFiles/cmTC_7c7e5.dir/x.c.o"),
        "CMakeFiles/cmTC_#####.dir/x.c.o"
    );
    assert_eq!(key("-O2"), "-O2");
    assert_eq!(key("-DFOO=./a//b"), "-DFOO=./a//b");
    assert_eq!(key("-std=c++23"), "-std=c++23");
    assert_eq!(key("."), ".");
}

// jig_test.cc:120
#[test]
fn test_store_resolve() {
    let store = store();
    let dir = store_dir();
    // the vendor root is in $EKACACHE_STORE_ROOTS (main), an unrelated root is not
    let vendor = vendor_root();
    let vendored = format!("{vendor}/x-1.0/src/util.rs");
    assert_eq!(
        store
            .resolve(&mask_hashes(vendored.as_bytes()))
            .map(|p| s(&p)),
        Some(vendored)
    );
    assert!(
        store
            .resolve(format!("{dir}/*-elsewhere/f.h").as_bytes())
            .is_none()
    );
    assert_eq!(
        store.resolve(b"/tmp/f.h").map(|p| s(&p)),
        Some("/tmp/f.h".to_string())
    );
    // a config text from another build: its root becomes ours, quoted or not, longer names untouched
    let dash = vendor.find('-').unwrap();
    let other = format!("{dir}/{}{}", "a".repeat(STORE_HASH_LENGTH), &vendor[dash..]);
    let text = format!("p=\"{other}/x\" q={other}-ng/y");
    assert_eq!(
        s(&store.resolve_all(text.as_bytes())),
        format!(
            "p=\"{vendor}/x\" q={}-ng/y",
            s(&mask_hashes(other.as_bytes()))
        )
    );
}

// jig_test.cc:136: two packages whose bin/rustc link to one launcher: distinct ids, the launcher
// not in them
#[test]
fn test_store_tool_id() {
    let store = store();
    assert_eq!(store.tool_id(b"/no/such/tool"), b"/no/such/tool");
    let tmp = Scratch::new("ec-toolid");
    let tmp = &tmp.0;
    for pkg in ["rust-a", "rust-b"] {
        std::fs::create_dir_all(tmp.join(pkg).join("bin")).unwrap();
        std::os::unix::fs::symlink(tmp.join("launch"), tmp.join(pkg).join("bin/rustc")).unwrap();
    }
    std::fs::write(tmp.join("launch"), "").unwrap();
    std::os::unix::fs::symlink(tmp.join("rust-a"), tmp.join("alias")).unwrap();
    let id = |p: &str| store.tool_id(&path_bytes(&tmp.join(p)));
    assert_eq!(
        id("rust-a/bin/rustc"),
        path_bytes(&tmp.join("rust-a/bin/rustc"))
    );
    assert_ne!(id("rust-a/bin/rustc"), id("rust-b/bin/rustc"));
    assert_eq!(id("alias/bin/rustc"), id("rust-a/bin/rustc"));
}

// jig_test.cc:153: cgo writes the joined -o form
#[test]
fn test_parse_joined_output() {
    let inv = parse(&["-c", "foo.c", "-o/tmp/b/x.o"]);
    assert!(inv.cacheable && inv.output == b"/tmp/b/x.o" && inv.key_args == v(&["-c"]));
}

// jig_test.cc:160: ghc hands cc everything in one @rsp: -shared in there must still count (no
// crt_interp.o). Words follow the GNU quoting clang reads
#[test]
fn test_response_files() {
    let dir = Scratch::new("ec-rsp-test");
    let rsp = dir.0.join("a.rsp");
    assert!(write_file(
        &path_bytes(&rsp),
        b"-shared '-o' 'lib sp.so'\nx.o y\\ z.o \"q\\\"\"\n"
    ));
    let mut at = b"@".to_vec();
    at.extend_from_slice(&path_bytes(&rsp));
    let got = expand_response_files(&[b"-O".to_vec(), at, b"@missing".to_vec()]);
    assert_eq!(
        got,
        v(&[
            "-O",
            "-shared",
            "-o",
            "lib sp.so",
            "x.o",
            "y z.o",
            "q\"",
            "@missing"
        ])
    );
}

// jig_test.cc:170
#[test]
fn test_parse_link() {
    let inv = parse(&["-o", "prog", "main.o", "libutil.a", "-lz", "-shared"]);
    assert!(inv.cacheable && inv.link && !inv.link_one);
    assert!(inv.output == b"prog" && inv.source == b"prog");
    assert_eq!(inv.inputs, v(&["main.o", "libutil.a"]));
    assert!(!parse(&["-o", "prog", "-Wl,@objs.rsp"]).cacheable);
    assert!(!parse(&["-shared", "-o", "x.so"]).cacheable);
    let inv = parse(&["-r", "-o", "m.o", "a.os", "-Wl,-Map,m.mapT"]);
    assert!(inv.link && !inv.cacheable);
    // a "does this flag compile" probe: nothing to store, and no temp files beside /dev/null
    assert!(!parse(&["-mabi=lp64d", "-c", "cpuid.c", "-o", "/dev/null"]).cacheable);
}

// jig_test.cc:189
#[test]
fn test_parse_invocation() {
    let inv = parse(&[
        "-O2",
        "-c",
        "foo.c",
        "-o",
        "out/foo.o",
        "-MD",
        "-MF",
        "out/foo.d",
        "-MT",
        "x",
    ]);
    assert!(inv.cacheable && inv.compile_only && !inv.link_one);
    assert!(inv.source == b"foo.c" && inv.output == b"out/foo.o");
    assert!(inv.wants_depfile && inv.depfile == b"out/foo.d");
    assert_eq!(inv.key_args, v(&["-O2", "-c"]));

    let inv = parse(&["-c", "dir/foo.c"]);
    assert!(inv.cacheable && inv.output == b"foo.o" && !inv.wants_depfile);

    let inv = parse(&["-c", "foo.c", "-MD"]);
    assert!(inv.wants_depfile && inv.depfile == b"foo.d");

    let inv = parse(&["-Wp,-MMD,scripts/.fixdep.d", "-o", "fixdep", "fixdep.c"]);
    assert!(inv.cacheable && inv.link_one && inv.wants_depfile);
    assert!(inv.depfile == b"scripts/.fixdep.d" && inv.output == b"fixdep");

    let inv = parse(&["-O2", "conftest.c"]);
    assert!(inv.cacheable && inv.link_one && inv.output == b"a.out");

    assert!(!parse(&["-o", "conftest", "conftest.c", "conftstm.o"]).cacheable);
    assert!(!parse(&["-shared", "-o", "lib.so", "a.c"]).cacheable);
    for flag in ["-M", "--version", "-print-search-dirs", "-fsyntax-only"] {
        assert!(!parse(&[flag, "conftest.c"]).cacheable, "{flag}");
    }
    assert!(!parse(&["-c", "a.c", "b.c"]).cacheable);
    assert!(!parse(&["-c", "-x", "c", "-"]).cacheable);
}

// jig_test.cc:225
#[test]
fn test_search_dirs() {
    let clang = b"programs: =/cc/bin\nlibraries: =/cc/lib/clang/23:/sysroot/lib\n";
    let ldflags = v(&["-L/deps/zlib/lib", "-Wl,-z,now", "-L/deps/xz/lib"]);
    assert_eq!(
        s(&with_package_lib_dirs(clang, &ldflags)),
        "programs: =/cc/bin\nlibraries: =/cc/lib/clang/23:/sysroot/lib:/deps/zlib/lib:/deps/xz/lib\n"
    );
    assert_eq!(with_package_lib_dirs::<Vec<u8>>(clang, &[]), clang);
}

// jig_test.cc:234
#[test]
fn test_parse_pch() {
    assert!(!parse(&["-x", "c++-header", "pch.hxx", "-o", "pch.hxx.pch", "-c"]).cacheable);
    assert!(
        !parse(&[
            "-Xclang",
            "-emit-pch",
            "-c",
            "cmake_pch.hxx.cxx",
            "-o",
            "cmake_pch.hxx.pch"
        ])
        .cacheable
    );
    let inv = parse(&["-include-pch", "x.pch", "-c", "a.cc", "-o", "a.o"]);
    assert!(inv.cacheable && inv.pch == v(&["x.pch"]));
    let inv = parse(&[
        "-Xclang",
        "-include-pch",
        "-Xclang",
        "/b/x.pch",
        "-Xclang",
        "-include",
        "-Xclang",
        "/b/x.hxx",
        "-c",
        "a.cc",
    ]);
    assert!(inv.cacheable && inv.pch == v(&["/b/x.pch"]) && inv.source == b"a.cc");
}

// jig_test.cc:246: configure's preprocessor probes: cached like a compile, -E/-S part of the
// key, text to stdout without -o
#[test]
fn test_parse_preprocess() {
    let inv = parse(&["-std=gnu23", "-E", "conftest.c"]);
    assert!(inv.cacheable && inv.compile_only && inv.to_stdout);
    assert_eq!(inv.key_args, v(&["-std=gnu23", "-E"]));
    let inv = parse(&["-E", "conftest.c", "-o", "-"]);
    assert!(inv.cacheable && inv.to_stdout);
    let inv = parse(&["-E", "-o", "x.i", "x.c"]);
    assert!(inv.cacheable && !inv.to_stdout && inv.output == b"x.i");
    let inv = parse(&["-S", "x.c"]);
    assert!(inv.cacheable && !inv.to_stdout && inv.output == b"x.s");
    assert_eq!(inv.key_args, v(&["-S"]));
    assert!(!parse(&["-E", "-"]).cacheable);
}

// jig_test.cc:259
#[test]
fn test_depfile() {
    let text =
        b"out/foo.o: foo.c \\\n  /inc/a.h /inc/b.h \\\n /inc/sp\\ ace.h\n/inc/a.h:\n/inc/b.h:\n";
    assert_eq!(
        parse_depfile(text),
        v(&["foo.c", "/inc/a.h", "/inc/b.h", "/inc/sp ace.h"])
    );
    assert_eq!(parse_depfile(b"x: \\\n a.c\n"), v(&["a.c"]));
    assert!(parse_depfile(b"").is_empty());
    // lld --dependency-file
    assert_eq!(
        parse_depfile(
            b"conftest: \\\n /s/lib/Scrt1.o \\\n /tmp/conftest-1.o \\\n /s/lib/libc.so\n\n/s/lib/libc.so:\n"
        ),
        v(&["/s/lib/Scrt1.o", "/tmp/conftest-1.o", "/s/lib/libc.so"])
    );
}

// jig_test.cc:270
#[test]
fn test_manifest() {
    let store = store();
    let scratch = Scratch::new("ec-test");
    let dir = scratch.0.to_str().unwrap().to_string();
    std::fs::write(format!("{dir}/a.h"), "A").unwrap();
    std::fs::write(format!("{dir}/b.h"), "B").unwrap();
    let key = RequestKey::new(Tool::Cc, &hash_of(b"k1"));
    let other = RequestKey::new(Tool::Cc, &hash_of(b"other"));
    assert_eq!(
        RequestKey::new(Tool::Rustc, &hash_of(b"k1")).text(),
        [b"rs/".as_slice(), key.text()].concat()
    );
    // unconnected: identities are hashed locally
    let mut offline = |_: &[&[u8]]| Vec::new();
    let (a, bh) = (format!("{dir}/a.h"), format!("{dir}/b.h"));
    // ADOPTED Q-KEY-3: legacy drops the line of an input without an identity; the port refuses
    // to store the result
    let none: [&str; 0] = [];
    assert_eq!(
        manifest::build(
            &store,
            &key,
            &["src.c", &a, &bh, "/nonexistent"],
            b"src.c",
            &none,
            &mut offline
        ),
        Err(Unstorable::Unreadable(b("/nonexistent")))
    );
    let built = manifest::build(
        &store,
        &key,
        &["src.c", &a, &bh],
        b"src.c",
        &none,
        &mut offline,
    )
    .unwrap();
    assert!(built.text.starts_with(format!("{dir}/a.h\tC:").as_bytes()));
    assert_eq!(split(&built.text, b'\n', false).len(), 2);
    let validate = |k: &RequestKey, text: &[u8]| {
        manifest::validate(&store, k, text, &mut |_: &[&[u8]]| Vec::new())
    };
    assert_eq!(validate(&key, &built.text), Ok(built.result_key.clone()));
    assert_ne!(validate(&other, &built.text), Ok(built.result_key.clone()));
    assert_eq!(
        slot::manifest(&key),
        [b"m/".as_slice(), key.text()].concat()
    );
    assert_eq!(
        slot::object(&built.result_key),
        [b"o/".as_slice(), built.result_key.text()].concat()
    );
    std::fs::write(&bh, "B2").unwrap();
    assert_eq!(
        validate(&key, &built.text),
        Err(b(&format!("inputs-changed:{dir}/b.h")))
    );

    // absent lookups: a hit needs them still absent. Existing (the compiler's own output) and
    // store paths drop out
    let shadow = manifest::build(
        &store,
        &key,
        &["src.c", &a],
        b"src.c",
        &[
            format!("{dir}/early/a.h"),
            format!("{dir}/./early//a.h"),
            format!("{dir}/b.h"),
            "/nix/store/x-y/z.h".to_string(),
            String::new(),
        ],
        &mut offline,
    )
    .unwrap();
    let first = split(&built.text, b'\n', false)[0].to_vec();
    assert_eq!(
        split(&shadow.text, b'\n', false),
        vec![first, b(&format!("!{dir}/early/a.h"))]
    );
    assert_eq!(validate(&key, &shadow.text), Ok(shadow.result_key.clone()));
    std::fs::create_dir_all(format!("{dir}/early")).unwrap();
    std::fs::write(format!("{dir}/early/a.h"), "A2").unwrap();
    assert_eq!(
        validate(&key, &shadow.text),
        Err(b(&format!("appeared:{dir}/early/a.h")))
    );
}

const ELF_CONF: &str = r#"{"cc": "/seed/bin/clang", "flags": ["--target=x", "-O2"], "cxxflags": ["-stdlib=libc++"], "libc": "/sr/libc", "crt": "/cc/lib/crt_interp.o", "runtimes": "/sr/rt/lib"}"#;
const COFF_CONF: &str =
    r#"{"cc": "/seed/bin/clang", "binfmt": "coff", "flags": ["--target=x86_64-pc-windows-msvc"]}"#;

fn conf(text: &str) -> DriverConf {
    parse_driver_conf(text.as_bytes(), b"").unwrap()
}

fn driver(conf: &DriverConf, lang: Language, args: &[&str]) -> Vec<Vec<u8>> {
    build_driver_args(conf, lang, &v(args), b"")
}

// jig_test.cc:318: conf parsing, and binfmt: macho gets rpaths and @rpath ids but no interp, coff
// a c++14 floor and no PIC
#[test]
fn test_driver_conf() {
    let elf = conf(ELF_CONF);
    assert!(elf.present && elf.cc == b"/seed/bin/clang" && elf.flags == v(&["--target=x", "-O2"]));
    let fortran = parse_driver_conf(
        br#"{"cc": "/c", "fc": "/f/bin/flang", "fflags": ["-L@/lib", "-fintrinsic-modules-path", "@/finc", "-resource-dir=@/rd", "-Da@/b"]}"#,
        b"/self",
    )
    .unwrap();
    assert_eq!(fortran.fc, b"/f/bin/flang");
    assert_eq!(
        fortran.fflags,
        v(&[
            "-L/self/lib",
            "-fintrinsic-modules-path",
            "/self/finc",
            "-resource-dir=/self/rd",
            "-Da@/b"
        ])
    );
    let macho = conf(
        r#"{"cc": "/seed/bin/clang", "binfmt": "macho", "flags": ["--target=arm64-apple-macos14.0"], "libc": "/sr"}"#,
    );
    assert_eq!(macho.binfmt, BinFmt::MachO);
    let macho_exe = joined(&driver(&macho, Language::C, &["a.c", "-o", "a"]));
    assert!(
        !macho_exe.contains("dynamic-linker")
            && !macho_exe.contains("crt_interp")
            && !macho_exe.contains("$ORIGIN")
    );
    assert!(
        macho_exe.contains("-Wl,-headerpad_max_install_names") && !macho_exe.contains("-rpath")
    );
    // COFF: PIC is not a thing to ask for
    let coff = conf(COFF_CONF);
    assert!(!has(
        &driver(&coff, Language::C, &["-fPIC", "-c", "a.c"]),
        "-fPIC"
    ));
    assert!(has(
        &driver(&coff, Language::Cxx, &["-std=c++11", "-c", "a.cc"]),
        "-std=c++14"
    ));
    assert!(has(
        &driver(&coff, Language::Cxx, &["-std=gnu++17", "-c", "a.cc"]),
        "-std=gnu++17"
    ));
    assert!(has(
        &driver(&macho, Language::Cxx, &["-std=c++11", "-c", "a.cc"]),
        "-std=c++11"
    ));
    assert!(!joined(&driver(&coff, Language::C, &["-c", "a.c"])).contains("build-id"));

    // compile: conf flags, no link policy
    let out = driver(&elf, Language::C, &["-c", "a.c"]);
    assert_eq!(
        out,
        v(&[
            "--start-no-unused-arguments",
            "--target=x",
            "-O2",
            "-Wl,--build-id=sha1",
            r#"-Wl,--package-metadata={"type":"repkgs"}"#,
            "--end-no-unused-arguments",
            "-c",
            "a.c"
        ])
    );
    // C++ name adds driver mode + cxxflags
    let out = driver(&elf, Language::Cxx, &["-c", "a.cc"]);
    assert!(out[3] == b"--driver-mode=g++" && out[4] == b"-stdlib=libc++");

    // -lWS2_32 names a lower-case sysroot import lib, -lLLVM-22 a mixed-case one on a -L dir
    let wlib = Scratch::new("ec-coff");
    let wlib_dir = wlib.0.to_str().unwrap();
    std::fs::write(wlib.0.join("libLLVM-22.dll.a"), "").unwrap();
    let out = driver(
        &coff,
        Language::C,
        &["a.o", &format!("-L{wlib_dir}"), "-lWS2_32", "-lLLVM-22"],
    );
    assert!(has(&out, "-lws2_32"));
    assert!(has(&out, "-lLLVM-22"));
    assert!(has(
        &driver(
            &coff,
            Language::C,
            &["a.o", "-nostartfiles", "-lWS2_32.lib"]
        ),
        "-lws2_32.lib"
    ));
    let out = driver(&coff, Language::C, &["a.o", "-L", wlib_dir, "-lLLVM-22"]);
    assert!(has(&out, "-lLLVM-22"));
}

// jig_test.cc:367: package flags: after the toolchain's, before the build system's. ldflags only
// when linking
#[test]
fn test_driver_package_flags() {
    let mut pkg = conf(ELF_CONF);
    pkg.package = PackageCcFlags {
        cflags: v(&["-O3"]),
        cxxflags: v(&["-fno-rtti"]),
        ldflags: v(&["-Wl,-z,x"]),
    };
    let out = driver(&pkg, Language::Cxx, &["-c", "a.cc", "-O0"]);
    assert_eq!(
        out,
        v(&[
            "--start-no-unused-arguments",
            "--target=x",
            "-O2",
            "-O3",
            "--driver-mode=g++",
            "-stdlib=libc++",
            "-fno-rtti",
            "-Wl,--build-id=sha1",
            r#"-Wl,--package-metadata={"type":"repkgs"}"#,
            "--end-no-unused-arguments",
            "-c",
            "a.cc",
            "-O0"
        ])
    );
    let out = driver(&pkg, Language::C, &["-shared", "-o", "x.so", "x.o", "-L."]);
    assert!(joined(&out).contains("x.o -L. -Wl,-z,x -Wl,"));
    // the user's --build-id comes later and wins
    let out = driver(&pkg, Language::C, &["-Wl,--build-id=none", "x.o"]);
    assert!(joined(&out).contains(
        r#"-Wl,--package-metadata={"type":"repkgs"} --end-no-unused-arguments -Wl,--build-id=none x.o"#
    ));
    pkg.package.cflags = v(&["-O2", "-D_FORTIFY_SOURCE=3"]);
    assert!(joined(&driver(&pkg, Language::C, &["-c", "a.c"])).contains("FORTIFY"));
    assert!(!joined(&driver(&pkg, Language::C, &["-c", "a.c", "-O0"])).contains("FORTIFY"));
    assert!(!joined(&driver(&pkg, Language::C, &["-c", "a.c", "-O2", "-O0"])).contains("=3"));
    let out = joined(&driver(
        &pkg,
        Language::C,
        &["-c", "a.c", "-D_FORTIFY_SOURCE=2"],
    ));
    assert!(!out.contains("=3") && out.contains("=2"));
}

// jig_test.cc:390: executable link: rpath (runtimes only for C++), interp stub, host rpaths and
// foreign --dynamic-linker dropped. shared: rpath, no interp. static / -r / -nostartfiles: nothing
#[test]
fn test_driver_link() {
    let elf = conf(ELF_CONF);
    let store = store_dir();
    let out = joined(&driver(
        &elf,
        Language::C,
        &[
            "-o",
            "x",
            "x.c",
            "-Wl,-rpath,/usr/lib:/build/lib",
            "-Wl,--dynamic-linker=/lib/ld.so",
        ],
    ));
    assert!(!out.contains("/usr/lib"));
    assert!(!out.contains("/lib/ld.so "));
    assert!(out.contains("-Wl,-rpath,/build/lib:/sr/libc/lib/.:/_"));
    assert!(!out.contains("/sr/rt/lib"));
    // the interpreter's name is the host's (legacy asserts the x86-64 one)
    #[cfg(target_arch = "x86_64")]
    assert!(out.contains(
        "-x none /cc/lib/crt_interp.o -Wl,--dynamic-linker=/sr/libc/lib/././././././././././././ld-linux-x86-64.so.2 -Wl,--export-dynamic-symbol=__reloc_start"
    ));
    // cmake links with "-rpath,<build>:" and its install step insists on finding "<build>:" verbatim
    let out = joined(&driver(
        &elf,
        Language::C,
        &["-o", "x", "x.c", "-Wl,-rpath,/build/build:"],
    ));
    assert!(out.contains("-Wl,-rpath,/build/build::/sr/libc/lib/.:/_"));
    let out = joined(&driver(&elf, Language::Cxx, &["-o", "x", "x.cc"]));
    assert!(out.contains("/sr/rt/lib/.:/sr/libc/lib/.:/_"));
    let out = joined(&driver(&elf, Language::C, &["-o", "x", "x.c", "-lc++"]));
    assert!(out.contains("/sr/rt/lib/.:/sr/libc/lib/.:/_"));
    let out = joined(&driver(
        &elf,
        Language::C,
        &["-shared", "-o", "l.so", "l.c"],
    ));
    assert!(out.contains("-rpath") && !out.contains("crt_interp"));
    for flag in ["-static", "-static-pie", "-r", "-nostartfiles"] {
        let out = joined(&driver(&elf, Language::C, &[flag, "-o", "x", "x.c"]));
        assert!(!out.contains("-rpath"), "{flag}");
    }
    // store .so by path -> its dir is rpath'd
    let lib = format!("{store}/h-zlib/lib/libz.so.1");
    let out = joined(&driver(&elf, Language::C, &["-o", "x", "x.c", &lib]));
    assert!(out.contains(&format!("-rpath,{store}/h-zlib/lib/.:/sr/libc/lib/.:/_")));
    assert!(is_shared_lib_name(b"libz.so") && is_shared_lib_name(b"libz.so.1.3"));
    assert!(!is_shared_lib_name(b"libz.son") && !is_shared_lib_name(b"x.o"));
}

// jig_test.cc:425: cargo runs rustc from the workspace root and dep-info names module files
// relative to it
#[test]
fn test_dep_info() {
    let cwd = std::env::current_dir().unwrap();
    let cwd = cwd.to_str().unwrap();
    let info = parse_dep_info(
        b"out/t.d: src/lib.rs src/m.rs\n\nout/libt.rlib: src/lib.rs src/m.rs\n\nsrc/lib.rs:\nsrc/m.rs:\n# env-dep:X\n",
        cwd.as_bytes(),
    );
    assert_eq!(info.outputs, v(&["out/t.d", "out/libt.rlib"]));
    assert_eq!(
        info.inputs,
        v(&[&format!("{cwd}/src/lib.rs"), &format!("{cwd}/src/m.rs")])
    );
}

// jig_test.cc:433
#[test]
fn test_rust_invocation() {
    let inv = parse_rust_invocation(&v(&[
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
    ]));
    assert!(inv.cacheable && inv.source == b"src/lib.rs" && inv.crate_name == b"foo");
    assert!(inv.out_dir == b"/b/deps" && inv.extra_filename == b"-abcd");
    assert_eq!(inv.externs, v(&["/b/deps/libbar-1.rmeta"]));
    assert!(has(&inv.key_args, "--crate-type=lib"));
    assert!(has(&inv.key_args, "--cap-lints=allow"));
    assert!(!has(&inv.key_args, "-C=metadata=abcd"));
    assert!(!inv.links);
    let inv = parse_rust_invocation(&v(&[
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
    ]));
    assert!(inv.links && !inv.cacheable);
    let inv = parse_rust_invocation(&v(&["-", "--crate-type", "lib"]));
    assert!(!inv.cacheable && inv.query);
    let inv = parse_rust_invocation(&v(&[
        "-",
        "--crate-name",
        "___",
        "--print=file-names",
        "--crate-type",
        "bin",
    ]));
    assert!(inv.query);
}

// jig_test.cc:660
#[test]
fn test_classify_launch() {
    for name in [
        "cc",
        "c++",
        "gcc",
        "g++",
        "clang",
        "clang++",
        "gcc-15",
        "clang++-21.1",
        "x86_64-unknown-linux-gnu-gcc",
        "aarch64-unknown-linux-gnu-g++",
        "x86_64-w64-mingw32-cc",
    ] {
        assert_eq!(classify(name.as_bytes()), Kind::Cc, "{name}");
    }
    assert_eq!(classify(b"rustc"), Kind::Rustc);
    assert_eq!(classify(b"go"), Kind::Go);
    for name in [
        "cpp",
        "gfortran",
        "gccgo",
        "gcc-ar",
        "clang-tidy",
        "ld",
        "rustdoc",
        "gofmt",
        "xcc",
    ] {
        assert_eq!(classify(name.as_bytes()), Kind::Other, "{name}");
    }
}

// jig_test.cc:673
#[test]
fn test_key_environment() {
    let env = [
        "PATH=/bin",
        "NIX_HARDENING_ENABLE=bindnow relro",
        "NIX_BUILD_TOP=/build",
        "CPATH=/inc",
        "NIX_LDFLAGS=-rpath /nix/store/x/lib",
        "NIX_LOG_FD=2",
        "NIX_ATTRS_SH_FILE=/build/.attrs.sh",
    ];
    assert_eq!(key_environment(env, false), b"CPATH=/inc\n");
    assert_eq!(
        key_environment(env, true),
        b"CPATH=/inc\nNIX_HARDENING_ENABLE=bindnow relro\nNIX_LDFLAGS=-rpath /nix/store/x/lib\n"
    );
    assert!(mentions_build_time(b"const char* d = __DATE__;"));
    assert!(!mentions_build_time(b"int x;"));
}

// jig_test.cc:684
#[test]
fn test_parse_guards() {
    assert!(!parse(&["-cc1", "-emit-obj", "-o", "a.o", "a.c"]).cacheable);
    assert!(!parse(&["-fuse-ld=gold", "a.o", "-o", "a"]).cacheable);
    assert!(parse(&["-fuse-ld=bfd", "a.o", "-o", "a"]).cacheable);
}
