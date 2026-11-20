//! The `ec` binary (`behaviors/ec-dispatch.md`).
//!
//! `#![no_main]`: std's runtime start-up would set `SIGPIPE` to `SIG_IGN` before `main`. Legacy
//! ec never touches it, so the disposition ec inherited is what ec and its children run with
//! (`implementation/05-ec-foundation.md §Signal disposition`). `std::env::args_os` still works:
//! std reads `argv` from `.init_array` on glibc and from `_NSGetArgv` on macOS.
#![no_main]

use std::ffi::{c_char, c_int};

// SAFETY: the C entry point; it takes no pointers it dereferences and never returns.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
extern "C" fn main(_argc: c_int, _argv: *const *const c_char) -> c_int {
    // exit flushes Rust's stdout; values above 255 are truncated by the kernel, as C's exit is
    std::process::exit(ekacache::dispatch::main())
}
