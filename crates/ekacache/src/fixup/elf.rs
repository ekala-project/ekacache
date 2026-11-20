//! A5: the ELF fixup (`behaviors/ec-reloc-fixup.md §A5`, `§ELF64 structures read or written`,
//! `§RUNPATH slot after rewrite`, `§PT_INTERP segment after rewrite`; `elf_fixup.cc`).
//!
//! RUNPATH entries become `$ORIGIN`-relative, NEEDED entries direct `$ORIGIN/…` paths when the
//! slack allows, and PT_INTERP is handed to the entry stub. All reads and writes go through
//! [`Image`] at explicit offsets; offset arithmetic wraps modulo 2^64.

use ekacache_core::bytes::{concat, find, join, split};
use ekacache_core::fspath;
use ekacache_core::store_path::{STORE_DIR, is_store_path};

use super::fsops::exists_or_abort;
use super::image::Image;
use super::output::abort_now;
use super::writeback::write_back;
use super::{Fixup, relative_to};

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const SHT_DYNAMIC: u32 = 6;
const SHT_NOBITS: u32 = 8;
const SHT_DYNSYM: u32 = 11;
const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;
const PT_NULL: u32 = 0;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;

const EHDR_SIZE: usize = 64;
const SHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const DYN_SIZE: u64 = 16;
const SYM_SIZE: u64 = 24;
const VERNEED_SIZE: usize = 16;

/// `struct StubHeader` in `crt_interp.c`: 16 bytes before the stub's entry, starting `RELOCSTB`.
const RELOC_STUB_MAGIC: &[u8] = b"RELOCSTB";
const RELOC_STUB_HEADER: u64 = 16;

/// Bytes of a leaked store path shown in the WARN line (`kLeakContext`).
const LEAK_CONTEXT: usize = 100;

