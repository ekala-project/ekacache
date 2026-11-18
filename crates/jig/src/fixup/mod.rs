//! reloc-fixup (`behaviors/jig-reloc-fixup.md`; `fixup_mode.cc`, `elf_fixup.cc`,
//! `macho_fixup.cc`; `implementation/10-jig-reloc-fixup.md`). Phase P8a, and the
//! host-independent Mach-O half of P9.
//!
//! The mode rewrites every ELF64-LE and 64-bit Mach-O image under `<prefix>` in place so the
//! tree still loads after it is copied to `--dest`, and checks every file for `--deny` hashes.
//! The output is legacy's, byte for byte, including the quirks the spec keeps or has not yet
//! decided (each marked `Q-FIX-n` where it is reproduced):
//!
//! - §Invocation: [`run`].
//! - A1/A3: [`run`] and [`fix_one`]; A2: [`walk`]; A4: [`deny`]; A5: [`elf`]; A6: [`macho`];
//!   A7: [`writeback`].
//! - §Conventions: *Image primitives* in [`image`]; the lexical path operations are
//!   `jig_core::fspath`, with `Final`, `OnDisk` and `RelativeTo` here and `rel(p)` and `exists`
//!   (which touch the disk) in [`fsops`].
//! - §Errors and messages: [`output`] writes them as legacy's `std::println` does, including
//!   stdout's block buffering, which decides what is lost when legacy aborts (Q-FIX-10).
//!
//! Offset arithmetic wraps modulo 2^64 as in the spec; every other integer operation is checked
//! or wrapping, enforced by `clippy::arithmetic_side_effects`.

#![warn(clippy::arithmetic_side_effects)]

mod deny;
mod elf;
mod fsops;
mod image;
mod macho;
mod output;
mod walk;
mod writeback;

use std::cmp::Ordering;

use jig_core::bytes::concat;
use jig_core::fspath;
use jig_core::store_path::{STORE_DIR, STORE_HASH_LENGTH, is_store_path};

use crate::Ctx;
use crate::util::read_file;

use self::fsops::relative_or_abort;
use self::image::Image;
use self::output::Out;

/// Message #1.
pub const USAGE: &[u8] =
    b"usage: reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...";

/// `FixupContext`, plus the two output streams.
struct Fixup {
    /// `<prefix>`, normalized: where the tree is now.
    prefix: Vec<u8>,
    /// `--dest`, normalized (default `prefix`): where the tree will live.
    dest: Vec<u8>,
    /// A2's `own_lib_dirs`, sorted by path comparison.
    own_lib_dirs: Vec<Vec<u8>>,
    /// The `--deny` needles, in order.
    denied: Vec<Vec<u8>>,
    /// `--sdk`, verbatim.
    sdk: Vec<u8>,
    errors: u64,
    out: Out,
}

impl Fixup {
    /// One error message to stderr, counted.
    fn error(&mut self, line: Vec<u8>) {
        self.out.err(line);
        self.count_error();
    }

    /// An error whose message was already written.
    fn count_error(&mut self) {
        self.errors = self.errors.wrapping_add(1);
    }

    /// `Final(p)`: `prefix/x` → `dest/x`.
    fn final_path(&self, p: &[u8]) -> Vec<u8> {
        map_under(p, &self.prefix, &self.dest)
    }

    /// `OnDisk(p)`: `dest/x` → `prefix/x`.
    fn on_disk(&self, p: &[u8]) -> Vec<u8> {
        map_under(p, &self.dest, &self.prefix)
    }
}

/// `Final`/`OnDisk`: `p` re-rooted from `from` to `to`; `p` verbatim when it is not under `from`.
/// "Not under" is a relative path that is empty or starts with the bytes `..` (Q-FIX-9).
fn map_under(p: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let r = fspath::relative(&fspath::normalize(p), from);
    // Q-FIX-9: a string prefix test, so a top-level `..data` counts as outside
    if r.is_empty() || r.starts_with(b"..") {
        return p.to_vec();
    }
    fspath::normalize(&fspath::join(to, &r))
}

/// `RelativeTo(dir, target, anchor)`: `<anchor>` or `<anchor>/<rel>`, plain `rel` without an
/// anchor. `dir` is used as given; an empty relative path gives `<anchor>/`.
fn relative_to(dir: &[u8], target: &[u8], anchor: &[u8]) -> Vec<u8> {
    let r = fspath::relative(&fspath::normalize(target), dir);
    if anchor.is_empty() {
        return r;
    }
    if r == b"." {
        return anchor.to_vec();
    }
    concat(&[anchor, b"/", &r])
}

/// A3 for one walked file.
fn fix_one(fx: &mut Fixup, path: &[u8]) {
    if fspath::extension(path) == b".debug" {
        return;
    }
    let Some(data) = read_file(path) else {
        return;
    };
    let rel = relative_or_abort(path, &fx.prefix);
    // a compiler driver's config names the build machine's compiler on purpose
    let exempt = [&b"etc/jig.json"[..], b"etc/roots"]
        .iter()
        .any(|e| fspath::compare(&rel, e) == Ordering::Equal);
    if !exempt {
        deny::check(fx, &rel, &data);
    }
    let mut img = Image::new(data);
    if !elf::fix_elf(fx, path, &rel, &mut img) {
        macho::fix_macho(fx, path, &rel, &mut img);
    }
}

