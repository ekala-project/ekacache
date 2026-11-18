//! `BuildDriverArgs` and `ScanUserArgs` (`behaviors/jig-driver-policy.md` A1–A5): conf flags
//! first, the build system's rpaths taken over, the RUNPATH/interp policy appended when linking.

use jig_core::bytes::split;

use super::conf::{BinFmt, DriverConf};
use super::elf::{RunpathList, elf_link};
use super::{Language, coff, macho};

/// `UserArgs`: the user args minus what the policy took, and what the scan detected.
#[derive(Clone, Debug)]
pub struct UserArgs {
    pub args: Vec<Vec<u8>>,
    /// Seeded with the build system's rpaths (ELF).
    pub runpath: RunpathList,
    pub linking: bool,
    pub have_input: bool,
    pub shared: bool,
    pub no_policy: bool,
    pub optimizes: bool,
    pub sets_fortify: bool,
}

/// `TakeRpathArg(args, i)`: the value of a `-Wl,-rpath` in the spellings build systems use;
/// advances `i` past a separate value.
fn take_rpath_arg(args: &[Vec<u8>], i: &mut usize) -> Option<Vec<u8>> {
    let arg = &args[*i][..];
    if (arg == b"-Wl,-rpath" || arg == b"-Wl,--rpath")
        && args.get(*i + 1).is_some_and(|n| n.starts_with(b"-Wl,"))
    {
        *i += 1;
        return Some(args[*i][4..].to_vec());
    }
    for spelling in [
        &b"-Wl,-rpath,"[..],
        b"-Wl,--rpath,",
        b"-Wl,-rpath=",
        b"-Wl,--rpath=",
    ] {
        // Q-DRV-5: the rest is taken whole, commas included
        if let Some(rest) = arg.strip_prefix(spelling) {
            return Some(rest.to_vec());
        }
    }
    if arg == b"-Xlinker"
        && *i + 3 < args.len()
        && (args[*i + 1] == b"-rpath" || args[*i + 1] == b"--rpath")
        && args[*i + 2] == b"-Xlinker"
    {
        *i += 3;
        return Some(args[*i].clone());
    }
    None
}

fn is_output_only_mode(arg: &[u8]) -> bool {
    matches!(
        arg,
        b"-c" | b"-E" | b"-S" | b"-M" | b"-MM" | b"-fsyntax-only" | b"-###"
    )
}

/// Static and partial links and anything refusing start files get no link policy (Q-DRV-14).
fn refuses_link_policy(arg: &[u8]) -> bool {
    matches!(
        arg,
        b"-static" | b"-static-pie" | b"-r" | b"-Wl,-r" | b"-nostartfiles"
    )
}

/// `IsFortifyArg` (A4).
fn is_fortify_arg(arg: &[u8]) -> bool {
    arg.starts_with(b"-D_FORTIFY_SOURCE")
        || arg == b"-U_FORTIFY_SOURCE"
        || arg.starts_with(b"-Wp,-D_FORTIFY_SOURCE")
        || arg.starts_with(b"-Wp,-U_FORTIFY_SOURCE")
}

/// `ElfArg`: the build system's dynamic linker request is dropped, ours wins (Q-DRV-6).
fn elf_arg(arg: &[u8]) -> Option<Vec<u8>> {
    let theirs =
        arg.starts_with(b"-Wl,--dynamic-linker") || arg.starts_with(b"-Wl,-dynamic-linker");
    (!theirs).then(|| arg.to_vec())
}

/// `PolicyFor(binfmt).arg`.
fn policy_arg(binfmt: BinFmt, arg: &[u8]) -> Option<Vec<u8>> {
    match binfmt {
        BinFmt::Elf => elf_arg(arg),
        BinFmt::MachO => Some(arg.to_vec()),
        BinFmt::Coff => coff::coff_arg(arg),
    }
}