const ORIGIN: &[u8] = b"$ORIGIN";

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at.wrapping_add(2))
        .and_then(|s| s.try_into().ok())
        .map_or(0, u16::from_le_bytes)
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at.wrapping_add(4))
        .and_then(|s| s.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

fn le64(b: &[u8], at: usize) -> u64 {
    b.get(at..at.wrapping_add(8))
        .and_then(|s| s.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

/// `Elf64_Ehdr` as read at offset 0, kept whole: A5.9 writes it back with only `e_entry`
/// changed.
struct Ehdr {
    raw: [u8; EHDR_SIZE],
    e_type: u16,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
}

impl Ehdr {
    fn parse(raw: [u8; EHDR_SIZE]) -> Self {
        Self {
            e_type: le16(&raw, 16),
            e_entry: le64(&raw, 24),
            e_phoff: le64(&raw, 32),
            e_shoff: le64(&raw, 40),
            e_phentsize: le16(&raw, 54),
            e_phnum: le16(&raw, 56),
            e_shentsize: le16(&raw, 58),
            e_shnum: le16(&raw, 60),
            raw,
        }
    }

    /// File offset of program header `i`: `e_phoff + i × e_phentsize`.
    fn ph_off(&self, i: u16) -> u64 {
        self.e_phoff
            .wrapping_add(u64::from(i).wrapping_mul(u64::from(self.e_phentsize)))
    }
}

/// The fields of an `Elf64_Shdr` that are used.
struct Section {
    ty: u32,
    offset: u64,
    size: u64,
    link: u32,
    entsize: u64,
}

/// A5.2: `e_shnum` headers from `e_shoff`, stopping at the first failed read.
fn read_sections(img: &Image, eh: &Ehdr) -> Vec<Section> {
    let mut sections = Vec::new();
    for i in 0..eh.e_shnum {
        let off = eh
            .e_shoff
            .wrapping_add(u64::from(i).wrapping_mul(u64::from(eh.e_shentsize)));
        let Some(raw) = img.read::<SHDR_SIZE>(off) else {
            break;
        };
        sections.push(Section {
            ty: le32(&raw, 4),
            offset: le64(&raw, 24),
            size: le64(&raw, 32),
            link: le32(&raw, 40),
            entsize: le64(&raw, 56),
        });
    }
    sections
}

/// `SectionEnd(s)`: the end of the section's file range clipped at L.
fn section_end(img: &Image, s: &Section) -> u64 {
    let l = img.len();
    if s.offset >= l {
        return 0;
    }
    s.offset.wrapping_add(s.size.min(l.wrapping_sub(s.offset)))
}

/// The section `sections[link]`, if `link` is in range.
fn linked(sections: &[Section], link: u32) -> Option<&Section> {
    sections.get(usize::try_from(link).ok()?)
}

/// The first SHT_DYNSYM and its string table offset, if `entsize ≥ 24` and its link is valid.
fn dynsym(sections: &[Section]) -> Option<(&Section, u64)> {
    let sym = sections.iter().find(|s| s.ty == SHT_DYNSYM)?;
    if sym.entsize < SYM_SIZE {
        return None;
    }
    let strtab = linked(sections, sym.link)?.offset;
    Some((sym, strtab))
}

/// One DT_NEEDED entry.
struct Needed {
    name: Vec<u8>,
    /// File offset of this entry's `d_val`.
    val_offset: u64,
    /// File offsets of the `vn_file` fields naming it.
    vn_files: Vec<u64>,
}

#[derive(Default)]
struct DynamicInfo {
    needed: Vec<Needed>,
    dynstr: u64,
    runpath_offset: Option<u64>,
}

/// A5.3: NEEDED, RUNPATH/RPATH and the verneed `vn_file` fields.
fn read_dynamic(img: &Image, sections: &[Section]) -> DynamicInfo {
    let mut info = DynamicInfo::default();
    let Some(dynamic) = sections.iter().find(|s| s.ty == SHT_DYNAMIC) else {
        return info;
    };
    let Some(strs) = linked(sections, dynamic.link) else {
        return info;
    };
    if strs.ty == SHT_NOBITS {
        return info;
    }
    let dynstr = strs.offset;
    info.dynstr = dynstr;
    let end = section_end(img, dynamic);
    let mut off = dynamic.offset;
    while off < end {
        let Some(raw) = img.read::<16>(off) else {
            break;
        };
        let tag = le64(&raw, 0) as i64;
        let val = le64(&raw, 8);
        if tag == DT_NULL {
            break;
        }
        if tag == DT_NEEDED {
            info.needed.push(Needed {
                name: img.cstring(dynstr.wrapping_add(val)).to_vec(),
                val_offset: off.wrapping_add(8),
                vn_files: Vec::new(),
            });
        }
        if tag == DT_RUNPATH || tag == DT_RPATH {
            // the last one wins
            info.runpath_offset = Some(dynstr.wrapping_add(val));
        }
        off = off.wrapping_add(DYN_SIZE);
    }
    // the dynamic section's string table, not the verneed section's sh_link
    let mut off = sections
        .iter()
        .find(|s| s.ty == SHT_GNU_VERNEED)
        .map_or(0, |s| s.offset);
    while off != 0 {
        let Some(raw) = img.read::<VERNEED_SIZE>(off) else {
            break;
        };
        let vn_file = le32(&raw, 4);
        let vn_next = le32(&raw, 12);
        let file = img.cstring(dynstr.wrapping_add(u64::from(vn_file)));
        for lib in &mut info.needed {
            if lib.name == file {
                lib.vn_files.push(off.wrapping_add(4));
            }
        }
        off = if vn_next == 0 {
            0
        } else {
            off.wrapping_add(u64::from(vn_next))
        };
    }
    info
}

/// A5.6: the first `.dynsym` name starting strictly inside `(begin, end)`. A failed read skips
/// the entry.
fn symbol_inside(img: &Image, sections: &[Section], begin: u64, end: u64) -> Option<Vec<u8>> {
    let (sym, strtab) = dynsym(sections)?;
    let stop = section_end(img, sym);
    let mut off = sym.offset;
    while off < stop {
        if let Some(raw) = img.read::<24>(off) {
            let name = strtab.wrapping_add(u64::from(le32(&raw, 0)));
            if name > begin && name < end {
                return Some(img.cstring(name).to_vec());
            }
        }
        off = off.wrapping_add(sym.entsize);
    }
    None
}

/// One RUNPATH element: its spelling, the on-disk directory it denotes, whether it is rendered.
struct RunpathDir {
    entry: Vec<u8>,
    dir: Vec<u8>,
    keep: bool,
}

/// A5.4 steps 2–5.
fn relativize_runpath(fx: &Fixup, old: &[u8], here: &[u8]) -> Vec<RunpathDir> {
    let final_here = fx.final_path(here);
    let mut runpath = Vec::new();
    for entry in split(old, b':', false) {
        let mut d = fx.final_path(entry);
        if entry == ORIGIN || entry.starts_with(b"$ORIGIN/") {
            // `join` with an absolute remainder replaces final_here (Q-FIX-14)
            let rest = &entry[entry.len().min(ORIGIN.len().wrapping_add(1))..];
            d = fspath::normalize(&fspath::join(&final_here, rest));
        }
        // everything not in the store drops out (Q-FIX-14)
        if is_store_path(&d) {
            runpath.push(RunpathDir {
                entry: relative_to(&final_here, &d, ORIGIN),
                dir: fx.on_disk(&d),
                keep: true,
            });
        }
    }
    for own in &fx.own_lib_dirs {
        runpath.push(RunpathDir {
            entry: relative_to(&final_here, &fx.final_path(own), ORIGIN),
            dir: own.clone(),
            keep: false,
        });
    }
    runpath
}

/// A5.4 step 6: kept entries in order, the first of identical strings only, `:`-joined.
fn render(runpath: &[RunpathDir]) -> Vec<u8> {
    let mut kept: Vec<&[u8]> = Vec::new();
    for d in runpath {
        // deduplication by exact string: `…/lib/` and `…/lib` both stay (Q-FIX-1, Q-FIX-16)
        if d.keep && !kept.contains(&d.entry.as_slice()) {
            kept.push(&d.entry);
        }
    }
    join(&kept, b":")
}

/// A5.3–A5.7. `false` when the file failed (already reported and counted).
fn fix_runpath(
    fx: &mut Fixup,
    path: &[u8],
    img: &mut Image,
    sections: &[Section],
    log: &mut Vec<Vec<u8>>,
    dirty: &mut bool,
) -> bool {
    let dynamic = read_dynamic(img, sections);
    let Some(runpath_offset) = dynamic.runpath_offset else {
        // no RUNPATH: NEEDED neither checked nor changed (Q-FIX-14)
        return true;
    };
    let old = img.cstring(runpath_offset).to_vec();
    let mut runpath = relativize_runpath(fx, &old, &fspath::parent_path(path));
    // (index into dynamic.needed, index into runpath)
    let mut direct: Vec<(usize, usize)> = Vec::new();
    for (ni, lib) in dynamic.needed.iter().enumerate() {
        let name = lib.name.as_slice();
        // Q-FIX-13: the skip list is glibc's
        if name.contains(&b'/') || name.starts_with(b"ld-linux") || name.starts_with(b"linux-vdso")
        {
            continue;
        }
        let Some(ri) = runpath
            .iter()
            .position(|r| exists_or_abort(&fspath::join(&r.dir, name)))
        else {
            let line = concat(&[
                path,
                b": NEEDED ",
                name,
                b" not found in RUNPATH [",
                &old,
                b"] nor under ",
                &fx.prefix,
            ]);
            fx.error(line);
            continue;
        };
        // Q-FIX-13: libc's dir keeps serving by soname, and its `keep` is left as it is
        if !exists_or_abort(&fspath::join(&runpath[ri].dir, b"libc.so.6")) {
            runpath[ri].keep = false;
            direct.push((ni, ri));
        }
    }
    let target = |runpath: &[RunpathDir], (ni, ri): (usize, usize)| -> Vec<u8> {
        concat(&[&runpath[ri].entry, b"/", &dynamic.needed[ni].name])
    };
    let mut neu = render(&runpath);
    let mut blob = neu.clone();
    for &dep in &direct {
        blob.push(0);
        blob.extend_from_slice(&target(&runpath, dep));
    }
    if blob.len() > old.len() {
        // the direct NEEDED do not fit: they stay sonames and their dirs on RUNPATH, silently
        // (Q-FIX-16)
        for &(_, ri) in &direct {
            runpath[ri].keep = true;
        }
        direct.clear();
        neu = render(&runpath);
        blob.clone_from(&neu);
    }
    if blob != old {
        let old_end = runpath_offset.wrapping_add(old.len() as u64);
        if let Some(sym) = symbol_inside(img, sections, runpath_offset, old_end) {
            fx.error(concat(&[
                path,
                b": symbol '",
                &sym,
                b"' shares bytes with RUNPATH (suffix-merged .dynstr, not lld?)",
            ]));
            return false;
        }
        // the slot is the old string plus its NUL
        if !img.write_padded(runpath_offset, (old.len() as u64).wrapping_add(1), &blob) {
            let line = format!(": RUNPATH does not fit ({} > {}): ", blob.len(), old.len());
            fx.error(concat(&[path, line.as_bytes(), &neu]));
            return false;
        }
        let mut str_off = runpath_offset
            .wrapping_add(neu.len() as u64)
            .wrapping_add(1)
            .wrapping_sub(dynamic.dynstr);
        for &dep in &direct {
            let lib = &dynamic.needed[dep.0];
            img.write_u64_le(lib.val_offset, str_off);
            for &vn in &lib.vn_files {
                img.write_u32_le(vn, str_off as u32);
            }
            str_off = str_off
                .wrapping_add(target(&runpath, dep).len() as u64)
                .wrapping_add(1);
        }
        *dirty = true;
    }
    log.push(concat(&[b"RUNPATH ", &neu]));
    for &dep in &direct {
        log.push(concat(&[b"NEEDED ", &target(&runpath, dep)]));
    }
    true
}

/// A5.8 `FileOffset(vaddr)`: through the first PT_LOAD that maps it; failed reads skip.
fn file_offset(img: &Image, eh: &Ehdr, vaddr: u64) -> Option<u64> {
    for i in 0..eh.e_phnum {
        let Some(raw) = img.read::<PHDR_SIZE>(eh.ph_off(i)) else {
            continue;
        };
        let (p_type, p_offset, p_vaddr, p_filesz) =
            (le32(&raw, 0), le64(&raw, 8), le64(&raw, 16), le64(&raw, 32));
        if p_type == PT_LOAD && vaddr >= p_vaddr && vaddr.wrapping_sub(p_vaddr) < p_filesz {
            return Some(p_offset.wrapping_add(vaddr.wrapping_sub(p_vaddr)));
        }
    }
    None
}

/// A5.8: the stub's entry, `e_entry` itself after a `RELOCSTB` header, else `__reloc_start`.
fn find_reloc_start(img: &Image, eh: &Ehdr, sections: &[Section]) -> Option<u64> {
    if eh.e_entry >= RELOC_STUB_HEADER
        && let Some(off) = file_offset(img, eh, eh.e_entry.wrapping_sub(RELOC_STUB_HEADER))
    {
        if off > img.len() {
            // Q-FIX-10: `string_view::substr` with a start past the end throws
            abort_now();
        }
        let at = &img.bytes[off as usize..];
        if at[..at.len().min(RELOC_STUB_MAGIC.len())] == *RELOC_STUB_MAGIC {
            return Some(eh.e_entry);
        }
    }
    let (sym, strtab) = dynsym(sections)?;
    let stop = section_end(img, sym);
    let mut off = sym.offset;
    while off < stop {
        // here a failed read stops the walk
        let raw = img.read::<24>(off)?;
        if img.cstring(strtab.wrapping_add(u64::from(le32(&raw, 0)))) == b"__reloc_start" {
            return Some(le64(&raw, 8));
        }
        off = off.wrapping_add(sym.entsize);
    }
    None
}

/// A5.9. `false` when the file failed (already reported and counted).
fn fix_interp(
    fx: &mut Fixup,
    path: &[u8],
    img: &mut Image,
    eh: &Ehdr,
    sections: &[Section],
    log: &mut Vec<Vec<u8>>,
    dirty: &mut bool,
) -> bool {
    let stub = find_reloc_start(img, eh, sections);
    for i in 0..eh.e_phnum {
        let ph_off = eh.ph_off(i);
        let Some(mut raw) = img.read::<PHDR_SIZE>(ph_off) else {
            break;
        };
        if le32(&raw, 0) != PT_INTERP {
            continue;
        }
        let ioff = le64(&raw, 8);
        let isz = le64(&raw, 32);
        let old = img.cstring(ioff).to_vec();
        let Some(stub) = stub else {
            log.push(concat(&[b"INTERP ", &old, b" kept (no stub)"]));
            continue;
        };
        let mut neu = old.clone();
        if is_store_path(&old) {
            neu = relative_to(&fx.final_path(&fspath::parent_path(path)), &old, b"");
            if !img.write_padded(ioff, isz, &neu) {
                fx.error(concat(&[path, b": interp does not fit: ", &neu]));
                return false;
            }
        }
        raw[..4].copy_from_slice(&PT_NULL.to_le_bytes());
        img.overwrite(ph_off, &raw);
        // the header as read in A5.1, whatever has been written over it since
        let mut new_eh = eh.raw;
        new_eh[24..32].copy_from_slice(&stub.to_le_bytes());
        img.overwrite(0, &new_eh);
        *dirty = true;
        log.push(concat(&[
            b"INTERP ",
            &neu,
            b" (PT_NULL, entry=__reloc_start)",
        ]));
    }
    true
}

/// A5. `false` is "not mine": not ELF64-LE, or a failed write-back.
pub fn fix_elf(fx: &mut Fixup, path: &[u8], rel: &[u8], img: &mut Image) -> bool {
    if !img.is_elf64_le() {
        return false;
    }
    let Some(raw) = img.read::<EHDR_SIZE>(0) else {
        return true;
    };
    let eh = Ehdr::parse(raw);
    // e_machine is not checked
    if eh.e_type != ET_EXEC && eh.e_type != ET_DYN {
        return true;
    }
    let sections = read_sections(img, &eh);
    let mut log = vec![rel.to_vec()];
    let mut dirty = false;
    if !fix_runpath(fx, path, img, &sections, &mut log, &mut dirty) {
        return true;
    }
    if !fix_interp(fx, path, img, &eh, &sections, &mut log, &mut dirty) {
        return true;
    }
    if dirty {
        if !write_back(&mut fx.out, path, &img.bytes) {
            fx.count_error();
            return false;
        }
        fx.out.line(join(&log, b"  "));
    }
    let needle = concat(&[STORE_DIR, b"/"]);
    if let Some(k) = find(&img.bytes, &needle) {
        let text = img.cstring(k as u64);
        let text = &text[..text.len().min(LEAK_CONTEXT)];
        let at = format!(" @{k:#x}: ");
        fx.out.line(concat(&[
            b"  WARN absolute store ref in ",
            rel,
            at.as_bytes(),
            text,
        ]));
    }
    true
}
