//! The link-driver policy against `tests/vectors/ec-driver-policy.toml`
//! (`behaviors/ec-driver-policy.md §Test vectors`, `implementation/12-testing.md §L1: spec
//! vectors`). The file keeps the doc's notation (START, END, META, CRT_TAIL, `_×N`); see its
//! header for what each kind checks. TV-37's end-to-end rows run in `cc_mode.rs`.
#![allow(clippy::unwrap_used)]

use ekacache::driver::{
    BinFmt, DriverConf, Fatal, Language, PackageCcFlags, build_driver_args, is_shared_lib_name,
    package_flags, parse_driver_conf, with_package_lib_dirs,
};
use ekacache_core::store_path::STORE_DIR;
use ekacache_testkit::vectors::{Vector, VectorFile};

const UNDEFINED_VERSION: &[u8] = b"-Wl,--undefined-version";
const END: &[u8] = b"--end-no-unused-arguments";

fn vectors() -> VectorFile {
    VectorFile::load("ec-driver-policy")
}

/// The doc's notation expanded, with `subst` applied to each string first.
fn expand(items: &[&str], subst: &dyn Fn(&str) -> String) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for &item in items {
        match item {
            "START" => out.push(b"--start-no-unused-arguments".to_vec()),
            "END" => out.push(END.to_vec()),
            "META" => out.push(br#"-Wl,--package-metadata={"type":"repkgs"}"#.to_vec()),
            "CRT_TAIL" => out.extend(expand(
                &[
                    "-x",
                    "none",
                    "/cc/lib/crt_interp.o",
                    "-Wl,--dynamic-linker=/sr/libc/lib/././././././././././././ld-linux-x86-64.so.2",
                    "-Wl,--export-dynamic-symbol=__reloc_start",
                ],
                subst,
            )),
            _ => out.push(underscores(&subst(item)).into_bytes()),
        }
    }
    out
}

/// `_×N` inside a string: N underscores.
fn underscores(s: &str) -> String {
    match s.split_once("_×") {
        Some((head, n)) => {
            let digits: String = n.chars().take_while(char::is_ascii_digit).collect();
            let rest = &n[digits.len()..];
            format!("{head}{}{rest}", "_".repeat(digits.parse().unwrap()))
        }
        None => s.to_string(),
    }
}

fn store_dir() -> String {
    String::from_utf8(STORE_DIR.to_vec()).unwrap()
}

/// `/nix/store` in a vector stands for the store dir.
fn in_store(s: &str) -> String {
    s.replace("/nix/store", &store_dir())
}

fn joined(v: &[Vec<u8>]) -> String {
    String::from_utf8(ekacache_core::bytes::join(v, b" ")).unwrap()
}

fn conf_text<'a>(file: &'a VectorFile, v: &'a Vector) -> &'a str {
    let name = v.str("conf");
    if name.starts_with('{') {
        name
    } else {
        file.top.str(&format!("conf_{name}"))
    }
}

fn conf(file: &VectorFile, v: &Vector, subst: &dyn Fn(&str) -> String) -> DriverConf {
    let root = v.opt_bytes("self").unwrap_or(b"");
    let mut c = parse_driver_conf(conf_text(file, v).as_bytes(), root).unwrap();
    let flags = |key: &str| -> Vec<Vec<u8>> {
        v.opt_strs(key)
            .unwrap_or_default()
            .iter()
            .map(|s| subst(s).into_bytes())
            .collect()
    };
    c.package.cflags = flags("package_cflags");
    c.package.cxxflags = flags("package_cxxflags");
    c.package.ldflags = flags("package_ldflags");
    c
}

fn lang(v: &Vector) -> Language {
    match v.str("lang") {
        "c" => Language::C,
        "c++" => Language::Cxx,
        "fortran" => Language::Fortran,
        l => panic!("{}: unknown lang {l}", v.label()),
    }
}

