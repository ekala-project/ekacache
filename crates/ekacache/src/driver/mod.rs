//! Driver policy: what turns `cc <user args>` into the real compiler's command line
//! (`behaviors/jig-driver-policy.md`, `driver.cc`, `implementation/06-jig-cc.md §Part 1`).
//!
//! No Q-DRV quirk is adopted, so everything here reproduces the C++ jig, including the aborts of
//! Q-DRV-9 and Q-DRV-10. Q-DRV-10's libstdc++ `terminate called …` text is not imitated: the
//! process dies by SIGABRT without a message.

mod args;
mod coff;
mod conf;
mod elf;
pub mod json;
mod macho;
mod search_dirs;

pub use args::build_driver_args;
pub use conf::{
    BinFmt, DriverConf, Fatal, PackageCcFlags, conf_less, load_driver_conf, load_driver_conf_for,
    package_flags, parse_driver_conf,
};
pub use elf::{INTERP_SLACK, NEEDED_SLACK, RUNPATH_SLACK, is_shared_lib_name};
pub use search_dirs::with_package_lib_dirs;

use ekacache_core::bytes::{contains, os_path};

use crate::sys;

/// `Language`, from the invoked name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    C,
    Cxx,
    Fortran,
}

impl Language {
    /// §Interface to cc mode step 4: `++` → C++, else `fortran` or `flang` → Fortran, else C.
    #[must_use]
    pub fn of(name: &[u8]) -> Self {
        if contains(name, b"++") {
            Language::Cxx
        } else if contains(name, b"fortran") || contains(name, b"flang") {
            Language::Fortran
        } else {
            Language::C
        }
    }
}

/// The throwing `std::filesystem::exists(path)`: `stat` succeeds → true; `ENOENT`/`ENOTDIR` →
/// false; `EOVERFLOW` → true (libstdc++ reports `file_type::unknown`, which exists); any other
/// error throws, and the uncaught `filesystem_error` aborts the process (Q-DRV-10).
pub(crate) fn exists_or_abort(path: &[u8]) -> bool {
    let c = sys::cstring(path);
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `c` is NUL-terminated and `st` is valid for writes of one `struct stat`.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::stat(c.as_ptr(), st.as_mut_ptr()) };
    if rc == 0 {
        return true;
    }
    match sys::errno() {
        libc::ENOENT | libc::ENOTDIR => false,
        libc::EOVERFLOW => true,
        // Q-DRV-10
        _ => std::process::abort(),
    }
}

/// `RealDir(path)`: `std::filesystem::canonical(path, ec)` (every symlink resolved, absolute),
/// or the path itself on any error.
pub(crate) fn real_dir(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return Vec::new();
    }
    std::fs::canonicalize(os_path(path)).map_or_else(
        |_| path.to_vec(),
        |p| p.into_os_string().into_encoded_bytes(),
    )
}

/// `FlagValue(args, i, flag)`: `-Lx` → `x`; `-L x` → `x` (the value is not skipped).
pub(crate) fn flag_value<'a, A: AsRef<[u8]>>(
    args: &'a [A],
    i: usize,
    flag: &[u8],
) -> Option<&'a [u8]> {
    let arg = args[i].as_ref();
    if !arg.starts_with(flag) {
        return None;
    }
    if arg.len() > flag.len() {
        return Some(&arg[flag.len()..]);
    }
    args.get(i + 1).map(AsRef::as_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language() {
        for (name, lang) in [
            ("c++", Language::Cxx),
            ("g++", Language::Cxx),
            ("clang++-21", Language::Cxx),
            ("x86_64-linux-gnu-g++", Language::Cxx),
            ("flang++", Language::Cxx),
            ("gfortran", Language::Fortran),
            ("flang", Language::Fortran),
            ("flang-new", Language::Fortran),
            ("cc", Language::C),
            ("gcc", Language::C),
            ("clang", Language::C),
            ("Fortran", Language::C),
        ] {
            assert_eq!(Language::of(name.as_bytes()), lang, "{name}");
        }
    }

    #[test]
    fn probes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().as_os_str().as_encoded_bytes().to_vec();
        assert!(exists_or_abort(&d));
        assert!(!exists_or_abort(b"/nonexistent/x"));
        assert!(!exists_or_abort(b""));
        let mut f = d.clone();
        f.extend_from_slice(b"/file");
        std::fs::write(os_path(&f), "").unwrap();
        let mut under_file = f.clone();
        under_file.extend_from_slice(b"/x");
        assert!(!exists_or_abort(&under_file)); // ENOTDIR
        assert_eq!(real_dir(b"/nonexistent/x"), b"/nonexistent/x");
        assert_eq!(real_dir(b""), b"");
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            real_dir(link.as_os_str().as_encoded_bytes()),
            canon.as_os_str().as_encoded_bytes()
        );
        let args: [&[u8]; 3] = [b"-Lx", b"-L", b"y"];
        assert_eq!(flag_value(&args, 0, b"-L"), Some(&b"x"[..]));
        assert_eq!(flag_value(&args, 1, b"-L"), Some(&b"y"[..]));
        assert_eq!(flag_value(&args, 2, b"-L"), None);
        assert_eq!(flag_value(&[b"-L"], 0, b"-L"), None);
    }
}
