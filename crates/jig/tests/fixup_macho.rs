//! The Mach-O fixup (A6) through the `jig` binary. First the spec's vectors
//! (`behaviors/jig-reloc-fixup.md §Test vectors`, *From TestMachOFixup* and *Derived and
//! confirmed: Mach-O re-signing*); then, gated on `JIG_LEGACY_JIG`, images compared with legacy
//! (`implementation/12-testing.md`): a golden tree of load-command, header-room and signature
//! cases under several invocations, the aborts, and mutants. The images are built here byte by
//! byte, in the layout of `jig_test.cc`'s `macho` namespace; nothing needs macOS.
#![allow(clippy::unwrap_used)]

#[path = "fixtures/fixup/common.rs"]
mod common;

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use common::{
    DEST, Rng, Run, Sink, arg, chmod, difference, difftest, get, interesting, legacy, nix_store,
    put, run_at, run_mode, scratch, write,
};

const MAGIC_64: u64 = 0xfeed_facf;
const LC_SEGMENT_64: u32 = 0x19;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_ID_DYLIB: u32 = 0xd;
const LC_UUID: u32 = 0x1b;
const LC_CODE_SIGNATURE: u32 = 0x1d;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_RPATH: u32 = 0x8000_001c;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001f;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CODE_DIRECTORY: u32 = 0xfade_0c02;

/// The vectors' `OUT` and `ZLIB`.
const OUT: &str = "/nix/store/9123456789abcdfghijklmnpqrsvwxyz-openssl";
const ZLIB: &str = "/nix/store/7123456789abcdfghijklmnpqrsvwxyz-zlib/lib/libz.1.dylib";
const ZLIB_REL: &str = "@loader_path/../../7123456789abcdfghijklmnpqrsvwxyz-zlib/lib/libz.1.dylib";
const FILE_SIZE: usize = 2048;
/// Where `__text` starts in a roomy image.
const ROOMY: u32 = 1024;
/// `dataoff` of the vectors' signature.
const SIG_OFF: usize = 1536;

fn le(b: &mut [u8], at: usize, width: usize, v: u64) {
    put(b, at, width, v, false);
}

fn be(b: &mut [u8], at: usize, v: u32) {
    put(b, at, 4, u64::from(v), true);
}

/// A path command: `cmd`, `cmdsize`, the string's offset `name_off` (24 in a `dylib_command`, 12
/// in an `rpath_command`), the path, NUL padding to `((len + 8) / 8) × 8`.
fn path_command(cmd: u32, name_off: usize, path: &[u8]) -> Vec<u8> {
    let mut b = vec![0; name_off];
    b.extend_from_slice(path);
    b.resize((b.len() + 8) / 8 * 8, 0);
    let n = b.len() as u64;
    le(&mut b, 0, 4, cmd.into());
    le(&mut b, 4, 4, n);
    le(&mut b, 8, 4, name_off as u64);
    b
}

fn dylib_cmd(cmd: u32, path: &str) -> Vec<u8> {
    path_command(cmd, 24, path.as_bytes())
}

fn load(path: &str) -> Vec<u8> {
    dylib_cmd(LC_LOAD_DYLIB, path)
}

fn id(path: &str) -> Vec<u8> {
    dylib_cmd(LC_ID_DYLIB, path)
}

fn rpath(path: &str) -> Vec<u8> {
    path_command(LC_RPATH, 12, path.as_bytes())
}

/// `cmd` with its string offset field overwritten by `name_off`.
fn with_name_off(mut cmd: Vec<u8>, name_off: u32) -> Vec<u8> {
    le(&mut cmd, 8, 4, name_off.into());
    cmd
}

/// LC_CODE_SIGNATURE (`linkedit_data_command`).
fn code_signature(dataoff: u32, datasize: u32) -> Vec<u8> {
    let mut b = vec![0; 16];
    le(&mut b, 0, 4, LC_CODE_SIGNATURE.into());
    le(&mut b, 4, 4, 16);
    le(&mut b, 8, 4, dataoff.into());
    le(&mut b, 12, 4, datasize.into());
    b
}

/// A command no step looks into (LC_UUID).
fn uuid() -> Vec<u8> {
    let mut b = vec![0x5a; 24];
    le(&mut b, 0, 4, LC_UUID.into());
    le(&mut b, 4, 4, 24);
    b
}

/// `segment_command_64` with one `section_64` per entry of `sections` (its `offset` field).
fn segment(fileoff: u64, filesize: u64, sections: &[u32]) -> Vec<u8> {
    let mut b = vec![0; 72 + 80 * sections.len()];
    let n = b.len() as u64;
    le(&mut b, 0, 4, LC_SEGMENT_64.into());
    le(&mut b, 4, 4, n);
    le(&mut b, 40, 8, fileoff);
    le(&mut b, 48, 8, filesize);
    le(&mut b, 64, 4, sections.len() as u64);
    for (i, &o) in sections.iter().enumerate() {
        le(&mut b, 72 + 80 * i + 48, 4, o.into());
    }
    b
}

/// A `size`-byte image: the header (`ncmds` counts `cmds`) and the commands.
fn macho(size: usize, cmds: &[Vec<u8>]) -> Vec<u8> {
    let body = cmds.concat();
    let mut f = vec![0; size];
    le(&mut f, 0, 4, MAGIC_64);
    le(&mut f, 16, 4, cmds.len() as u64);
    le(&mut f, 20, 4, body.len() as u64);
    f[32..32 + body.len()].copy_from_slice(&body);
    f
}