/// Runs one `driver` or `driver-store-lib` vector; `input` fills in the placeholders of the
/// conf and arguments, `subst` those of the expectations.
fn check_driver(
    file: &VectorFile,
    v: &Vector,
    input: &dyn Fn(&str) -> String,
    subst: &dyn Fn(&str) -> String,
) {
    let at = v.label();
    let c = conf(file, v, input);
    let args = expand(&v.strs("args"), input);
    let prefix_map = v.opt_bytes("prefix_map").unwrap_or(b"");
    let out = build_driver_args(&c, lang(v), &args, prefix_map);
    let text = joined(&out);
    let mut checked = 0;
    if let Some(want) = v.opt_strs("out") {
        assert_eq!(joined(&expand(&want, subst)), text, "{at}");
        checked += 1;
    }
    for s in v.opt_strs("contains").unwrap_or_default() {
        assert!(text.contains(&subst(s)), "{at}: no {s:?} in {text}");
        checked += 1;
    }
    for s in v.opt_strs("lacks").unwrap_or_default() {
        assert!(!text.contains(&subst(s)), "{at}: {s:?} in {text}");
        checked += 1;
    }
    for a in expand(&v.opt_strs("has_arg").unwrap_or_default(), subst) {
        assert!(out.contains(&a), "{at}: no argument {a:?} in {text}");
        checked += 1;
    }
    if v.get("index").is_some() {
        let values = expand(&v.strs("index_values"), subst);
        for (i, want) in v.ints("index").into_iter().zip(values) {
            assert_eq!(out[i as usize], want, "{at}: out[{i}] of {text}");
            checked += 1;
        }
    }
    let undefined = out.iter().position(|a| a == UNDEFINED_VERSION);
    if let Some(want) = v.opt_strs("user") {
        let end = out.iter().position(|a| a == END).unwrap() + 1;
        let stop = undefined.unwrap_or(out.len());
        assert_eq!(
            joined(&out[end..stop]),
            joined(&expand(&want, subst)),
            "{at}"
        );
        checked += 1;
    }
    if let Some(want) = v.opt_str("rpath") {
        let i = undefined.unwrap_or_else(|| panic!("{at}: no {UNDEFINED_VERSION:?} in {text}"));
        assert_eq!(
            String::from_utf8_lossy(&out[i + 1]),
            underscores(&subst(want)),
            "{at}"
        );
        checked += 1;
    }
    if let Some(want) = v.opt_strs("ends_with") {
        let want = expand(&want, subst);
        assert!(out.ends_with(&want), "{at}: {text}");
        checked += 1;
    }
    assert!(checked > 0, "{at}: no expectation");
}

#[test]
fn conf_vectors() {
    let file = vectors();
    for v in file.of_kind("conf", 4) {
        let at = v.label();
        let c = conf(&file, v, &in_store);
        let mut checked = 0;
        if let Some(p) = v.opt_bool("present") {
            assert_eq!(c.present, p, "{at}");
            checked += 1;
        }
        checked += check_conf_fields(&c, v);
        assert!(checked > 0, "{at}: no expectation");
    }
}

/// The `cc`, `fc`, `flags`, `fflags`, `libc` and `binfmt` a vector gives; how many there were.
fn check_conf_fields(c: &DriverConf, v: &Vector) -> usize {
    let at = v.label();
    let mut checked = 0;
    for (key, got) in [("cc", &c.cc), ("fc", &c.fc), ("libc", &c.libc)] {
        if let Some(want) = v.opt_bytes(key) {
            assert_eq!(got, want, "{at}: {key}");
            checked += 1;
        }
    }
    for (key, got) in [("flags", &c.flags), ("fflags", &c.fflags)] {
        if v.get(key).is_some() {
            assert_eq!(got, &v.byte_strs(key), "{at}: {key}");
            checked += 1;
        }
    }
    if let Some(b) = v.opt_str("binfmt") {
        let want = match b {
            "elf" => BinFmt::Elf,
            "macho" => BinFmt::MachO,
            "coff" => BinFmt::Coff,
            _ => panic!("{at}: binfmt {b}"),
        };
        assert_eq!(c.binfmt, want, "{at}");
        checked += 1;
    }
    checked
}