/// `ScanUserArgs(raw, policy)` (A3).
fn scan_user_args(raw: &[Vec<u8>], binfmt: BinFmt) -> UserArgs {
    let mut user = UserArgs {
        args: Vec::new(),
        runpath: RunpathList::default(),
        linking: true,
        have_input: false,
        shared: false,
        no_policy: false,
        optimizes: true,
        sets_fortify: false,
    };
    let mut i = 0;
    while i < raw.len() {
        if binfmt == BinFmt::Elf
            && let Some(rpath) = take_rpath_arg(raw, &mut i)
        {
            user.runpath.add_verbatim(&rpath);
            i += 1;
            continue;
        }
        let arg = &raw[i][..];
        // the C++ also records FlagValue(raw, i, "-o"), which nothing reads (A3 step 3)
        if let Some(kept) = policy_arg(binfmt, arg) {
            user.linking = user.linking && !is_output_only_mode(arg);
            user.shared = user.shared || arg == b"-shared" || arg == b"-dynamiclib";
            user.no_policy = user.no_policy || refuses_link_policy(arg);
            // Q-DRV-3: any argument not starting with '-'
            user.have_input = user.have_input || arg.first() != Some(&b'-');
            user.sets_fortify = user.sets_fortify || is_fortify_arg(arg);
            if arg.starts_with(b"-O") {
                user.optimizes = arg != b"-O0";
            }
            user.args.push(kept);
        }
        i += 1;
    }
    user
}

fn prefix_map_arg(mapping: &[u8]) -> Vec<u8> {
    let mut a = b"-ffile-prefix-map=".to_vec();
    a.extend_from_slice(mapping);
    a
}

