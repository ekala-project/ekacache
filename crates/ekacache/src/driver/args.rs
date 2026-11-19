//! `BuildDriverArgs` and `ScanUserArgs` (`behaviors/jig-driver-policy.md` A1–A5): conf flags
//! first, the build system's rpaths taken over, the RUNPATH/interp policy appended when linking.

use ekacache_core::bytes::split;

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
    //! The doc's TV-1…TV-34 are in `tests/driver_vectors.rs`, read from
    //! `tests/vectors/jig-driver-policy.toml`; these are the cases beyond them.
    use super::*;
    use crate::driver::conf::{PackageCcFlags, parse_driver_conf};

    const COFF: &str = r#"{"cc": "/seed/bin/clang", "binfmt": "coff", "flags": ["--target=x86_64-pc-windows-msvc"]}"#;

    fn v(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|a| a.as_bytes().to_vec()).collect()
    }

    fn joined(v: &[Vec<u8>]) -> String {
        String::from_utf8(ekacache_core::bytes::join(v, b" ")).unwrap()
    }

    /// TV-8's setup: a package `-L` dir counts for COFF lower-casing too, and package `-l`
    /// entries are never lower-cased.
    #[test]
    fn coff_package_lib_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("libLLVM-22.dll.a"), "").unwrap();
        let l = format!("-L{}", dir.path().to_str().unwrap());
        let mut c = parse_driver_conf(COFF.as_bytes(), b"").unwrap();
        c.package = PackageCcFlags {
            cflags: Vec::new(),
            cxxflags: Vec::new(),
            ldflags: v(&[&l, "-lPKG"]),
        };
        assert_eq!(
            build_driver_args(&c, Language::C, &v(&["a.o", "-lLLVM-22", "-lX"]), b""),
            v(&[
                "--start-no-unused-arguments",
                "--target=x86_64-pc-windows-msvc",
                "--end-no-unused-arguments",
                "a.o",
                "-lLLVM-22",
                "-lx",
                &l,
                "-lPKG"
            ])
        );
    }

    /// TV-30's conf: Fortran ignores `PKGS_PREFIX_MAP`.
    #[test]
    fn fortran_ignores_pkgs_prefix_map() {
        let f = parse_driver_conf(
            br#"{"cc":"/c","fc":"/f","fflags":["-L@/lib","@/a@/b","--@/x","a=b@/c=@/d","x@/y","-I","@/i"],"flags":["-fc"],"prefix-map":["/b=/c"],"libc":"/l"}"#,
            b"/self",
        )
        .unwrap();
        let raw = v(&["-c", "a.f"]);
        assert!(
            !joined(&build_driver_args(&f, Language::Fortran, &raw, b"a=b")).contains("prefix-map")
        );
    }
}