#[test]
fn conf_parse_vectors() {
    for v in vectors().of_kind("conf-parse", 13) {
        let at = v.label();
        let text = match v.opt_bytes("text") {
            Some(t) => t.to_vec(),
            None => v.hex("text_hex"),
        };
        let got = parse_driver_conf(&text, b"");
        if v.opt_bool("abort") == Some(true) {
            assert_eq!(got, Err(Fatal::Abort), "{at}");
        } else if let Some(stderr) = v.opt_bytes("stderr") {
            assert_eq!(v.int("status"), 2, "{at}: Fatal::Exit exits 2");
            assert_eq!(got, Err(Fatal::Exit([stderr, b"\n"].concat())), "{at}");
        } else {
            let c = got.unwrap_or_else(|e| panic!("{at}: {e:?}"));
            assert!(check_conf_fields(&c, v) > 0, "{at}: no expectation");
        }
    }
}

/// TV-37's `PKGS_CC` rows at the library level, with R = `/r`.
#[test]
fn pkgs_cc_vectors() {
    let root = b"/r";
    for v in vectors().of_kind("pkgs-cc", 6) {
        let at = v.label();
        let text = v.str("pkgs_cc").replace("<R>", "/r");
        let got = package_flags(text.as_bytes(), root);
        if v.opt_bool("abort") == Some(true) {
            assert_eq!(got, Err(Fatal::Abort), "{at}");
        } else {
            assert!(v.bool("none"), "{at}: no expectation");
            assert_eq!(got, Ok(PackageCcFlags::default()), "{at}");
        }
    }
}

#[test]
fn driver_vectors() {
    let file = vectors();
    for v in file.of_kind("driver", 49) {
        let dir = tempfile::tempdir().unwrap();
        for name in v.opt_strs("tempdir_files").unwrap_or_default() {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let w = dir.path().to_str().unwrap().to_owned();
        let subst = |s: &str| in_store(s).replace("<W>", &w);
        check_driver(&file, v, &subst, &subst);
    }
}

/// TV-33 needs a real directory under the store dir; it runs only where that is writable.
#[test]
fn driver_store_lib_vectors() {
    let file = vectors();
    for v in file.of_kind("driver-store-lib", 1) {
        let store = store_dir();
        let Ok(dir) = tempfile::Builder::new()
            .prefix("ec-tv33-")
            .tempdir_in(&store)
        else {
            eprintln!("{store} not writable: skipped");
            return;
        };
        let z = dir.path().join("lib");
        std::fs::create_dir(&z).unwrap();
        std::fs::write(z.join("libz.so"), "").unwrap();
        let real = std::fs::canonicalize(&z).unwrap();
        let (z, real) = (z.display().to_string(), real.display().to_string());
        // the arguments spell <Z> as created, the rpath names the canonical directory
        check_driver(
            &file,
            v,
            &|s: &str| in_store(s).replace("<Z>", &z),
            &|s: &str| in_store(s).replace("<Z>", &real),
        );
    }
}

#[test]
fn shared_lib_name_vectors() {
    for v in vectors().of_kind("shared-lib-name", 7) {
        assert_eq!(
            is_shared_lib_name(v.bytes("name")),
            v.bool("shared"),
            "{}",
            v.label()
        );
    }
}

#[test]
fn search_dirs_vectors() {
    for v in vectors().of_kind("search-dirs", 5) {
        assert_eq!(
            String::from_utf8(with_package_lib_dirs(
                v.bytes("text"),
                &v.byte_strs("ldflags")
            ))
            .unwrap(),
            v.str("result"),
            "{}",
            v.label()
        );
    }
}
