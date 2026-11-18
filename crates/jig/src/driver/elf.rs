//! The ELF link policy: RUNPATH over the store dirs with slack for `jig reloc-fixup`, our
//! dynamic linker, the crt_interp stub (`behaviors/jig-driver-policy.md` A6–A10).

use jig_core::bytes::split;
use jig_core::fspath;
use jig_core::store_path::is_store_path;

use super::args::UserArgs;
use super::{DriverConf, exists_or_abort, flag_value, real_dir};

/// Pad bytes per RUNPATH entry (absolute → `$ORIGIN`-relative).
pub const RUNPATH_SLACK: usize = 48;
/// Pad bytes per `-l` (`$ORIGIN/../../<hash>-x/lib/libx.so.N`).
pub const NEEDED_SLACK: usize = 80;
/// `./` pairs in front of the interp basename.
pub const INTERP_SLACK: usize = 12;

/// `IsHostDir`: a bare byte prefix, no component boundary (Q-DRV-4).
fn is_host_dir(path: &[u8]) -> bool {
    path.starts_with(b"/usr") || path.starts_with(b"/lib") || path.starts_with(b"/opt")
}

/// `RunpathList` (A7): the build system's rpaths verbatim, then our de-duplicated entries.
#[derive(Clone, Debug, Default)]
pub struct RunpathList {
    verbatim: Vec<u8>,
    entries: Vec<Vec<u8>>,
}

impl RunpathList {
    pub fn add(&mut self, dir: &[u8]) {
        if is_host_dir(dir) || self.entries.iter().any(|e| e == dir) {
            return;
        }
        self.entries.push(dir.to_vec());
    }

    /// Empty parts and duplicates are kept (cmake's install step string-matches
    /// `<build dir>:`); host dirs are dropped.
    pub fn add_verbatim(&mut self, rpath: &[u8]) {
        for part in split(rpath, b':', true) {
            if !is_host_dir(part) {
                self.verbatim.extend_from_slice(part);
                self.verbatim.push(b':');
            }
        }
    }

    /// `Render(libs)` (A8): verbatim, our entries spelt `<e>/.:`, then one `/___…` pad entry of
    /// `max(stores, 1)·48 + libs·80` bytes.
    #[must_use]
    pub fn render(&self, libs: usize) -> Vec<u8> {
        let mut out = self.verbatim.clone();
        for e in &self.entries {
            out.extend_from_slice(e);
            out.extend_from_slice(b"/.:");
        }
        // Q-DRV-15: every verbatim part counts as a store entry
        #[allow(clippy::naive_bytecount)] // a short string; no bytecount dependency for it
        let stores = self.entries.len() + self.verbatim.iter().filter(|&&c| c == b':').count();
        let pad = stores.max(1) * RUNPATH_SLACK + libs * NEEDED_SLACK - 1;
        out.push(b'/');
        out.resize(out.len() + pad, b'_');
        out
    }
}

/// `IsRuntimeLib`.
fn is_runtime_lib(lib: &[u8]) -> bool {
    matches!(
        lib,
        b"c++" | b"stdc++" | b"gcc_s" | b"unwind" | b":libunwind.so"
    )
}

/// `IsSharedLibName(basename)` (A10): `.dylib`, or `.so` (the last one) followed by nothing or
/// by `.` and only dots and digits.
#[must_use]
pub fn is_shared_lib_name(base: &[u8]) -> bool {
    if base.ends_with(b".dylib") {
        return true;
    }
    let Some(p) = jig_core::bytes::rfind(base, b".so") else {
        return false;
    };
    let tail = &base[p + 3..];
    if tail.is_empty() {
        return true;
    }
    tail[0] == b'.' && tail[1..].iter().all(|&c| c == b'.' || c.is_ascii_digit())
}

