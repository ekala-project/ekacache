//! Mode selection (`behaviors/ec-dispatch.md §Before dispatch`, `§Mode selection`, `main.cc`).

use std::io::Write;

use ekacache_core::env::Env;
use ekacache_core::fspath;

use crate::{Ctx, cache_cmd, cc, fixup, gocache, launcher, nix_store, rustc};

/// The usage text, byte for byte (`main.cc:76-89`, `§Usage text`).
pub const USAGE: &[u8] = b"usage: ec <mode> [args...]   or via a symlink named after the mode
       ec /path/to/<compiler> [args...]   launcher for gcc, clang, rustc or go

  cc c++ <args>         compiler driver + compile cache (any other name too)
  rustcwrap <args>      rustc through the cache
  gocacheprog           GOCACHEPROG server on stdin/stdout
  reloc-fixup <dir>     rewrite store references in an output tree
  nix-store <args>      realise paths over the builder RPC socket
  cache get|put <key> <file>, get-dir|put-dir <key> <dir>
  slot <program> <args> run holding an ecd job slot

  EKACACHE_SOCK  ecd socket (default <store>/../var/nix/ekacache/socket)
  EKACACHE_LOG, EKACACHE_LOG_ARGS  per-invocation log files
";

/// The whole program: capture the process state, dispatch, return the exit status.
#[must_use]
pub fn main() -> i32 {
    let argv: Vec<Vec<u8>> = std::env::args_os()
        .map(std::ffi::OsString::into_encoded_bytes)
        .collect();
    let argv0 = argv.first().cloned().unwrap_or_default();
    let mut ctx = Ctx::new(argv0, Env::capture());
    dispatch(&argv, &mut ctx)
}

/// The name the binary answers to as itself (`behaviors/ec-dispatch.md`).
pub const SELF_NAME: &[u8] = b"ec";

/// The table of `§Mode selection`. Comparisons are exact and case-sensitive; the same names
/// work as `argv[0]`'s basename and as `ec <mode>` (Q-DISPATCH-1).
pub fn dispatch(argv: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let (mut mode, mut args): (&[u8], &[Vec<u8>]) = match argv.split_first() {
        Some((argv0, rest)) => (fspath::filename(argv0), rest),
        None => (b"", &[]),
    };
    if mode == SELF_NAME {
        let Some((first, rest)) = args.split_first() else {
            let _ = std::io::stderr().write_all(USAGE);
            return 2;
        };
        if first == b"--help" || first == b"-h" {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(USAGE);
            let _ = out.flush();
            return 0;
        }
        mode = first;
        args = rest;
        if mode.contains(&b'/') {
            return launcher::run(mode, args, ctx);
        }
    }
    match mode {
        b"nix-store" => nix_store::run(args, ctx),
        b"cache" => cache_cmd::run(args, ctx),
        b"slot" => cache_cmd::slot(args, ctx),
        b"gocacheprog" => gocache::run(ctx),
        b"rustcwrap" => rustc::run(args, ctx),
        b"reloc-fixup" => fixup::run(args, ctx),
        _ => cc::run(mode, args, None, ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_matches_the_doc() {
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../behaviors/ec-dispatch.md"
        ))
        .unwrap();
        let start = doc.find("```\nusage: ec").unwrap() + 4;
        let end = start + doc[start..].find("```").unwrap();
        assert_eq!(&doc.as_bytes()[start..end], USAGE);
    }
}
