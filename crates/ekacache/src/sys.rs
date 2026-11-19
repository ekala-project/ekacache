//! Thin wrappers over the C library where std's behaviour differs from legacy's: error texts
//! (`strerror`), C strings cut at the first NUL (what `std::string::c_str()` hands to a system
//! call), and the process environment pointer the child of `fork` execs with.

use std::ffi::{CString, c_char, c_int};

/// `std::error_code(errno, std::generic_category()).message()`: glibc's `strerror` text, for
/// example `No such file or directory`. Rust's `io::Error` Display appends ` (os error 2)`, which
/// legacy never prints.
#[must_use]
pub fn strerror(errno: c_int) -> Vec<u8> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for `buf.len()` bytes; the XSI `strerror_r` (what the libc
    // crate binds) writes a NUL-terminated message into it, truncated if needed.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::strerror_r(errno, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if rc != 0 && end == 0 {
        return format!("Unknown error {errno}").into_bytes();
    }
    buf[..end].to_vec()
}

/// `errno` of the last failed call.
#[must_use]
pub fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `bytes` as a C string: everything before the first NUL, as `c_str()` of a `std::string` that
/// contains one would be read by the kernel.
#[must_use]
pub fn cstring(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    // no NUL left in the slice, so `new` cannot fail
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// `std::filesystem::remove(path, ec)`: the C library's `remove(3)` (a file, a symlink, or an
/// empty directory). `false` when nothing was removed, including when `path` did not exist.
pub fn remove(path: &[u8]) -> bool {
    let c = cstring(path);
    // SAFETY: `c` is a valid NUL-terminated string for the duration of the call.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::remove(c.as_ptr()) };
    rc == 0
}

/// A NULL-terminated `char*` array over `strings`, which must outlive it.
#[must_use]
pub fn ptr_array(strings: &[CString]) -> Vec<*const c_char> {
    let mut v: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    v.push(std::ptr::null());
    v
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
unsafe extern "C" {
    /// The C library's environment pointer, which `execvp` reads in the child.
    static mut environ: *const *const c_char;
}

/// Point the C library's `environ` at `envp`. Called only in the child between `fork` and
/// `exec`, where nothing else runs.
///
/// # Safety
///
/// `envp` must be a NULL-terminated array of NUL-terminated strings that stays valid until the
/// process execs or exits, and no other thread may read or change the environment meanwhile.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub unsafe fn set_environ(envp: *const *const c_char) {
    // SAFETY: the caller guarantees exclusive access and a valid array.
    unsafe {
        environ = envp;
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe extern "C" {
    fn _NSGetEnviron() -> *mut *const *const c_char;
}

/// See the Linux variant.
///
/// # Safety
///
/// As on Linux.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub unsafe fn set_environ(envp: *const *const c_char) {
    // SAFETY: `_NSGetEnviron` returns the address of the process's `environ` variable; the
    // caller guarantees exclusive access and a valid array.
    unsafe {
        *_NSGetEnviron() = envp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strerror_is_the_c_text() {
        assert_eq!(strerror(libc::ENOENT), b"No such file or directory");
        assert_eq!(strerror(libc::EACCES), b"Permission denied");
        assert_eq!(strerror(99999), b"Unknown error 99999");
    }

    #[test]
    fn remove_files_links_and_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n).into_os_string().into_encoded_bytes();
        std::fs::write(dir.path().join("f"), "").unwrap();
        std::os::unix::fs::symlink("/nonexistent", dir.path().join("l")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::create_dir_all(dir.path().join("full/x")).unwrap();
        assert!(remove(&p("f")) && remove(&p("l")) && remove(&p("d")));
        assert!(!remove(&p("full")) && !remove(&p("missing")));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn cstring_cuts_at_nul() {
        assert_eq!(cstring(b"ab\0cd").as_bytes(), b"ab");
        assert_eq!(cstring(b"").as_bytes(), b"");
    }
}