/// §Invocation and A1, with output through `out`.
fn fixup_mode(args: &[Vec<u8>], out: Out) -> (i32, Out) {
    let mut out = out;
    let Some((first, rest)) = args.split_first() else {
        out.err(USAGE.to_vec());
        return (2, out);
    };
    let prefix = fspath::normalize(first);
    let mut dest = prefix.clone();
    let mut sdk = Vec::new();
    let mut denied = Vec::new();
    // Q-FIX-2: flags at odd positions only, the last unpaired argument ignored
    for pair in rest.chunks_exact(2) {
        let (flag, value) = (&pair[0], &pair[1]);
        match flag.as_slice() {
            b"--dest" => dest = fspath::normalize(value),
            b"--sdk" => sdk.clone_from(value),
            b"--deny" => {
                // Q-FIX-3: the last component only; `…/` gives an empty needle
                let name = fspath::filename(value);
                denied.push(name[..name.len().min(STORE_HASH_LENGTH)].to_vec());
            }
            _ => {
                out.err(concat(&[b"reloc-fixup: unknown argument ", flag]));
                return (2, out);
            }
        }
    }
    if !is_store_path(&dest) {
        out.err(concat(&[
            b"reloc-fixup: ",
            &dest,
            b" is not under ",
            STORE_DIR,
        ]));
        return (2, out);
    }
    let tree = walk::walk(&prefix);
    let mut fx = Fixup {
        prefix,
        dest,
        own_lib_dirs: tree.own_lib_dirs,
        denied,
        sdk,
        errors: 0,
        out,
    };
    for file in &tree.files {
        fix_one(&mut fx, file);
    }
    (i32::from(fx.errors != 0), fx.out)
}

/// `jig reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...`
///
/// Only the compile-time store directory is used; nothing of `ctx` is.
pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let _ = ctx;
    let (code, mut out) = fixup_mode(args, Out::default());
    // what legacy's exit flushes
    out.flush();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(prefix: &[u8], dest: &[u8]) -> Fixup {
        Fixup {
            prefix: prefix.to_vec(),
            dest: dest.to_vec(),
            own_lib_dirs: Vec::new(),
            denied: Vec::new(),
            sdk: Vec::new(),
            errors: 0,
            out: Out::default(),
        }
    }

    /// `(dir, target, anchor, result)`.
    type Case<'a> = (&'a [u8], &'a [u8], &'a [u8], &'a [u8]);

    /// §Test vectors, *path operations*.
    #[test]
    fn relative_to_vectors() {
        let here: &[u8] = b"/nix/store/y-foo/bin";
        let cases: [Case; 7] = [
            (
                here,
                b"/nix/store/x-zlib/lib/.",
                b"$ORIGIN",
                b"$ORIGIN/../../x-zlib/lib/",
            ),
            (
                here,
                b"/nix/store/x-zlib/lib",
                b"$ORIGIN",
                b"$ORIGIN/../../x-zlib/lib",
            ),
            (here, b"/nix/store/y-foo/bin/", b"$ORIGIN", b"$ORIGIN"),
            (here, b"/nix/store/y-foo/bin/..", b"$ORIGIN", b"$ORIGIN/../"),
            (
                here,
                b"/nix/store/g-glibc/lib/./././ld-linux-x86-64.so.2",
                b"",
                b"../../g-glibc/lib/ld-linux-x86-64.so.2",
            ),
            (here, b"relative/x", b"$ORIGIN", b"$ORIGIN/"),
            (
                b"/nix/store/y-foo/lib",
                b"/nix/store/y-foo/lib/libz.dylib",
                b"@loader_path",
                b"@loader_path/libz.dylib",
            ),
        ];
        for (dir, target, anchor, want) in cases {
            assert_eq!(
                relative_to(dir, target, anchor),
                want,
                "{}",
                String::from_utf8_lossy(target)
            );
        }
    }

    /// §Test vectors, *path operations*, with `prefix=/tmp/out`, `dest=/nix/store/y-foo`.
    #[test]
    fn final_and_on_disk_vectors() {
        let f = fx(b"/tmp/out", b"/nix/store/y-foo");
        let finals: [(&[u8], &[u8]); 7] = [
            (b"/tmp/out/lib", b"/nix/store/y-foo/lib"),
            (b"/tmp/out", b"/nix/store/y-foo/"),
            (b"/tmp/out/lib/.", b"/nix/store/y-foo/lib/"),
            (b"/tmp/outx/lib", b"/tmp/outx/lib"),
            (b"/nix/store/x/lib/.", b"/nix/store/x/lib/."),
            (b"$ORIGIN/../lib", b"$ORIGIN/../lib"),
            (b"/tmp//out//lib", b"/nix/store/y-foo/lib"),
        ];
        for (p, want) in finals {
            assert_eq!(f.final_path(p), want, "{}", String::from_utf8_lossy(p));
        }
        assert_eq!(f.on_disk(b"/nix/store/y-foo"), b"/tmp/out/");
        assert_eq!(f.on_disk(b"/nix/store/x/lib"), b"/nix/store/x/lib");
        // Q-FIX-9: `..data` directly under the prefix is "outside"
        assert_eq!(f.final_path(b"/tmp/out/..data/x"), b"/tmp/out/..data/x");
    }
}