/// `macho::Dylib`: a FILE_SIZE image whose __TEXT maps all of it, with one section at
/// `text_off`, then `cmds`.
fn dylib(text_off: u32, cmds: &[Vec<u8>]) -> Vec<u8> {
    let mut all = vec![segment(0, FILE_SIZE as u64, &[text_off])];
    all.extend_from_slice(cmds);
    macho(FILE_SIZE, &all)
}

/// A 4096-byte [`dylib`] for many commands, `__text` at 3072.
fn big(cmds: &[Vec<u8>]) -> Vec<u8> {
    let mut all = vec![segment(0, 4096, &[3072])];
    all.extend_from_slice(cmds);
    macho(4096, &all)
}

/// The commands of `TestMachOFixup` after the segment.
fn openssl_cmds() -> Vec<Vec<u8>> {
    vec![
        id("@rpath/libssl.3.dylib"),
        load("@rpath/libcrypto.3.dylib"),
        load(ZLIB),
        load("/usr/lib/libSystem.B.dylib"),
    ]
}

/// Every `cmdsize`, walking `ncmds` commands.
fn cmdsizes(f: &[u8]) -> Vec<u64> {
    let mut off = 32;
    let mut out = Vec::new();
    for _ in 0..get(f, 16, 4, false) {
        let n = get(f, off + 4, 4, false);
        out.push(n);
        off += n as usize;
    }
    out
}

/// A CodeDirectory's fields the fixup reads.
#[derive(Clone, Copy)]
struct Cd {
    hash_offset: u32,
    slots: u32,
    limit: u32,
    hash_size: u8,
    hash_type: u8,
    page_shift: u8,
}

impl Cd {
    fn sha256(slots: u32, limit: u32, page_shift: u8) -> Self {
        Cd {
            hash_offset: 88,
            slots,
            limit,
            hash_size: 32,
            hash_type: 2,
            page_shift,
        }
    }

    /// The bytes it takes, laid out with the standard hash offset.
    fn len(self) -> usize {
        88 + 32 * self.slots as usize
    }
}

/// An embedded-signature SuperBlob at `at`: one CodeDirectory per `cds` (the first in the
/// CodeDirectory slot, the rest alternates), with `requirements` an empty requirements blob
/// last, which nothing hashes. Returns where each CodeDirectory is.
fn superblob(f: &mut [u8], at: usize, cds: &[Cd], requirements: bool) -> Vec<usize> {
    let n = cds.len() + usize::from(requirements);
    let mut next = 12 + 8 * n;
    let mut out = Vec::new();
    be(f, at, EMBEDDED_SIGNATURE);
    be(f, at + 8, n as u32);
    for (i, cd) in cds.iter().enumerate() {
        let slot = if i == 0 { 0 } else { 0x1000 + i as u32 - 1 };
        be(f, at + 12 + 8 * i, slot);
        be(f, at + 16 + 8 * i, next as u32);
        let c = at + next;
        be(f, c, CODE_DIRECTORY);
        be(f, c + 4, cd.len() as u32);
        be(f, c + 8, 0x0002_0400);
        be(f, c + 16, cd.hash_offset);
        be(f, c + 28, cd.slots);
        be(f, c + 32, cd.limit);
        put(f, c + 36, 1, cd.hash_size.into(), false);
        put(f, c + 37, 1, cd.hash_type.into(), false);
        put(f, c + 39, 1, cd.page_shift.into(), false);
        out.push(c);
        next += cd.len();
    }
    if requirements {
        be(f, at + 12 + 8 * cds.len(), 2);
        be(f, at + 16 + 8 * cds.len(), next as u32);
        be(f, at + next, 0xfade_0c01);
        be(f, at + next + 4, 12);
        next += 12;
    }
    be(f, at + 4, next as u32);
    out
}

/// A roomy image with `cmds` signed at SIG_OFF with `cds`.
fn signed(cmds: &[Vec<u8>], cds: &[Cd], requirements: bool) -> Vec<u8> {
    let mut all = cmds.to_vec();
    all.push(code_signature(SIG_OFF as u32, 512));
    let mut f = dylib(ROOMY, &all);
    superblob(&mut f, SIG_OFF, cds, requirements);
    f
}

/// The re-signing vector: `TestMachOFixup` plus two LC_RPATHs and a signature of three 512-byte
/// pages, the CodeDirectory at +20. Returns the image and where the CodeDirectory is.
fn signed_openssl(hash_type: u8) -> (Vec<u8>, usize) {
    let mut cmds = openssl_cmds();
    cmds.push(rpath(&format!("{OUT}/lib")));
    cmds.push(rpath("@loader_path/../lib"));
    cmds.push(code_signature(SIG_OFF as u32, 224));
    let mut f = dylib(ROOMY, &cmds);
    let cd = Cd {
        hash_type,
        ..Cd::sha256(3, SIG_OFF as u32, 9)
    };
    let at = superblob(&mut f, SIG_OFF, &[cd], false);
    assert_eq!(at, [SIG_OFF + 20]);
    (f, at[0])
}