/// `BuildDriverArgs(conf, lang, raw_args)` (A2). `pkgs_prefix_map` is `$PKGS_PREFIX_MAP` (unset
/// and empty alike).
#[must_use]
pub fn build_driver_args(
    conf: &DriverConf,
    lang: Language,
    raw_args: &[Vec<u8>],
    pkgs_prefix_map: &[u8],
) -> Vec<Vec<u8>> {
    let cxx = lang == Language::Cxx;
    let fortran = lang == Language::Fortran;
    let mut user = scan_user_args(raw_args, conf.binfmt);
    // toolchain, then package, then build system: later wins
    let mut out: Vec<Vec<u8>> = vec![if fortran {
        b"-Qunused-arguments".to_vec()
    } else {
        b"--start-no-unused-arguments".to_vec()
    }];
    out.extend(
        if fortran { &conf.fflags } else { &conf.flags }
            .iter()
            .cloned(),
    );
    if !fortran {
        // glibc rejects _FORTIFY_SOURCE under -O0, and the command line's own level wins
        // (Q-DRV-8: user args only)
        for flag in &conf.package.cflags {
            if is_fortify_arg(flag) && (user.sets_fortify || !user.optimizes) {
                continue;
            }
            out.push(flag.clone());
        }
    }
    if cxx {
        out.push(b"--driver-mode=g++".to_vec());
        out.extend(conf.cxxflags.iter().cloned());
        out.extend(conf.package.cxxflags.iter().cloned());
    }
    if !fortran {
        out.extend(conf.prefix_map.iter().map(|m| prefix_map_arg(m)));
        out.extend(
            split(pkgs_prefix_map, b':', false)
                .into_iter()
                .map(prefix_map_arg),
        );
    }
    // the always flags: ELF only, Fortran and compile-only steps included
    if conf.binfmt == BinFmt::Elf {
        out.push(b"-Wl,--build-id=sha1".to_vec());
        out.push(br#"-Wl,--package-metadata={"type":"repkgs"}"#.to_vec());
    }
    if !fortran {
        out.push(b"--end-no-unused-arguments".to_vec());
    }
    let mut tail = Vec::new();
    if user.linking && user.have_input {
        tail.clone_from(&conf.package.ldflags);
        if conf.binfmt == BinFmt::Coff {
            coff::coff_libs(conf, &mut user);
        }
        if !user.no_policy {
            match conf.binfmt {
                BinFmt::Elf => elf_link(conf, cxx, &mut user, &mut tail),
                BinFmt::MachO => macho::macho_link(&mut tail),
                BinFmt::Coff => {}
            }
        }
    }
    out.append(&mut user.args);
    out.append(&mut tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::conf::{PackageCcFlags, parse_driver_conf};

    const ELF: &str = r#"{"cc": "/seed/bin/clang", "flags": ["--target=x", "-O2"], "cxxflags": ["-stdlib=libc++"], "libc": "/sr/libc", "crt": "/cc/lib/crt_interp.o", "runtimes": "/sr/rt/lib"}"#;
    const COFF: &str = r#"{"cc": "/seed/bin/clang", "binfmt": "coff", "flags": ["--target=x86_64-pc-windows-msvc"]}"#;
    const MACHO: &str = r#"{"cc": "/seed/bin/clang", "binfmt": "macho", "flags": ["--target=arm64-apple-macos14.0"], "libc": "/sr"}"#;

    fn conf(text: &str) -> DriverConf {
        parse_driver_conf(text.as_bytes(), b"").unwrap()
    }

    /// The vector notation: `START`, `END`, `META`, `CRT_TAIL`, and `_×N` inside a string.
    fn expand(items: &[&str]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for &item in items {
            match item {
                "START" => out.push(b"--start-no-unused-arguments".to_vec()),
                "END" => out.push(b"--end-no-unused-arguments".to_vec()),
                "META" => out.push(br#"-Wl,--package-metadata={"type":"repkgs"}"#.to_vec()),
                "CRT_TAIL" => out.extend(expand(&[
                    "-x",
                    "none",
                    "/cc/lib/crt_interp.o",
                    "-Wl,--dynamic-linker=/sr/libc/lib/././././././././././././ld-linux-x86-64.so.2",
                    "-Wl,--export-dynamic-symbol=__reloc_start",
                ])),
                _ => out.push(underscores(item).into_bytes()),
            }
        }
        out
    }

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

    fn build(conf: &DriverConf, lang: Language, args: &[&str]) -> Vec<Vec<u8>> {
        let raw: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
        build_driver_args(conf, lang, &raw, b"")
    }

    fn joined(v: &[Vec<u8>]) -> String {
        String::from_utf8(jig_core::bytes::join(v, b" ")).unwrap()
    }

    fn with_package(
        mut c: DriverConf,
        cflags: &[&str],
        cxxflags: &[&str],
        ldflags: &[&str],
    ) -> DriverConf {
        let v = |s: &[&str]| s.iter().map(|a| a.as_bytes().to_vec()).collect();
        c.package = PackageCcFlags {
            cflags: v(cflags),
            cxxflags: v(cxxflags),
            ldflags: v(ldflags),
        };
        c
    }

    use Language::{C, Cxx, Fortran};

    #[test]
    fn tv3_to_tv7() {
        let m = conf(MACHO);
        assert_eq!(m.binfmt, BinFmt::MachO);
        assert_eq!(
            build(&m, C, &["a.c", "-o", "a"]),
            expand(&[
                "START",
                "--target=arm64-apple-macos14.0",
                "END",
                "a.c",
                "-o",
                "a",
                "-Wl,-headerpad_max_install_names"
            ])
        );
        let c = conf(COFF);
        assert_eq!(
            build(&c, C, &["-fPIC", "-c", "a.c"]),
            expand(&[
                "START",
                "--target=x86_64-pc-windows-msvc",
                "END",
                "-c",
                "a.c"
            ])
        );
        assert!(joined(&build(&c, Cxx, &["-std=c++11", "-c", "a.cc"])).contains("-std=c++14"));
        assert!(joined(&build(&c, Cxx, &["-std=gnu++17", "-c", "a.cc"])).contains("-std=gnu++17"));
        assert!(joined(&build(&m, Cxx, &["-std=c++11", "-c", "a.cc"])).contains("-std=c++11"));
        assert!(!joined(&build(&c, C, &["-c", "a.c"])).contains("build-id"));
        let e = conf(ELF);
        assert_eq!(
            build(&e, C, &["-c", "a.c"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-c",
                "a.c"
            ])
        );
        assert_eq!(
            build(&e, Cxx, &["-c", "a.cc"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "--driver-mode=g++",
                "-stdlib=libc++",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-c",
                "a.cc"
            ])
        );
    }

    #[test]
    fn tv8_coff_lower_casing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("libLLVM-22.dll.a"), "").unwrap();
        let w = dir.path().to_str().unwrap();
        let c = conf(COFF);
        let l = format!("-L{w}");
        let out = joined(&build(&c, C, &["a.o", &l, "-lWS2_32", "-lLLVM-22"]));
        assert!(
            out.contains("-lws2_32") && out.contains("-lLLVM-22"),
            "{out}"
        );
        let out = joined(&build(&c, C, &["a.o", "-nostartfiles", "-lWS2_32.lib"]));
        assert!(out.contains("-lws2_32.lib"), "{out}");
        let out = joined(&build(&c, C, &["a.o", "-L", w, "-lLLVM-22"]));
        assert!(out.contains("-lLLVM-22"), "{out}");
        // a package -L dir counts too, and package -l entries are never lower-cased
        let pc = with_package(conf(COFF), &[], &[], &[&l, "-lPKG"]);
        assert_eq!(
            build(&pc, C, &["a.o", "-lLLVM-22", "-lX"]),
            expand(&[
                "START",
                "--target=x86_64-pc-windows-msvc",
                "END",
                "a.o",
                "-lLLVM-22",
                "-lx",
                &l,
                "-lPKG"
            ])
        );
    }

    #[test]
    fn tv9_to_tv12_package_flags() {
        let p = with_package(conf(ELF), &["-O3"], &["-fno-rtti"], &["-Wl,-z,x"]);
        assert_eq!(
            build(&p, Cxx, &["-c", "a.cc", "-O0"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-O3",
                "--driver-mode=g++",
                "-stdlib=libc++",
                "-fno-rtti",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-c",
                "a.cc",
                "-O0"
            ])
        );
        assert_eq!(
            build(&p, C, &["-shared", "-o", "x.so", "x.o", "-L."]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-O3",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-shared",
                "-o",
                "x.so",
                "x.o",
                "-L.",
                "-Wl,-z,x",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×47",
                "-Wl,--enable-new-dtags"
            ])
        );
        assert_eq!(
            build(&p, C, &["-Wl,--build-id=none", "x.o"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-O3",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-Wl,--build-id=none",
                "x.o",
                "-Wl,-z,x",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×47",
                "-Wl,--enable-new-dtags",
                "CRT_TAIL"
            ])
        );
        let f = with_package(conf(ELF), &["-O2", "-D_FORTIFY_SOURCE=3"], &[], &[]);
        assert!(joined(&build(&f, C, &["-c", "a.c"])).contains("FORTIFY"));
        assert!(!joined(&build(&f, C, &["-c", "a.c", "-O0"])).contains("FORTIFY"));
        assert!(!joined(&build(&f, C, &["-c", "a.c", "-O2", "-O0"])).contains("=3"));
        let out = joined(&build(&f, C, &["-c", "a.c", "-D_FORTIFY_SOURCE=2"]));
        assert!(!out.contains("=3") && out.contains("=2"));
    }

    #[test]
    fn tv13_to_tv18_links() {
        let e = conf(ELF);
        assert_eq!(
            build(
                &e,
                C,
                &[
                    "-o",
                    "x",
                    "x.c",
                    "-Wl,-rpath,/usr/lib:/build/lib",
                    "-Wl,--dynamic-linker=/lib/ld.so"
                ]
            ),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-o",
                "x",
                "x.c",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/build/lib:/sr/libc/lib/.:/_×95",
                "-Wl,--enable-new-dtags",
                "CRT_TAIL"
            ])
        );
        assert!(
            build(&e, C, &["-o", "x", "x.c", "-Wl,-rpath,/build/build:"]).contains(
                &underscores("-Wl,-rpath,/build/build::/sr/libc/lib/.:/_×143").into_bytes()
            )
        );
        assert_eq!(
            build(&e, Cxx, &["-o", "x", "x.cc"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "--driver-mode=g++",
                "-stdlib=libc++",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-o",
                "x",
                "x.cc",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/rt/lib/.:/sr/libc/lib/.:/_×95",
                "-Wl,--enable-new-dtags",
                "CRT_TAIL"
            ])
        );
        assert!(
            build(&e, C, &["-o", "x", "x.c", "-lc++"]).contains(
                &underscores("-Wl,-rpath,/sr/rt/lib/.:/sr/libc/lib/.:/_×175").into_bytes()
            )
        );
        assert_eq!(
            build(&e, C, &["-shared", "-o", "l.so", "l.c"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-shared",
                "-o",
                "l.so",
                "l.c",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×47",
                "-Wl,--enable-new-dtags"
            ])
        );
        for flag in ["-static", "-static-pie", "-r", "-nostartfiles"] {
            assert!(!joined(&build(&e, C, &[flag, "-o", "x", "x.c"])).contains("-rpath"));
        }
        assert_eq!(
            build(&e, C, &["-static", "-o", "x", "x.c"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-static",
                "-o",
                "x",
                "x.c"
            ])
        );
        let store = std::str::from_utf8(jig_core::store_path::STORE_DIR).unwrap();
        let lib = format!("{store}/h-zlib/lib/libz.so.1");
        let out = build(&e, C, &["-o", "x", "x.c", &lib]);
        assert!(
            out.contains(
                &underscores(&format!(
                    "-Wl,-rpath,{store}/h-zlib/lib/.:/sr/libc/lib/.:/_×95"
                ))
                .into_bytes()
            )
        );
        assert!(out.contains(&lib.clone().into_bytes()));
    }

    #[test]
    fn tv21_to_tv29() {
        let e = conf(ELF);
        assert_eq!(
            build(&e, C, &["-o", "x", "x.c", "-lfoo", "-l", "bar"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-o",
                "x",
                "x.c",
                "-lfoo",
                "-l",
                "bar",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×207",
                "-Wl,--enable-new-dtags",
                "CRT_TAIL"
            ])
        );
        let out = build(
            &e,
            C,
            &[
                "x.o",
                "-Xlinker",
                "-rpath",
                "-Xlinker",
                "/a",
                "-Wl,-rpath",
                "-Wl,/b",
                "-Wl,--rpath=/c",
                "-Wl,-rpath",
                "/d",
            ],
        );
        assert_eq!(out[6..9], expand(&["x.o", "-Wl,-rpath", "/d"])[..]);
        assert_eq!(
            out[10],
            underscores("-Wl,-rpath,/a:/b:/c:/sr/libc/lib/.:/_×191").into_bytes()
        );
        let out = build(&e, C, &["x.o", "-Wl,-rpath,", "-Wl,-rpath,/usr/lib,-z,now"]);
        assert_eq!(out[6..7], expand(&["x.o"])[..]);
        assert_eq!(
            out[8],
            underscores("-Wl,-rpath,:/sr/libc/lib/.:/_×95").into_bytes()
        );
        assert_eq!(
            build(&e, C, &["-o", "x"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-o",
                "x",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×47",
                "-Wl,--enable-new-dtags",
                "CRT_TAIL"
            ])
        );
        assert_eq!(
            build(&e, C, &["--version"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "--version"
            ])
        );
        let out = build(&e, Cxx, &["x.o", "-nostdlib", "-lc++"]);
        assert!(
            out.contains(
                &underscores("-Wl,-rpath,/sr/rt/lib/.:/sr/libc/lib/.:/_×175").into_bytes()
            )
        );
        assert!(out.ends_with(&expand(&["CRT_TAIL"])));
        let out = build(&e, Cxx, &["x.o", "-lc++", "-nostdlib"]);
        assert!(out.contains(&underscores("-Wl,-rpath,/sr/libc/lib/.:/_×127").into_bytes()));
        let p = with_package(
            conf(ELF),
            &["-O3", "-D_FORTIFY_SOURCE=3"],
            &["-fno-rtti", "-D_FORTIFY_SOURCE=3"],
            &["-Wl,-z,x", "-lpkg"],
        );
        assert_eq!(
            build(&p, Cxx, &["-c", "a.cc", "-O0"]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-O3",
                "--driver-mode=g++",
                "-stdlib=libc++",
                "-fno-rtti",
                "-D_FORTIFY_SOURCE=3",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-c",
                "a.cc",
                "-O0"
            ])
        );
        assert!(
            joined(&build(&p, C, &["-c", "a.c", "-O0", "-Os"])).contains("-D_FORTIFY_SOURCE=3")
        );
        assert_eq!(
            build(&p, C, &["-shared", "-o", "x.so", "x.o", "-L."]),
            expand(&[
                "START",
                "--target=x",
                "-O2",
                "-O3",
                "-D_FORTIFY_SOURCE=3",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-shared",
                "-o",
                "x.so",
                "x.o",
                "-L.",
                "-Wl,-z,x",
                "-lpkg",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×127",
                "-Wl,--enable-new-dtags"
            ])
        );
        let nc = conf(r#"{"cc":"/c","libc":"/sr/libc","interp":"ld-linux-aarch64.so.1"}"#);
        assert_eq!(
            build(&nc, C, &["x.o", "-o", "x"]),
            expand(&[
                "START",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "x.o",
                "-o",
                "x",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/sr/libc/lib/.:/_×47",
                "-Wl,--enable-new-dtags",
                "-Wl,--dynamic-linker=/sr/libc/lib/ld-linux-aarch64.so.1"
            ])
        );
        let el = conf(r#"{"cc":"/c","crt":"/crt.o"}"#);
        assert_eq!(
            build(&el, C, &["x.o", "-o", "x"]),
            expand(&[
                "START",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "x.o",
                "-o",
                "x",
                "-Wl,--undefined-version",
                "-Wl,-rpath,/_×47",
                "-Wl,--enable-new-dtags",
                "-x",
                "none",
                "/crt.o",
                "-Wl,--dynamic-linker=/lib/././././././././././././ld-linux-x86-64.so.2",
                "-Wl,--export-dynamic-symbol=__reloc_start"
            ])
        );
    }

    #[test]
    fn tv30_fortran() {
        let f = parse_driver_conf(
            br#"{"cc":"/c","fc":"/f","fflags":["-L@/lib","@/a@/b","--@/x","a=b@/c=@/d","x@/y","-I","@/i"],"flags":["-fc"],"prefix-map":["/b=/c"],"libc":"/l"}"#,
            b"/self",
        )
        .unwrap();
        let prefix = [
            "-Qunused-arguments",
            "-L/self/lib",
            "/self/a@/b",
            "--/self/x",
            "a=b@/c=@/d",
            "x@/y",
            "-I",
            "/self/i",
            "-Wl,--build-id=sha1",
            "META",
        ];
        let mut want = prefix.to_vec();
        want.extend(["-c", "a.f90"]);
        assert_eq!(build(&f, Fortran, &["-c", "a.f90"]), expand(&want));
        let mut want = prefix.to_vec();
        want.extend([
            "a.o",
            "-o",
            "a",
            "-Wl,--undefined-version",
            "-Wl,-rpath,/l/lib/.:/_×47",
            "-Wl,--enable-new-dtags",
            "-Wl,--dynamic-linker=/l/lib/ld-linux-x86-64.so.2",
        ]);
        assert_eq!(build(&f, Fortran, &["a.o", "-o", "a"]), expand(&want));
        assert_eq!(
            build(&f, C, &["-c", "a.c"]),
            expand(&[
                "START",
                "-fc",
                "-ffile-prefix-map=/b=/c",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-c",
                "a.c"
            ])
        );
        let raw = vec![b"-c".to_vec(), b"a.c".to_vec()];
        assert_eq!(
            build_driver_args(&f, C, &raw, b"a=b::c=d:"),
            expand(&[
                "START",
                "-fc",
                "-ffile-prefix-map=/b=/c",
                "-ffile-prefix-map=a=b",
                "-ffile-prefix-map=c=d",
                "-Wl,--build-id=sha1",
                "META",
                "END",
                "-c",
                "a.c"
            ])
        );
        // Fortran ignores PKGS_PREFIX_MAP
        let raw = vec![b"-c".to_vec(), b"a.f".to_vec()];
        assert!(!joined(&build_driver_args(&f, Fortran, &raw, b"a=b")).contains("prefix-map"));
    }

    #[test]
    fn tv31_tv32() {
        let m = conf(MACHO);
        assert_eq!(
            build(&m, C, &["a.c", "-o", "a", "-Wl,-rpath,/x"]),
            expand(&[
                "START",
                "--target=arm64-apple-macos14.0",
                "END",
                "a.c",
                "-o",
                "a",
                "-Wl,-rpath,/x",
                "-Wl,-headerpad_max_install_names"
            ])
        );
        assert_eq!(
            build(&m, Cxx, &["-dynamiclib", "a.o", "-o", "a.dylib"]),
            expand(&[
                "START",
                "--target=arm64-apple-macos14.0",
                "--driver-mode=g++",
                "END",
                "-dynamiclib",
                "a.o",
                "-o",
                "a.dylib",
                "-Wl,-headerpad_max_install_names"
            ])
        );
        let c = conf(COFF);
        assert_eq!(
            build(
                &c,
                Cxx,
                &["-fPIC", "-std=gnu++0x", "-c", "a.cc", "-Wl,-rpath,/x"]
            ),
            expand(&[
                "START",
                "--target=x86_64-pc-windows-msvc",
                "--driver-mode=g++",
                "END",
                "-std=gnu++14",
                "-c",
                "a.cc",
                "-Wl,-rpath,/x"
            ])
        );
        assert_eq!(
            build(&c, C, &["a.o", "-lWS2_32", "-l", "Foo", "-lA/B"]),
            expand(&[
                "START",
                "--target=x86_64-pc-windows-msvc",
                "END",
                "a.o",
                "-lws2_32",
                "-l",
                "Foo",
                "-lA/B"
            ])
        );
        let out = build(
            &c,
            C,
            &[
                "-std=c++98",
                "-std=gnu++11",
                "-std=c++03",
                "-std=gnu++17",
                "-std=c11",
                "-c",
                "a.c",
            ],
        );
        assert_eq!(
            out[3..],
            expand(&[
                "-std=c++14",
                "-std=gnu++14",
                "-std=c++14",
                "-std=gnu++17",
                "-std=c11",
                "-c",
                "a.c",
                "END"
            ])[..7]
        );
    }

    /// TV-33 needs a real directory under the store dir; it runs only where that is writable.
    #[test]
    fn tv33_store_lib_dirs() {
        let store = std::str::from_utf8(jig_core::store_path::STORE_DIR).unwrap();
        let Ok(dir) = tempfile::Builder::new()
            .prefix("jig-tv33-")
            .tempdir_in(store)
        else {
            eprintln!("{store} not writable: skipped");
            return;
        };
        let z = dir.path().join("lib");
        std::fs::create_dir(&z).unwrap();
        std::fs::write(z.join("libz.so"), "").unwrap();
        let z = z.to_str().unwrap();
        let mut c = conf(r#"{"cc":"/c","libc":"/sr/libc","runtimes":"/usr/rt"}"#);
        c.package.ldflags = vec![format!("-L{z}/../lib").into_bytes(), b"-lm".to_vec()];
        let lib = format!("{z}/libz.so");
        let out = build(
            &c,
            Cxx,
            &["x.o", "-L", z, "-lz", &lib, "-L/nonstore", "-o", "x"],
        );
        let real = std::fs::canonicalize(z).unwrap();
        let rpath = underscores(&format!(
            "-Wl,-rpath,{}/.:/sr/libc/lib/.:/_×255",
            real.display()
        ));
        assert!(out.contains(&rpath.into_bytes()), "{}", joined(&out));
        assert!(out.ends_with(&expand(&[
            "-Wl,--enable-new-dtags",
            "-Wl,--dynamic-linker=/sr/libc/lib/ld-linux-x86-64.so.2"
        ])));
    }
}
