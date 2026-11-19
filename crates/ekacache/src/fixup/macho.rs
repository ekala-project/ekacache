//! A6: the Mach-O fixup (`behaviors/jig-reloc-fixup.md §A6`, `§Mach-O structures`,
//! `§Code signature`; `macho_fixup.cc`). Host independent: nothing here needs macOS.
//!
//! Path load commands are respelled `@loader_path`-relative (LC_ID_DYLIB becomes the final
//! absolute path), grown into the header padding, and an ad-hoc SHA-256 code signature is
//! re-hashed page by page. All reads and writes go through [`Image`]; offsets wrap modulo 2^64,
//! and where legacy computes a signature offset in 32 bits it wraps modulo 2^32, as there.

use std::cmp::Ordering;

use ekacache_core::bytes::{concat, join};
use ekacache_core::fspath;
use ekacache_core::store_path::is_store_path;
use sha2::{Digest, Sha256};

use super::fsops::exists_quiet;
use super::image::Image;
use super::output::abort_now;
use super::writeback::write_back;
use super::{Fixup, relative_to};

const MAGIC_64: u32 = 0xfeed_facf;
const HEADER_SIZE: u64 = 32;
/// `offsetof(mach_header_64, sizeofcmds)`.
const SIZEOFCMDS: u64 = 20;
const LOAD_COMMAND_SIZE: u32 = 8;

const LC_SEGMENT_64: u32 = 0x19;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_ID_DYLIB: u32 = 0xd;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_RPATH: u32 = 0x8000_001c;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;

/// `segment_command_64` field offsets and the `section_64` records after it.
const SEG_FILEOFF: u64 = 40;
const SEG_FILESIZE: u64 = 48;
const SEG_NSECTS: u64 = 64;
const SEG_SECTIONS: u64 = 72;
const SECTION_SIZE: u64 = 80;
const SECTION_OFFSET: u64 = 48;

const EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CODE_DIRECTORY: u32 = 0xfade_0c02;
const CD_HASH_OFFSET: u64 = 16;
const CD_CODE_SLOTS: u64 = 28;
const CD_CODE_LIMIT: u64 = 32;
const CD_HASH_SIZE: u64 = 36;
const CD_HASH_TYPE: u64 = 37;
const CD_PAGE_SIZE: u64 = 39;
const HASH_SHA256: u8 = 2;
const SHA256_SIZE: u8 = 32;

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at.wrapping_add(4))
        .and_then(|s| s.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

/// One kept load command.
struct Command {
    cmd: u32,
    /// The whole command, `cmdsize` bytes.
    bytes: Vec<u8>,
    /// The `lc_str` offset of a path command when it is inside the command, else 0.
    path_off: u32,
}

impl Command {
    /// The path: from `path_off` up to the first NUL or the end of the command.
    fn path(&self) -> &[u8] {
        if self.path_off == 0 {
            return b"";
        }
        let rest = self.bytes.get(self.path_off as usize..).unwrap_or_default();
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        &rest[..end]
    }

    /// `SetPath(t)`: the bytes before `path_off` kept, `t`, NUL padding to the next multiple of
    /// 8 that leaves at least one NUL, the new length as `cmdsize`.
    fn set_path(&mut self, path: &[u8]) {
        self.bytes.truncate(self.path_off as usize);
        self.bytes.extend_from_slice(path);
        // ((len + 8) / 8) × 8
        let padded = self.bytes.len().wrapping_add(8) & !7;
        self.bytes.resize(padded, 0);
        let size = (self.bytes.len() as u32).to_le_bytes();
        if let Some(field) = self.bytes.get_mut(4..8) {
            field.copy_from_slice(&size);
        }
    }
}

fn is_path_command(cmd: u32) -> bool {
    matches!(
        cmd,
        LC_LOAD_DYLIB
            | LC_LOAD_WEAK_DYLIB
            | LC_REEXPORT_DYLIB
            | LC_LOAD_UPWARD_DYLIB
            | LC_ID_DYLIB
            | LC_RPATH
    )
}

struct LoadCommands {
    commands: Vec<Command>,
    /// Bytes from the end of the header to the first segment content.
    room: u64,
}

/// A6.2.
fn read_load_commands(img: &Image, ncmds: u32) -> LoadCommands {
    let l = img.len();
    let mut out = LoadCommands {
        commands: Vec::new(),
        room: l.wrapping_sub(HEADER_SIZE),
    };
    let mut off = HEADER_SIZE;
    for _ in 0..ncmds {
        let Some(lc) = img.read::<8>(off) else {
            break;
        };
        let (cmd, cmdsize) = (le32(&lc, 0), le32(&lc, 4));
        let end = off.wrapping_add(u64::from(cmdsize));
        // Q-FIX-11: the commands after a stop are dropped
        if cmdsize < LOAD_COMMAND_SIZE || end > l {
            break;
        }
        let Some(bytes) = img.bytes.get(off as usize..end as usize) else {
            break;
        };
        let mut command = Command {
            cmd,
            bytes: bytes.to_vec(),
            path_off: 0,
        };
        if is_path_command(cmd) {
            let po = img.u32_le(off.wrapping_add(8)).unwrap_or(0);
            command.path_off = if po < cmdsize { po } else { 0 };
        }
        if cmd == LC_SEGMENT_64 {
            let fileoff = img.u64_le(off.wrapping_add(SEG_FILEOFF)).unwrap_or(0);
            let filesize = img.u64_le(off.wrapping_add(SEG_FILESIZE)).unwrap_or(0);
            if filesize != 0 && fileoff == 0 {
                // __TEXT maps the header itself: its sections' offsets bound the room
                let nsects = img.u32_le(off.wrapping_add(SEG_NSECTS)).unwrap_or(0);
                for s in 0..nsects {
                    let at = off
                        .wrapping_add(SEG_SECTIONS)
                        .wrapping_add(u64::from(s).wrapping_mul(SECTION_SIZE))
                        .wrapping_add(SECTION_OFFSET);
                    // legacy reads 0 and goes on; every later record is past the end as well
                    let Some(so) = img.u32_le(at) else {
                        break;
                    };
                    if so != 0 {
                        out.room = out.room.min(u64::from(so).wrapping_sub(HEADER_SIZE));
                    }
                }
            } else if filesize != 0 {
                out.room = out.room.min(fileoff.wrapping_sub(HEADER_SIZE));
            }
        }
        out.commands.push(command);
        off = end;
    }
    out
}

/// `/usr/lib/…` and `/System/…`, which the OS provides.
fn is_system_path(p: &[u8]) -> bool {
    p.starts_with(b"/usr/lib/") || p.starts_with(b"/System/")
}

/// `Resolve(p)`: where a load command points once the tree is at dest, `None` when dyld would
/// not find it.
fn resolve(fx: &Fixup, here: &[u8], p: &[u8]) -> Option<Vec<u8>> {
    if is_system_path(p) {
        // the SDK lists what the OS ships: libz.1.dylib as libz.1.tbd, Foo.framework/Foo as Foo.tbd
        let stem = p.strip_suffix(b".dylib").unwrap_or(p);
        let tbd = concat(&[stem.get(1..).unwrap_or_default(), b".tbd"]);
        let known = fx.sdk.is_empty() || exists_quiet(&fspath::join(&fx.sdk, &tbd));
        return known.then(|| p.to_vec());
    }
    let target = if let Some(rest) = p.strip_prefix(b"@rpath/") {
        // only our own: a dependency's id is absolute, so nothing else says @rpath
        fx.own_lib_dirs
            .iter()
            .map(|d| fspath::join(d, rest))
            .find(|candidate| exists_quiet(candidate))
            .map(|found| fx.final_path(&found))
    } else if let Some(rest) = p.strip_prefix(b"@loader_path/") {
        Some(fspath::normalize(&fspath::join(here, rest)))
    } else {
        let f = fx.final_path(p);
        is_store_path(&f).then_some(f)
    }?;
    // ours must exist; a dependency's store path does, or the linker had not found it
    let on_disk = fx.on_disk(&target);
    // Q-FIX-7: when prefix == dest nothing is checked
    if fspath::compare(&on_disk, &target) != Ordering::Equal && !exists_quiet(&on_disk) {
        return None;
    }
    Some(target)
}

/// A6.3 step 2: what a path command should say, `None` to leave it.
fn new_spelling(
    fx: &mut Fixup,
    self_path: &[u8],
    command: &Command,
    shown: &[u8],
) -> Option<Vec<u8>> {
    let p = command.path();
    let here = fspath::parent_path(self_path);
    if command.cmd == LC_ID_DYLIB {
        // Q-FIX-8: the final absolute path, not `@rpath/<name>` as the legacy header says
        return Some(self_path.to_vec());
    }
    if command.cmd == LC_RPATH {
        let target = fx.final_path(p);
        return is_store_path(&target).then(|| relative_to(&here, &target, b"@loader_path"));
    }
    let Some(target) = resolve(fx, &here, p) else {
        fx.error(concat(&[
            b"reloc-fixup: ",
            shown,
            b": links ",
            p,
            b", which neither the package, a dependency nor the SDK has",
        ]));
        return None;
    };
    Some(if is_system_path(p) {
        p.to_vec()
    } else {
        relative_to(&here, &target, b"@loader_path")
    })
}

/// The message of a failed re-sign.
fn signature_error(fx: &mut Fixup, shown: &[u8], what: &str) {
    fx.out
        .err(concat(&[b"reloc-fixup: ", shown, b": ", what.as_bytes()]));
}

/// A6.6: re-hash the code slots of every SHA-256 CodeDirectory. `false` after a message.
fn resign(fx: &mut Fixup, img: &mut Image, shown: &[u8]) -> bool {
    let ncmds = img.u32_le(16).unwrap_or(0);
    let mut off = HEADER_SIZE;
    let mut sig: Option<u32> = None;
    for _ in 0..ncmds {
        let Some(lc) = img.read::<8>(off) else {
            break;
        };
        let (cmd, cmdsize) = (le32(&lc, 0), le32(&lc, 4));
        if cmdsize < LOAD_COMMAND_SIZE {
            break;
        }
        if cmd == LC_CODE_SIGNATURE {
            // linkedit_data_command.dataoff; an unreadable one means "none" again
            sig = img.u32_le(off.wrapping_add(8));
        }
        off = off.wrapping_add(u64::from(cmdsize));
    }
    let Some(sig) = sig else {
        // unsigned (the x86_64 default): nothing to keep in sync
        return true;
    };
    if img.u32_be(u64::from(sig)) != Some(EMBEDDED_SIGNATURE) {
        signature_error(fx, shown, "LC_CODE_SIGNATURE is no embedded signature blob");
        return false;
    }
    // legacy adds to the u32 `dataoff` in 32 bits: these offsets wrap modulo 2^32
    let count = img.u32_be(u64::from(sig.wrapping_add(8))).unwrap_or(0);
    let index = u64::from(sig.wrapping_add(12));
    for j in 0..count {
        let entry = index
            .wrapping_add(u64::from(j).wrapping_mul(8))
            .wrapping_add(4);
        let blob = img.u32_be(entry);
        let cd = u64::from(sig.wrapping_add(blob.unwrap_or(0)));
        if img.u32_be(cd) != Some(CODE_DIRECTORY) {
            if blob.is_none() {
                // every later index entry is past the end too, so each would be this same
                // skipped blob at `sig`: legacy loops on to `count` without effect
                break;
            }
            continue;
        }
        let hash_off = img.u32_be(cd.wrapping_add(CD_HASH_OFFSET)).unwrap_or(0);
        let slots = img.u32_be(cd.wrapping_add(CD_CODE_SLOTS)).unwrap_or(0);
        let limit = img.u32_be(cd.wrapping_add(CD_CODE_LIMIT)).unwrap_or(0);
        let hash_size = img.u8(cd.wrapping_add(CD_HASH_SIZE)).unwrap_or(0);
        let hash_type = img.u8(cd.wrapping_add(CD_HASH_TYPE)).unwrap_or(0);
        let page_shift = img.u8(cd.wrapping_add(CD_PAGE_SIZE)).unwrap_or(0);
        if hash_type != HASH_SHA256 || hash_size != SHA256_SIZE {
            let what = format!("code signature hash type {hash_type} is not SHA-256");
            signature_error(fx, shown, &what);
            return false;
        }
        // legacy shifts a 64-bit 1 by pageSize, undefined for ≥ 64 (Q-FIX-10): refused here
        // like a foreign hash type when a slot needs the page size (needs sign-off); without
        // slots the value is never used and the blob is done, as in legacy
        let Some(page) = 1u64.checked_shl(u32::from(page_shift)) else {
            if slots == 0 {
                continue;
            }
            let what = format!("code signature page size shift {page_shift} is out of range");
            signature_error(fx, shown, &what);
            return false;
        };
        for slot in 0..slots {
            let begin = u64::from(slot).wrapping_mul(page);
            let end = begin.wrapping_add(page).min(u64::from(limit));
            let l = img.len();
            if begin > l {
                // Q-FIX-10: `string_view::substr` with a start past the end throws
                abort_now();
            }
            // `end < begin` wraps to "the rest of the file" (Q-FIX-10)
            let n = end.wrapping_sub(begin).min(l.wrapping_sub(begin));
            let Some(page_bytes) = img
                .bytes
                .get(begin as usize..begin.wrapping_add(n) as usize)
            else {
                abort_now();
            };
            let digest = Sha256::digest(page_bytes);
            let at = cd
                .wrapping_add(u64::from(hash_off))
                .wrapping_add(u64::from(slot).wrapping_mul(u64::from(hash_size)));
            if !img.overwrite(at, &digest) {
                let what = format!("code signature slot {slot} lies outside the file");
                signature_error(fx, shown, &what);
                return false;
            }
        }
    }
    true
}

/// A6. `false` is "not mine": no 64-bit little-endian Mach-O header.
pub fn fix_macho(fx: &mut Fixup, path: &[u8], rel: &[u8], img: &mut Image) -> bool {
    let Some(header) = img.read::<{ HEADER_SIZE as usize }>(0) else {
        return false;
    };
    if le32(&header, 0) != MAGIC_64 {
        return false;
    }
    // filetype is not checked
    let ncmds = le32(&header, 16);
    let self_path = fx.final_path(path);
    let shown = rel;
    let mut table = read_load_commands(img, ncmds);
    let mut log = vec![shown.to_vec()];
    for command in &mut table.commands {
        if command.path_off == 0 {
            continue;
        }
        if let Some(text) = new_spelling(fx, &self_path, command, shown)
            && text != command.path()
        {
            command.set_path(&text);
            log.push(text);
        }
    }
    if log.len() == 1 {
        // A6.4: nothing changed (errors may have been reported)
        return true;
    }
    let mut cmds: Vec<u8> = table
        .commands
        .iter()
        .flat_map(|c| c.bytes.iter().copied())
        .collect();
    if cmds.len() as u64 > table.room {
        let what = format!(
            "load commands need {} bytes, the header has room for {} (-headerpad)",
            cmds.len(),
            table.room
        );
        fx.error(concat(&[b"reloc-fixup: ", shown, b": ", what.as_bytes()]));
        return true;
    }
    // ncmds is not changed (Q-FIX-11)
    img.write_u32_le(SIZEOFCMDS, cmds.len() as u32);
    // room ≤ L − 32: everything up to the room bound is zeroed
    cmds.resize(table.room as usize, 0);
    img.overwrite(HEADER_SIZE, &cmds);
    if !resign(fx, img, shown) {
        fx.count_error();
        return true;
    }
    if !write_back(&mut fx.out, path, &img.bytes) {
        fx.count_error();
        return true;
    }
    fx.out.line(join(&log, b"  "));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_path_pads_to_eight_with_a_nul() {
        let mut c = Command {
            cmd: LC_RPATH,
            bytes: vec![0x1c, 0, 0, 0x80, 16, 0, 0, 0, 12, 0, 0, 0, b'/', b'x', 0, 0],
            path_off: 12,
        };
        assert_eq!(c.path(), b"/x");
        c.set_path(b"@loader_path");
        assert_eq!(c.bytes.len(), 32);
        assert_eq!(le32(&c.bytes, 4), 32);
        assert_eq!(c.path(), b"@loader_path");
        c.set_path(b"1234");
        // 12 + 4 = 16 → 24: there is always at least one NUL
        assert_eq!(c.bytes.len(), 24);
        // the bytes before path_off are kept
        assert_eq!(le32(&c.bytes, 0), LC_RPATH);
        assert_eq!(le32(&c.bytes, 8), 12);
    }

    fn fixup(prefix: &[u8], dest: &[u8]) -> Fixup {
        Fixup {
            prefix: prefix.to_vec(),
            dest: dest.to_vec(),
            own_lib_dirs: Vec::new(),
            denied: Vec::new(),
            sdk: Vec::new(),
            errors: 0,
            out: super::super::output::Out::default(),
        }
    }

    /// Q-FIX-7, which the integration tests cannot reach (prefix == dest needs a writable
    /// store): a dangling in-package target passes when prefix == dest.
    #[test]
    fn no_existence_check_when_prefix_is_dest() {
        let p: &[u8] = b"/nix/store/00000000000000000000000000000000-jig-test-missing";
        let here = [p, b"/lib"].concat();
        let load = b"@loader_path/libgone.dylib";
        assert_eq!(
            resolve(&fixup(p, p), &here, load),
            Some([&here[..], b"/libgone.dylib"].concat())
        );
        let dest: &[u8] = b"/nix/store/11111111111111111111111111111111-dest";
        let here = [dest, b"/lib"].concat();
        assert_eq!(resolve(&fixup(p, dest), &here, load), None);
        // a dependency outside dest is not checked either
        let dep = b"/nix/store/22222222222222222222222222222222-dep/lib/libz.dylib";
        assert_eq!(resolve(&fixup(p, dest), &here, dep), Some(dep.to_vec()));
    }
}