/// `AddRunpathEntries(conf, cxx, args, runpath)` (A9). Returns the `-l` count.
pub fn add_runpath_entries(
    conf: &DriverConf,
    cxx: bool,
    args: &[Vec<u8>],
    runpath: &mut RunpathList,
) -> usize {
    let mut lib_dirs: Vec<&[u8]> = Vec::new();
    let mut libs: Vec<&[u8]> = Vec::new();
    let mut links_runtime = cxx;
    for (i, arg) in args.iter().enumerate() {
        if let Some(dir) = flag_value(args, i, b"-L") {
            lib_dirs.push(dir);
        } else if let Some(lib) = flag_value(args, i, b"-l") {
            links_runtime = links_runtime || is_runtime_lib(lib);
            libs.push(lib);
        } else if is_store_path(arg) && is_shared_lib_name(fspath::filename(arg)) {
            runpath.add(&fspath::parent_path(&real_dir(arg)));
        }
        // Q-DRV-7: order-dependent
        if arg == b"-nostdlib++" || arg == b"-nostdlib" {
            links_runtime = false;
        }
    }
    let provides = |dir: &[u8]| {
        libs.iter().any(|lib| {
            [&b".so"[..], b".dylib"].iter().any(|ext| {
                let mut p = dir.to_vec();
                p.extend_from_slice(b"/lib");
                p.extend_from_slice(lib);
                p.extend_from_slice(ext);
                exists_or_abort(&p)
            })
        })
    };
    for dir in &lib_dirs {
        let real = real_dir(dir);
        if is_store_path(&real) && provides(&real) {
            runpath.add(&real);
        }
    }
    if links_runtime && !conf.runtimes.is_empty() {
        runpath.add(&conf.runtimes);
    }
    let mut libc_lib = conf.libc.clone();
    libc_lib.extend_from_slice(b"/lib");
    runpath.add(&libc_lib);
    libs.len()
}

/// `ElfLink` (A6), appending to the tail.
pub fn elf_link(conf: &DriverConf, cxx: bool, user: &mut UserArgs, out: &mut Vec<Vec<u8>>) {
    let mut link_args = user.args.clone();
    link_args.extend(conf.package.ldflags.iter().cloned());
    let libs = add_runpath_entries(conf, cxx, &link_args, &mut user.runpath);
    let mut rpath = b"-Wl,-rpath,".to_vec();
    rpath.extend_from_slice(&user.runpath.render(libs));
    out.push(b"-Wl,--undefined-version".to_vec());
    out.push(rpath);
    out.push(b"-Wl,--enable-new-dtags".to_vec());
    if user.shared {
        return;
    }
    let mut dynamic_linker = b"-Wl,--dynamic-linker=".to_vec();
    dynamic_linker.extend_from_slice(&conf.libc);
    dynamic_linker.extend_from_slice(b"/lib/");
    if conf.crt.is_empty() {
        dynamic_linker.extend_from_slice(&conf.interp);
        out.push(dynamic_linker);
        return;
    }
    for _ in 0..INTERP_SLACK {
        dynamic_linker.extend_from_slice(b"./");
    }
    dynamic_linker.extend_from_slice(&conf.interp);
    // after the user's args, so a trailing `-x c` (ghc's configure) must not claim the object
    out.push(b"-x".to_vec());
    out.push(b"none".to_vec());
    out.push(conf.crt.clone());
    out.push(dynamic_linker);
    out.push(b"-Wl,--export-dynamic-symbol=__reloc_start".to_vec());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tv19_tv35_shared_lib_names() {
        for (name, want) in [
            ("libz.so", true),
            ("libz.so.1.3", true),
            ("libz.son", false),
            ("x.o", false),
            ("libz.so.1a", false),
            ("a.dylib", true),
            ("libz.so.", true),
            ("x.so", true),
            ("x.dylib.1", false),
            ("libz.so.1.so", true),
            ("libz.so.1.so.x", false),
        ] {
            assert_eq!(is_shared_lib_name(name.as_bytes()), want, "{name}");
        }
    }

    #[test]
    fn runpath_list() {
        let mut r = RunpathList::default();
        r.add_verbatim(b"");
        assert_eq!(r.render(0), [&b":/"[..], &[b'_'; 47]].concat());
        let mut r = RunpathList::default();
        r.add_verbatim(b"a::/usr/x:b");
        r.add(b"/s");
        r.add(b"/s");
        r.add(b"/lib64");
        // stores = 3 verbatim parts + 1 entry
        assert_eq!(
            r.render(1),
            [&b"a::b:/s/.:/"[..], &[b'_'; 4 * 48 + 80 - 1]].concat()
        );
    }
}