/// Does each of the `slots` code slots of the CodeDirectory at `cd` hold the SHA-256 of its page?
fn slots_hash_pages(f: &[u8], cd: usize, slots: usize, page: usize, limit: usize) -> bool {
    (0..slots).all(|s| {
        let begin = s * page;
        let end = (begin + page).min(limit);
        f[cd + 88 + 32 * s..][..32] == Sha256::digest(&f[begin..end])[..]
    })
}

fn fixup(cwd: &Path, args: &[Vec<u8>]) -> Run {
    run_mode(
        Path::new(common::JIG),
        cwd,
        args,
        Sink::File,
        Duration::from_secs(60),
    )
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// §Test vectors, *From TestMachOFixup* (the port of `legacy/jig/src/jig_test.cc:570`), and
/// *Derived and confirmed: Mach-O re-signing*.
#[test]
fn test_mach_o_fixup() {
    if !nix_store() {
        return;
    }
    let dir = scratch("v-macho");
    let p = dir.join("p");
    let file_path = p.join("lib/libssl.3.dylib");
    let run = |file: &[u8], extra: &[Vec<u8>]| -> (Run, Vec<u8>) {
        write(&file_path, file);
        write(&p.join("lib/libcrypto.3.dylib"), b"");
        let mut args = vec![arg(&p), arg("--dest"), arg(OUT)];
        args.extend_from_slice(extra);
        let r = fixup(&dir, &args);
        (r, std::fs::read(&file_path).unwrap())
    };

    let file = dylib(ROOMY, &openssl_cmds());
    let (r, after) = run(&file, &[]);
    assert_eq!(
        text(&r.stdout),
        format!(
            "lib/libssl.3.dylib  {OUT}/lib/libssl.3.dylib  @loader_path/libcrypto.3.dylib  {ZLIB_REL}\n"
        )
    );
    assert_eq!(text(&r.stderr), "");
    assert_eq!(r.code, Some(0));
    assert_eq!(after.len(), FILE_SIZE);
    assert_eq!(cmdsizes(&after), [152, 96, 56, 104, 56]);
    assert_eq!(get(&after, 20, 4, false), 464);
    // zeroed from the end of the commands to the room bound, the rest untouched
    assert!(after[32 + 464..1024].iter().all(|&c| c == 0));
    assert_eq!(after[1024..], file[1024..]);
    let has = |s: &str| after.windows(s.len()).any(|w| w == s.as_bytes());
    assert!(has(&format!("{OUT}/lib/libssl.3.dylib\0")));
    assert!(has("@loader_path/libcrypto.3.dylib\0"));
    assert!(has(ZLIB_REL));
    assert!(has("/usr/lib/libSystem.B.dylib\0"));
    assert!(!has(ZLIB));

    // loads dyld could not resolve; a system library the SDK has passes
    let sdk = dir.join("sdk");
    write(&sdk.join("usr/lib/libSystem.B.tbd"), b"");
    for (load_path, error) in [
        ("@rpath/libgone.dylib", true),
        ("@loader_path/libgone.dylib", true),
        ("/usr/lib/libgone.dylib", true),
        ("/usr/lib/libSystem.B.dylib", false),
    ] {
        let lost = dylib(ROOMY, &[load(load_path)]);
        let (r, after) = run(&lost, &[arg("--sdk"), arg(&sdk)]);
        let want = if error {
            format!(
                "reloc-fixup: lib/libssl.3.dylib: links {load_path}, which neither the package, a dependency nor the SDK has\n"
            )
        } else {
            String::new()
        };
        assert_eq!(text(&r.stderr), want, "{load_path}");
        assert_eq!(r.code, Some(i32::from(error)));
        assert!(r.stdout.is_empty());
        assert_eq!(after, lost);
    }

    // __text right behind the commands: the longer zlib spelling does not fit
    let grows = load(ZLIB);
    assert_eq!(grows.len(), 96);
    let tight = dylib(32 + 72 + 80 + 96, &[grows]);
    let (r, after) = run(&tight, &[]);
    assert_eq!(
        text(&r.stderr),
        "reloc-fixup: lib/libssl.3.dylib: load commands need 256 bytes, the header has room for 248 (-headerpad)\n"
    );
    assert_eq!(r.code, Some(1));
    assert!(r.stdout.is_empty());
    assert_eq!(after, tight);

    // re-signing
    let (file, cd) = signed_openssl(2);
    assert!(!slots_hash_pages(&file, cd, 3, 512, SIG_OFF));
    let (r, after) = run(&file, &[]);
    assert_eq!(
        text(&r.stdout),
        format!(
            "lib/libssl.3.dylib  {OUT}/lib/libssl.3.dylib  @loader_path/libcrypto.3.dylib  {ZLIB_REL}  @loader_path\n"
        )
    );
    assert_eq!(r.code, Some(0));
    assert_eq!(cmdsizes(&after), [152, 96, 56, 104, 56, 32, 32, 16]);
    assert_eq!(get(&after, 20, 4, false), 544);
    assert_eq!(get(&after, 16, 4, false), 8);
    assert!(slots_hash_pages(&after, cd, 3, 512, SIG_OFF));
    let (file, _) = signed_openssl(1);
    let (r, after) = run(&file, &[]);
    assert_eq!(
        text(&r.stderr),
        "reloc-fixup: lib/libssl.3.dylib: code signature hash type 1 is not SHA-256\n"
    );
    assert_eq!(r.code, Some(1));
    assert!(r.stdout.is_empty());
    assert_eq!(after, file);

    // Q-FIX-10: a page size shift legacy cannot compute is refused like a foreign hash type;
    // without code slots nothing needs the page size and the image is re-signed as before
    let cmds = openssl_cmds();
    let file = signed(&cmds, &[Cd::sha256(1, 512, 64)], false);
    let (r, after) = run(&file, &[]);
    assert_eq!(
        text(&r.stderr),
        "reloc-fixup: lib/libssl.3.dylib: code signature page size shift 64 is out of range\n"
    );
    assert_eq!((r.code, after), (Some(1), file));
    let file = signed(&cmds, &[Cd::sha256(0, 512, 200)], false);
    let (r, after) = run(&file, &[]);
    assert_eq!((r.code, text(&r.stderr)), (Some(0), String::new()));
    assert_ne!(after, file);
    common::wipe(&dir);
}

/// The golden Mach-O tree for a prefix at `w` (some commands name `w` itself).
fn macho_tree(w: &Path) {
    let wl = format!("{}/lib", w.display());
    let lib = |name: &str, f: &[u8]| write(&w.join("lib").join(name), f);
    let sha = Cd::sha256;
    // the vectors
    lib("libssl.3.dylib", &dylib(ROOMY, &openssl_cmds()));
    lib(
        "libcrypto.3.dylib",
        &dylib(
            ROOMY,
            &[
                id("@rpath/libcrypto.3.dylib"),
                load(ZLIB),
                load("/usr/lib/libSystem.B.dylib"),
            ],
        ),
    );
    lib("libsigned.dylib", &signed_openssl(2).0);
    lib("libsha1.dylib", &signed_openssl(1).0);
    lib("libtight.dylib", &dylib(280, &[load(ZLIB)]));
    // every spelling of a load
    lib(
        "libloads.dylib",
        &big(&[
            load("@rpath/libgone.dylib"),
            load("@rpath/libcrypto.3.dylib"),
            load("@rpath/../lib/libssl.3.dylib"),
            load("@loader_path/libgone.dylib"),
            load("@loader_path/libcrypto.3.dylib"),
            load("@loader_path/../lib/./libssl.3.dylib"),
            load("@loader_path/"),
            load("@loader_path"),
            load("@executable_path/../lib/libx.dylib"),
            load("/usr/lib/libgone.dylib"),
            load("/usr/lib/libSystem.B.dylib"),
            load("/usr/lib/libz.1"),
            load("/System/Library/Frameworks/Foo.framework/Foo"),
            load("/opt/lib/libfoo.dylib"),
            load("/nix/store/x"),
            load(ZLIB),
            load(&format!("{wl}/libnothere.dylib")),
            load(&format!("{wl}/libcrypto.3.dylib")),
            load(&format!("{wl}/../lib//libssl.3.dylib")),
            load(&format!("{DEST}/lib/libcrypto.3.dylib")),
            load(&format!("{DEST}/lib/libnothere.dylib")),
            load("relative/libr.dylib"),
            load(""),
        ]),
    );
    // the other path commands, odd string offsets, commands no step reads
    lib(
        "libkinds.dylib",
        &big(&[
            id(""),
            dylib_cmd(LC_LOAD_WEAK_DYLIB, &format!("{wl}/libcrypto.3.dylib")),
            dylib_cmd(LC_REEXPORT_DYLIB, "@rpath/libssl.3.dylib"),
            dylib_cmd(LC_LOAD_UPWARD_DYLIB, ZLIB),
            dylib_cmd(LC_LOAD_UPWARD_DYLIB, "@rpath/libnope.dylib"),
            uuid(),
            with_name_off(load(ZLIB), 1000),
            with_name_off(load(ZLIB), 0),
            with_name_off(load(ZLIB), 96),
            with_name_off(load(ZLIB), 95),
            with_name_off(id("x"), 4),
            with_name_off(id("x"), 2),
            with_name_off(rpath(&format!("{OUT}/lib")), 13),
            rpath(&wl),
            rpath(&format!("{}/lib/.", w.display())),
            rpath(&format!("{}/nothere", w.display())),
            rpath(&format!("{}/", w.display())),
            rpath("@loader_path/../lib"),
            rpath("/usr/local/lib"),
            rpath("/nix/store/0123456789abcdfghijklmnpqrsvwxyz-dep/lib"),
            rpath(&format!("{DEST}/lib")),
            rpath("/nix/store"),
        ]),
    );
    write(
        &w.join("bin/tool"),
        &big(&[
            load("@rpath/libssl.3.dylib"),
            load("@rpath/libnope.dylib"),
            load(&format!("{wl}/libcrypto.3.dylib")),
            rpath("@loader_path/../lib"),
            rpath(&wl),
        ]),
    );
    // header room
    let mut noseg = macho(1024, &[id("@rpath/libnoseg.dylib"), load(ZLIB)]);
    noseg[400..].fill(0xa5);
    lib("libnoseg.dylib", &noseg);
    let mut trunc = macho(
        FILE_SIZE,
        &[
            segment(0, FILE_SIZE as u64, &[ROOMY]),
            load("@rpath/libcrypto.3.dylib"),
            path_command(0x99, 8, b""),
            load(ZLIB),
        ],
    );
    let bad = 32 + 152 + 56;
    le(&mut trunc, bad + 4, 4, 4);
    lib("libtrunc.dylib", &trunc);
    let mut past = macho(
        600,
        &[
            segment(0, 600, &[500]),
            load("@rpath/libcrypto.3.dylib"),
            uuid(),
        ],
    );
    le(&mut past, 32 + 152 + 56 + 4, 4, 1000);
    lib("libpast.dylib", &past);
    lib(
        "libsegs.dylib",
        &macho(
            FILE_SIZE,
            &[
                segment(0, 0, &[]),
                segment(0, FILE_SIZE as u64, &[0, 1500, 900]),
                segment(700, 100, &[]),
                segment(800, 0, &[]),
                id("@rpath/libsegs.dylib"),
            ],
        ),
    );
    lib("libwrap.dylib", &dylib(16, &[id("@rpath/libwrap.dylib")]));
    lib(
        "libwrap2.dylib",
        &macho(
            FILE_SIZE,
            &[segment(8, 64, &[]), id("@rpath/libwrap2.dylib")],
        ),
    );
    lib(
        "libnosect.dylib",
        &macho(
            FILE_SIZE,
            &[segment(0, 64, &[]), id("@rpath/libnosect.dylib")],
        ),
    );
    // signatures
    let one = [id("@rpath/libsig.dylib")];
    let mut badsig = dylib(ROOMY, &[one[0].clone(), code_signature(SIG_OFF as u32, 16)]);
    badsig[SIG_OFF..SIG_OFF + 4].copy_from_slice(&[0xfa, 0xde, 0x0c, 0xc1]);
    lib("libbadsig.dylib", &badsig);
    lib(
        "libsigeof.dylib",
        &dylib(ROOMY, &[one[0].clone(), code_signature(5000, 16)]),
    );
    lib(
        "libsigend.dylib",
        &dylib(ROOMY, &[one[0].clone(), code_signature(2046, 16)]),
    );
    lib(
        "libslotout.dylib",
        &signed(
            &one,
            &[Cd {
                hash_offset: 0x7fff_0000,
                ..sha(3, 1536, 9)
            }],
            true,
        ),
    );
    lib(
        "libslotlast.dylib",
        &signed(
            &one,
            &[Cd {
                hash_offset: 512 - 20 - 32 - 8,
                ..sha(3, 1536, 9)
            }],
            false,
        ),
    );
    let mut multi = dylib(
        ROOMY,
        &[
            one[0].clone(),
            code_signature(100, 0),
            code_signature(1200, 448),
        ],
    );
    superblob(&mut multi, 1200, &[sha(2, 1200, 9), sha(5, 1200, 8)], true);
    lib("libmulti.dylib", &multi);
    let mut notcd = signed(&one, &[sha(3, 1536, 9)], false);
    be(&mut notcd, SIG_OFF + 16, 4);
    lib("libnotcd.dylib", &notcd);
    let mut count0 = signed(&one, &[sha(3, 1536, 9)], false);
    be(&mut count0, SIG_OFF + 8, 0);
    lib("libcount0.dylib", &count0);
    let mut count9 = signed(&one, &[sha(3, 1536, 9)], false);
    be(&mut count9, SIG_OFF + 8, 9);
    lib("libcount9.dylib", &count9);
    lib("libendwrap.dylib", &signed(&one, &[sha(3, 700, 9)], false));
    lib("libshift.dylib", &signed(&one, &[sha(10, 160, 4)], false));
    lib("libshift0.dylib", &signed(&one, &[sha(9, 7, 0)], false));
    lib("libslots0.dylib", &signed(&one, &[sha(0, 1536, 70)], true));
    lib(
        "libbiglimit.dylib",
        &signed(&one, &[sha(2, u32::MAX, 10)], false),
    );
    lib("liblimit0.dylib", &signed(&one, &[sha(2, 0, 9)], false));
    lib(
        "libsha1alt.dylib",
        &signed(
            &one,
            &[
                sha(3, 1536, 9),
                Cd {
                    hash_size: 20,
                    hash_type: 1,
                    ..sha(3, 1536, 9)
                },
            ],
            true,
        ),
    );
    lib(
        "libsize.dylib",
        &signed(
            &one,
            &[Cd {
                hash_size: 20,
                ..sha(3, 1536, 9)
            }],
            false,
        ),
    );
    // not Mach-O 64-bit little-endian, or unchanged
    let mut x32 = dylib(ROOMY, &openssl_cmds());
    le(&mut x32, 0, 4, 0xfeed_face);
    lib("x32.dylib", &x32);
    let mut fat = dylib(ROOMY, &openssl_cmds());
    be(&mut fat, 0, 0xcafe_babe);
    lib("fat.dylib", &fat);
    lib("short.dylib", &dylib(ROOMY, &openssl_cmds())[..31]);
    lib("header.dylib", &macho(32, &[]));
    lib(
        "libsys.dylib",
        &dylib(ROOMY, &[load("/usr/lib/libSystem.B.dylib")]),
    );
    lib("libempty.dylib", b"");
    lib("foo.debug", &dylib(ROOMY, &openssl_cmds()));
    // read-only file and directory, unreadable file
    lib("libro.dylib", &dylib(ROOMY, &openssl_cmds()));
    chmod(&w.join("lib/libro.dylib"), 0o444);
    write(&w.join("ro/libro.dylib"), &dylib(ROOMY, &openssl_cmds()));
    chmod(&w.join("ro"), 0o555);
    write(&w.join("secret.dylib"), &dylib(ROOMY, &openssl_cmds()));
    chmod(&w.join("secret.dylib"), 0);
    // an SDK
    for tbd in [
        "usr/lib/libSystem.B.tbd",
        "usr/lib/libz.1.tbd",
        "System/Library/Frameworks/Foo.framework/Foo.tbd",
    ] {
        write(&w.join("sdk").join(tbd), b"");
    }
}

/// The golden tree under several invocations.
#[test]
fn macho_tree_difftest() {
    let Some(old) = legacy() else { return };
    let dir = scratch("machotree");
    let work = dir.join("w");
    let w = arg(&work);
    let sdk = arg(work.join("sdk"));
    let invocations: Vec<(&str, Vec<Vec<u8>>)> = vec![
        ("dest", vec![w.clone(), arg("--dest"), arg(DEST)]),
        (
            "sdk",
            vec![
                w.clone(),
                arg("--dest"),
                arg(DEST),
                arg("--sdk"),
                sdk.clone(),
            ],
        ),
        (
            "missing sdk",
            vec![
                w.clone(),
                arg("--sdk"),
                arg(dir.join("nosdk")),
                arg("--dest"),
                arg(DEST),
            ],
        ),
        (
            "relative prefix and sdk",
            vec![
                arg("w"),
                arg("--sdk"),
                arg("w/sdk"),
                arg("--dest"),
                arg(OUT),
            ],
        ),
        (
            "trailing slashes",
            vec![
                arg(format!("{}/", work.display())),
                arg("--dest"),
                arg(format!("{DEST}//")),
            ],
        ),
        (
            "deep dest",
            vec![
                w.clone(),
                arg("--dest"),
                arg("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bar/sub/dir"),
            ],
        ),
        (
            "store root dest",
            vec![w.clone(), arg("--dest"), arg("/nix/store/")],
        ),
        (
            "deny",
            vec![
                w.clone(),
                arg("--dest"),
                arg(OUT),
                arg("--deny"),
                arg(ZLIB),
                arg("--deny"),
                arg("/nix/store/x/"),
            ],
        ),
    ];
    for (name, args) in &invocations {
        let l = difftest(&old, name, &work, &dir, args, &macho_tree, Sink::File);
        if *name == "sdk" {
            // the tree exercised what it is meant to
            let out = text(&l.stdout);
            let err = text(&l.stderr);
            for want in [
                "lib/libssl.3.dylib  ",
                "lib/libsigned.dylib  ",
                "lib/libmulti.dylib  ",
                "lib/libendwrap.dylib  ",
                "lib/libkinds.dylib  ",
                "lib/libnoseg.dylib  ",
                "lib/libtrunc.dylib  ",
                "lib/libslots0.dylib  ",
                "bin/tool  ",
            ] {
                assert!(out.contains(want), "{want}: {out}");
            }
            for want in [
                "links @rpath/libgone.dylib, which",
                "links /usr/lib/libgone.dylib, which",
                "the header has room for",
                "is no embedded signature blob",
                "code signature hash type 1 is not SHA-256",
                "code signature hash type 2 is not SHA-256",
                "lies outside the file",
                "cannot write back: No such file or directory",
            ] {
                assert!(err.contains(want), "{want}: {err}");
            }
            assert_eq!(l.code, Some(1));
        }
    }
    // a second run over legacy's result
    let legacy_bin = old.clone();
    let prep2 = move |w: &Path| {
        macho_tree(w);
        let r = run_mode(
            &legacy_bin,
            w.parent().unwrap(),
            &[arg(w), arg("--dest"), arg(DEST)],
            Sink::File,
            Duration::from_secs(60),
        );
        assert_eq!(r.code, Some(1));
    };
    let l = difftest(
        &old,
        "second run",
        &work,
        &dir,
        &invocations[0].1,
        &prep2,
        Sink::File,
    );
    assert!(!text(&l.stdout).contains("lib/libssl.3.dylib  "));
}

/// Q-FIX-10 aborts on Mach-O input, what reaches stdout before them, and a closed stderr.
#[test]
fn macho_aborts_and_streams() {
    let Some(old) = legacy() else { return };
    let dir = scratch("machoabort");
    let work = dir.join("w");
    let args = vec![arg(&work), arg("--dest"), arg(DEST)];
    let openssl = dylib(ROOMY, &openssl_cmds());
    // slot 1 starts at 4096, past the end of the file
    let beyond = signed(
        &[id("@rpath/libx.dylib")],
        &[Cd::sha256(2, 1536, 12)],
        false,
    );
    let prep = |w: &Path| {
        write(&w.join("lib/libssl.3.dylib"), &openssl);
        write(&w.join("lib/libcrypto.3.dylib"), b"");
        write(&w.join("lib/libbeyond.dylib"), &beyond);
    };
    let l = difftest(
        &old,
        "slot past the end",
        &work,
        &dir,
        &args,
        &prep,
        Sink::File,
    );
    assert_eq!(l.signal, Some(libc::SIGABRT));
    // the slot is not reached when the hash type is refused first
    let refused = signed(
        &[id("@rpath/libx.dylib")],
        &[Cd {
            hash_type: 3,
            ..Cd::sha256(2, 1536, 12)
        }],
        false,
    );
    let prep = |w: &Path| write(&w.join("lib/libx.dylib"), &refused);
    let l = difftest(
        &old,
        "refused before the slot",
        &work,
        &dir,
        &args,
        &prep,
        Sink::File,
    );
    assert_eq!(l.code, Some(1));

    // stdout beyond one buffer before an abort: the aborting file last in readdir order
    let many = |w: &Path| {
        write(&w.join("lib/libcrypto.3.dylib"), b"");
        for i in 0..60 {
            write(&w.join(format!("many/lib{i:02}.dylib")), &openssl);
        }
        let mut names: Vec<_> = std::fs::read_dir(w.join("many"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let last = names.pop().unwrap();
        std::fs::write(last, &beyond).unwrap();
    };
    for sink in [Sink::File, Sink::Pipe] {
        let l = difftest(
            &old,
            "flushed before abort",
            &work,
            &dir,
            &args,
            &many,
            sink,
        );
        assert_eq!(l.signal, Some(libc::SIGABRT));
        assert!(l.stdout.len() >= 4096, "{}", l.stdout.len());
    }

    // a closed stderr: fatal at the first message only
    let quiet = |w: &Path| {
        write(&w.join("lib/libssl.3.dylib"), &openssl);
        write(&w.join("lib/libcrypto.3.dylib"), b"");
    };
    let l = difftest(
        &old,
        "stderr closed, quiet",
        &work,
        &dir,
        &args,
        &quiet,
        Sink::StderrClosed,
    );
    assert_eq!(l.code, Some(0));
    let noisy = |w: &Path| write(&w.join("lib/libssl.3.dylib"), &openssl);
    let l = difftest(
        &old,
        "stderr closed, noisy",
        &work,
        &dir,
        &args,
        &noisy,
        Sink::StderrClosed,
    );
    assert_eq!(l.signal, Some(libc::SIGABRT));

    // a page size shift of 64 or more with code slots: undefined behaviour in legacy, which
    // is not compared; jig refuses it (see `macho.rs`, Q-FIX-10)
    for shift in [64u8, 65, 200, 255] {
        let file = signed(
            &[id("@rpath/libx.dylib")],
            &[Cd::sha256(2, 1536, shift)],
            false,
        );
        let prep = |w: &Path| write(&w.join("lib/libx.dylib"), &file);
        let limit = Duration::from_secs(60);
        let l = run_at(&old, &work, &dir, &args, &prep, Sink::File, limit);
        let r = run_at(
            Path::new(common::JIG),
            &work,
            &dir,
            &args,
            &prep,
            Sink::File,
            limit,
        );
        eprintln!(
            "page size shift {shift}: legacy {:?}/{:?} stdout {:?} stderr {:?}",
            l.code,
            l.signal,
            text(&l.stdout),
            text(&l.stderr)
        );
        assert_eq!(
            text(&r.stderr),
            format!(
                "reloc-fixup: lib/libx.dylib: code signature page size shift {shift} is out of range\n"
            )
        );
        assert_eq!(r.code, Some(1));
    }
}

/// A field a mutation targets.
#[derive(Clone, Copy)]
struct Field {
    at: usize,
    width: usize,
    big: bool,
    /// A loop count legacy runs out even past the end of the file: kept small.
    count: bool,
}

fn field(at: usize, width: usize) -> Field {
    Field {
        at,
        width,
        big: false,
        count: false,
    }
}

/// The fields and path strings of a seed: header, load commands, segments and sections, and
/// the SuperBlob and its CodeDirectories.
fn macho_fields(f: &[u8]) -> (Vec<Field>, Vec<(usize, usize)>) {
    let mut fields = vec![field(16, 4), field(20, 4), field(12, 4)];
    let mut strings = Vec::new();
    let mut off = 32;
    let mut sig = None;
    for _ in 0..get(f, 16, 4, false) {
        let cmd = get(f, off, 4, false) as u32;
        let size = get(f, off + 4, 4, false) as usize;
        if size < 8 || off + size > f.len() {
            break;
        }
        fields.extend([field(off, 4), field(off + 4, 4)]);
        match cmd {
            LC_SEGMENT_64 => {
                fields.extend([field(off + 40, 8), field(off + 48, 8)]);
                fields.push(Field {
                    count: true,
                    ..field(off + 64, 4)
                });
                for s in 0..get(f, off + 64, 4, false) as usize {
                    fields.push(field(off + 72 + 80 * s + 48, 4));
                }
            }
            LC_CODE_SIGNATURE => {
                fields.extend([field(off + 8, 4), field(off + 12, 4)]);
                sig = Some(get(f, off + 8, 4, false) as usize);
            }
            LC_LOAD_DYLIB | LC_ID_DYLIB | LC_RPATH | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB
            | LC_LOAD_UPWARD_DYLIB => {
                fields.push(field(off + 8, 4));
                let po = get(f, off + 8, 4, false) as usize;
                if po < size {
                    strings.push((off + po, size - po));
                }
            }
            _ => {}
        }
        off += size;
    }
    if let Some(sig) = sig
        && get(f, sig, 4, true) == u64::from(EMBEDDED_SIGNATURE)
    {
        let bf = |at, width| Field {
            big: true,
            ..field(at, width)
        };
        fields.push(Field {
            count: true,
            ..bf(sig + 8, 4)
        });
        for j in 0..get(f, sig + 8, 4, true) as usize {
            let entry = sig + 12 + 8 * j + 4;
            fields.push(bf(entry, 4));
            let cd = sig + get(f, entry, 4, true) as usize;
            if get(f, cd, 4, true) == u64::from(CODE_DIRECTORY) {
                fields.extend([
                    bf(cd + 16, 4),
                    bf(cd + 28, 4),
                    bf(cd + 32, 4),
                    bf(cd + 36, 1),
                    bf(cd + 37, 1),
                    bf(cd + 39, 1),
                ]);
            }
        }
    }
    (fields, strings)
}

/// One mutant of `seed`: one to three field, string or length mutations.
fn mutate_macho(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut b = seed.to_vec();
    let (fields, strings) = macho_fields(seed);
    for _ in 0..=rng.below(3) {
        match rng.below(12) {
            0 => {
                let n = rng.below(b.len() as u64 + 1) as usize;
                b.truncate(n);
            }
            1 | 2 if !strings.is_empty() => {
                let (off, size) = *rng.pick(&strings);
                let at = off + rng.below(size.max(1) as u64) as usize;
                let v = *rng.pick(&[0u8, b'/', b'.', b'@', b'x', 0xff]);
                if let Some(c) = b.get_mut(at) {
                    *c = v;
                }
            }
            _ => {
                let f = *rng.pick(&fields);
                let old = get(&b, f.at, f.width, f.big);
                let mut v = interesting(rng, f.width, b.len() as u64, old);
                if f.count && v > 0xffff {
                    v &= 0x3ff;
                }
                put(&mut b, f.at, f.width, v, f.big);
            }
        }
    }
    b
}

/// Mutated Mach-O images from several seeds, each as `lib/libssl.3.dylib` next to an empty
/// `lib/libcrypto.3.dylib`. A mutant jig refuses for its page size shift is not compared (legacy
/// is undefined there), nor one legacy takes more than the time limit over (a huge loop count
/// read from the wrong bytes).
#[test]
fn macho_mutants() {
    let Some(old) = legacy() else { return };
    let dir = scratch("machomut");
    let work = dir.join("w");
    let sha = Cd::sha256;
    let one = [
        id("@rpath/libsig.dylib"),
        load(ZLIB),
        rpath(&format!("{OUT}/lib")),
    ];
    let mut multi = dylib(
        ROOMY,
        &[one[0].clone(), load(ZLIB), code_signature(1200, 448)],
    );
    superblob(&mut multi, 1200, &[sha(2, 1200, 9), sha(5, 1200, 8)], true);
    let seeds = [
        dylib(ROOMY, &openssl_cmds()),
        signed_openssl(2).0,
        signed(&one, &[sha(10, 160, 4)], true),
        multi,
        macho(
            FILE_SIZE,
            &[
                segment(0, 0, &[]),
                segment(0, FILE_SIZE as u64, &[0, 1500, 900]),
                segment(700, 100, &[]),
                id("@rpath/libsegs.dylib"),
                load("@loader_path/libcrypto.3.dylib"),
            ],
        ),
        macho(
            1024,
            &[
                id("@rpath/libnoseg.dylib"),
                load(ZLIB),
                rpath("@loader_path/../lib"),
            ],
        ),
        dylib(280, &[load(ZLIB)]),
    ];
    let args = vec![arg(&work), arg("--dest"), arg(OUT)];
    let mut rng = Rng(0x00ac_0f1c_5eed);
    let (mut compared, mut aborts, mut changed, mut refused, mut slow) = (0, 0, 0, 0, 0);
    let count = mutant_count(400);
    let limit = Duration::from_secs(20);
    for i in 0..count {
        let m = mutate_macho(&seeds[i % seeds.len()], &mut rng);
        let prep = |w: &Path| {
            write(&w.join("lib/libssl.3.dylib"), &m);
            write(&w.join("lib/libcrypto.3.dylib"), b"");
        };
        let r = run_at(
            Path::new(common::JIG),
            &work,
            &dir,
            &args,
            &prep,
            Sink::File,
            limit,
        );
        assert!(!r.timed_out, "Mach-O mutant {i}: jig timed out");
        if text(&r.stderr).contains("page size shift") {
            refused += 1;
            continue;
        }
        let l = run_at(&old, &work, &dir, &args, &prep, Sink::File, limit);
        if l.timed_out {
            slow += 1;
            continue;
        }
        if let Some(d) = difference(&l, &r) {
            panic!("Mach-O mutant {i}: {d}");
        }
        compared += 1;
        aborts += usize::from(l.signal.is_some());
        changed += usize::from(l.stdout.starts_with(b"lib/libssl.3.dylib  "));
    }
    eprintln!(
        "Mach-O mutants: {compared} compared ({aborts} aborted, {changed} rewritten), {refused} refused for the page size, {slow} too slow in legacy"
    );
}

/// `JIG_FIXUP_MUTANTS` overrides the number of mutants (for a longer run by hand).
fn mutant_count(default: usize) -> usize {
    std::env::var("JIG_FIXUP_MUTANTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
