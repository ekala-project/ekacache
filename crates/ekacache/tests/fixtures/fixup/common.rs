//! Shared by the reloc-fixup integration tests (`fixup_*.rs`, included with `#[path]`): running
//! ec over a tree at one fixed path; snapshots of the tree; field mutation; ELF test binaries
//! (`implementation/12-testing.md`).
#![allow(dead_code, clippy::unwrap_used)]

use ekacache_testkit::busy::CommandBusyExt as _;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const JIG: &str = env!("CARGO_BIN_EXE_ec");

/// The `--dest` of most cases; `STORE_DIR` must be `/nix/store`.
pub const DEST: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo";

/// Is this ec built for `/nix/store`, as the vectors are?
pub fn nix_store() -> bool {
    if ekacache_core::store_path::STORE_DIR == b"/nix/store" {
        return true;
    }
    eprintln!("skipped: the vectors assume JIG_STORE_DIR=/nix/store");
    false
}

/// A fresh scratch directory (not removed when it has `chmod`ed contents left: see [`wipe`]).
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("jig-fixup-{}-{name}", std::process::id()));
    wipe(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `rm -rf`, after making every directory writable and searchable again.
pub fn wipe(p: &Path) {
    fn open_up(p: &Path) {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return;
        };
        if m.is_dir() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    open_up(&e.path());
                }
            }
        }
    }
    open_up(p);
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p).unwrap(),
        Ok(_) => std::fs::remove_file(p).unwrap(),
        Err(_) => {}
    }
}

/// `cp -a src dst` (modes, symlinks and hard links kept).
pub fn copy_tree(src: &Path, dst: &Path) {
    let st = Command::new("cp")
        .arg("-a")
        .arg(src)
        .arg(dst)
        .status_busy()
        .unwrap();
    assert!(st.success(), "cp -a {} {}", src.display(), dst.display());
}

pub fn write(p: &Path, data: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

pub fn chmod(p: &Path, mode: u32) {
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// One file system entry of a snapshot.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Entry {
    /// Relative to the tree, `.jig<pid>.` temp names masked.
    pub path: String,
    /// `f`, `d`, `l` or `?`.
    pub kind: char,
    pub mode: u32,
    pub nlink: u64,
    /// File contents (or `None` if unreadable), or the link target.
    pub data: Option<Vec<u8>>,
}

/// Mask `.jig<digits>.` (write-back temp names) in a path or message.
pub fn mask_tmp(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(b".jig") {
            let digits = text[i + 4..]
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .count();
            if digits > 0 && text.get(i + 4 + digits) == Some(&b'.') {
                out.extend_from_slice(b".jig<pid>.");
                i += 4 + digits + 1;
                continue;
            }
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

/// Every entry under `root`, sorted by path (readdir order does not matter here).
pub fn snapshot(root: &Path) -> Vec<Entry> {
    fn visit(root: &Path, p: &Path, out: &mut Vec<Entry>) {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            return;
        };
        let rel = p.strip_prefix(root).unwrap().as_os_str().as_bytes();
        let path = String::from_utf8_lossy(&mask_tmp(rel)).into_owned();
        let ft = m.file_type();
        let (kind, data) = if ft.is_symlink() {
            let t = std::fs::read_link(p).unwrap();
            ('l', Some(t.as_os_str().as_bytes().to_vec()))
        } else if ft.is_file() {
            ('f', std::fs::read(p).ok())
        } else if ft.is_dir() {
            ('d', None)
        } else {
            ('?', None)
        };
        out.push(Entry {
            path,
            kind,
            mode: m.mode() & 0o7777,
            nlink: if ft.is_dir() { 0 } else { m.nlink() },
            data,
        });
        if ft.is_dir()
            && let Ok(rd) = std::fs::read_dir(p)
        {
            for e in rd.flatten() {
                visit(root, &e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The outcome of one run.
#[derive(Debug)]
pub struct Run {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub tree: Vec<Entry>,
    /// The run was killed after the time limit.
    pub timed_out: bool,
}

/// Where stdout goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// A regular file (fully buffered, st_blksize).
    File,
    /// A pipe.
    Pipe,
    /// `/dev/full`.
    Full,
    /// stderr closed.
    StderrClosed,
}

/// Run `<bin> reloc-fixup <args>` in `cwd`, killing it after `limit`.
pub fn run_mode(bin: &Path, cwd: &Path, args: &[Vec<u8>], sink: Sink, limit: Duration) -> Run {
    let io = scratch_io();
    let out_path = io.join("stdout");
    let err_path = io.join("stderr");
    let mut cmd = if sink == Sink::StderrClosed {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg("exec 2>&-; exec \"$@\"").arg("sh").arg(bin);
        c
    } else {
        Command::new(bin)
    };
    cmd.arg("reloc-fixup")
        .args(args.iter().map(|a| OsStr::from_bytes(a)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env_clear()
        .env("LC_ALL", "C");
    match sink {
        Sink::File | Sink::StderrClosed => {
            cmd.stdout(std::fs::File::create(&out_path).unwrap());
        }
        Sink::Full => {
            cmd.stdout(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap(),
            );
        }
        Sink::Pipe => {
            cmd.stdout(Stdio::piped());
        }
    }
    if sink != Sink::StderrClosed {
        cmd.stderr(std::fs::File::create(&err_path).unwrap());
    }
    let mut child = cmd.spawn_busy().unwrap();
    let mut pipe = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = pipe.as_mut() {
            std::io::Read::read_to_end(p, &mut buf).unwrap();
        }
        buf
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            timed_out = true;
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    let piped = reader.join().unwrap();
    let stdout = if sink == Sink::Pipe {
        piped
    } else {
        std::fs::read(&out_path).unwrap_or_default()
    };
    let stderr = std::fs::read(&err_path).unwrap_or_default();
    wipe(&io);
    Run {
        stdout,
        stderr,
        code: status.code(),
        signal: status.signal(),
        tree: Vec::new(),
        timed_out,
    }
}

fn scratch_io() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    scratch(&format!("io{n}"))
}

/// `bin` over the tree `prep` builds at `work`; the tree is snapshot and removed.
pub fn run_at(
    bin: &Path,
    work: &Path,
    cwd: &Path,
    args: &[Vec<u8>],
    prep: &dyn Fn(&Path),
    sink: Sink,
    limit: Duration,
) -> Run {
    wipe(work);
    prep(work);
    let mut r = run_mode(bin, cwd, args, sink, limit);
    r.tree = snapshot(work);
    wipe(work);
    r
}

/// `s` as an argument.
pub fn arg(s: impl AsRef<OsStr>) -> Vec<u8> {
    s.as_ref().as_bytes().to_vec()
}

/// Deterministic xorshift64*, so a failing mutant can be reproduced from its index.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// A value for a mutated field of `width` bytes in a file of length `len`.
pub fn interesting(rng: &mut Rng, width: usize, len: u64, old: u64) -> u64 {
    let mask = if width >= 8 {
        u64::MAX
    } else {
        (1u64 << (width * 8)) - 1
    };
    let v = match rng.below(14) {
        0 => 0,
        1 => 1,
        2 => rng.below(64),
        3 => len,
        4 => len.wrapping_sub(rng.below(48)),
        5 => len.wrapping_add(rng.below(48)),
        6 => u64::MAX,
        7 => u64::MAX - rng.below(64),
        8 => old.wrapping_add(rng.below(32)),
        9 => old.wrapping_sub(rng.below(32)),
        10 => old ^ (1 << rng.below(width as u64 * 8)),
        11 => len / 2,
        12 => 1 << rng.below(width as u64 * 8),
        _ => rng.next(),
    };
    v & mask
}

/// Write `v` little-endian (or big-endian) into `buf[at..at+width]`, clipped at the end.
pub fn put(buf: &mut [u8], at: usize, width: usize, v: u64, big: bool) {
    let bytes = if big {
        v.to_be_bytes()[8 - width..].to_vec()
    } else {
        v.to_le_bytes()[..width].to_vec()
    };
    for (i, b) in bytes.into_iter().enumerate() {
        if let Some(slot) = buf.get_mut(at + i) {
            *slot = b;
        }
    }
}

/// Read a field like [`put`] writes it (missing bytes are 0).
pub fn get(buf: &[u8], at: usize, width: usize, big: bool) -> u64 {
    let mut v = 0u64;
    for i in 0..width {
        let b = u64::from(buf.get(at + i).copied().unwrap_or(0));
        if big {
            v = (v << 8) | b;
        } else {
            v |= b << (8 * i);
        }
    }
    v
}

/// gcc (the Nix cc-wrapper, or any gcc with GNU ld) and the glibc it links against.
pub struct Gcc {
    /// `<glibc>/lib`, a store path.
    pub glibc_lib: PathBuf,
    /// The C sources of [`Gcc::build`].
    src: PathBuf,
}

static GCC: std::sync::OnceLock<Option<Gcc>> = std::sync::OnceLock::new();

/// The C compiler, or `None` (with a message) when there is none or its glibc is not in the
/// store.
pub fn gcc() -> Option<&'static Gcc> {
    GCC.get_or_init(|| {
        let out = Command::new("gcc")
            .arg("-print-file-name=libc.so.6")
            .output_busy()
            .ok()
            .filter(|o| o.status.success());
        let Some(out) = out else {
            eprintln!("skipped: no gcc on PATH");
            return None;
        };
        let libc = String::from_utf8(out.stdout).unwrap();
        let libc = std::fs::canonicalize(libc.trim()).ok()?;
        let glibc_lib = libc.parent().unwrap().to_path_buf();
        if !glibc_lib.starts_with("/nix/store/") {
            eprintln!(
                "skipped: gcc's glibc {} is not in /nix/store",
                glibc_lib.display()
            );
            return None;
        }
        let src = scratch("csrc");
        for (name, text) in SOURCES {
            write(&src.join(name), text.as_bytes());
        }
        Some(Gcc { glibc_lib, src })
    })
    .as_ref()
}

const SOURCES: &[(&str, &str)] = &[
    ("bar.c", "int bar(void) { return 42; }\n"),
    (
        "foo.c",
        "int bar(void);\nvoid __reloc_start(void) {}\nint main(void) { return bar() == 42 ? 0 : 1; }\n",
    ),
    (
        "plain.c",
        "int bar(void);\nint main(void) { return bar() == 42 ? 0 : 1; }\n",
    ),
    ("gone.c", "int gone(void) { return 1; }\n"),
    (
        "usegone.c",
        "int gone(void);\nint bar(void);\nint main(void) { return gone() + bar(); }\n",
    ),
    ("vers.map", "VERS_1 { global: bar; local: *; };\n"),
    // a dynsym name GNU ld can suffix-merge into a RUNPATH that ends in `_` padding
    ("merge.c", "int __________(void) { return 1; }\n"),
    ("start.c", "void _start(void) { for (;;) { } }\n"),
];

impl Gcc {
    /// The glibc lib dir as a string.
    pub fn g(&self) -> String {
        self.glibc_lib.to_str().unwrap().to_owned()
    }

    /// `gcc <args>` with the Nix wrapper's automatic rpaths off; panics on failure.
    pub fn cc(&self, args: &[String]) {
        let out = Command::new("gcc")
            .args(args)
            .current_dir(&self.src)
            .env("NIX_DONT_SET_RPATH", "1")
            .env("NIX_LDFLAGS", "")
            .output_busy()
            .unwrap();
        assert!(
            out.status.success(),
            "gcc {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A C source of [`SOURCES`].
    pub fn src(&self, name: &str) -> String {
        self.src.join(name).to_str().unwrap().to_owned()
    }

    /// `-Wl,-rpath,<entries joined with :>` and new dtags.
    pub fn rpath(entries: &[&str]) -> Vec<String> {
        vec![
            "-Wl,--enable-new-dtags".to_owned(),
            format!("-Wl,-rpath,{}", entries.join(":")),
        ]
    }

    /// The driver's interpreter spelling with `./` × 12 of slack.
    pub fn padded_interp(&self) -> String {
        format!("{}/{}ld-linux-x86-64.so.2", self.g(), "./".repeat(12))
    }
}

/// The driver's RUNPATH pad entry: `/` + `_` × `n`.
pub fn pad(n: usize) -> String {
    format!("/{}", "_".repeat(n))
}

/// The ELF64 header and program/section header tables of a seed, for locating fields.
pub struct ElfLayout {
    pub entry: u64,
    pub phoff: usize,
    pub phentsize: usize,
    pub phnum: usize,
    pub shoff: usize,
    pub shentsize: usize,
    pub shnum: usize,
}

impl ElfLayout {
    pub fn parse(b: &[u8]) -> Self {
        let g = |at, w| get(b, at, w, false);
        ElfLayout {
            entry: g(24, 8),
            phoff: g(32, 8) as usize,
            phentsize: g(54, 2) as usize,
            phnum: g(56, 2) as usize,
            shoff: g(40, 8) as usize,
            shentsize: g(58, 2) as usize,
            shnum: g(60, 2) as usize,
        }
    }

    pub fn ph(&self, i: usize) -> usize {
        self.phoff + i * self.phentsize
    }

    pub fn sh(&self, i: usize) -> usize {
        self.shoff + i * self.shentsize
    }

    /// `(sh_type, sh_offset, sh_size, sh_entsize)` of section `i`.
    pub fn section(&self, b: &[u8], i: usize) -> (u32, usize, usize, usize) {
        let at = self.sh(i);
        (
            get(b, at + 4, 4, false) as u32,
            get(b, at + 24, 8, false) as usize,
            get(b, at + 32, 8, false) as usize,
            get(b, at + 56, 8, false) as usize,
        )
    }

    /// The file offset of `vaddr` through the first PT_LOAD mapping it, and that header's index.
    pub fn file_offset(&self, b: &[u8], vaddr: u64) -> Option<(usize, usize)> {
        (0..self.phnum).find_map(|i| {
            let at = self.ph(i);
            let (ty, off, va, filesz) = (
                get(b, at, 4, false),
                get(b, at + 8, 8, false),
                get(b, at + 16, 8, false),
                get(b, at + 32, 8, false),
            );
            (ty == 1 && vaddr >= va && vaddr - va < filesz)
                .then(|| ((off + (vaddr - va)) as usize, i))
        })
    }
}

/// `st_value` of the `.dynsym` symbol `name`.
pub fn dynsym_value(b: &[u8], name: &[u8]) -> Option<u64> {
    let l = ElfLayout::parse(b);
    let i = (0..l.shnum).find(|&i| l.section(b, i).0 == 11)?;
    let (_, off, size, ent) = l.section(b, i);
    let strtab = l.section(b, get(b, l.sh(i) + 40, 4, false) as usize).1;
    (off..off + size).step_by(ent.max(1)).find_map(|at| {
        let s = strtab + get(b, at, 4, false) as usize;
        let end = s + b[s..].iter().position(|&c| c == 0)?;
        (&b[s..end] == name).then(|| get(b, at + 8, 8, false))
    })
}

/// Binaries linked the way the jig link driver links them (`behaviors/jig-reloc-fixup.md`
/// *Interaction with the link driver*), and some that break its conventions. `tree/` is laid
/// out for a prefix at `baked` (its in-prefix RUNPATH entries name `baked/lib/.`); `side/`
/// holds libraries linked against but not shipped.
pub struct ElfSet {
    pub tree: PathBuf,
    pub side: PathBuf,
}

/// Build an [`ElfSet`] in `dir` for a tree that will be at `baked`.
pub fn build_elf_set(g: &Gcc, dir: &Path, baked: &Path) -> ElfSet {
    let tree = dir.join("tree");
    let side = dir.join("side");
    for d in ["lib", "bin"] {
        std::fs::create_dir_all(tree.join(d)).unwrap();
    }
    std::fs::create_dir_all(&side).unwrap();
    let s = |p: &Path| p.to_str().unwrap().to_owned();
    let (lib, bin) = (tree.join("lib"), tree.join("bin"));
    let gl = g.g();
    let g_dot = format!("{gl}/.");
    let in_lib = format!("{}/lib/.", baked.display());
    let p223 = pad(223);
    let shared = |out: &Path, src: &str, extra: &[String], rpath: &[&str]| {
        let mut a = vec![
            "-shared".to_owned(),
            "-fPIC".to_owned(),
            "-o".to_owned(),
            s(out),
            g.src(src),
            format!("-Wl,-soname,{}", out.file_name().unwrap().to_str().unwrap()),
        ];
        a.extend_from_slice(extra);
        if !rpath.is_empty() {
            a.extend(Gcc::rpath(rpath));
        }
        g.cc(&a);
    };
    let exe = |out: &Path, src: &str, extra: &[String]| {
        let mut a = vec![
            "-o".to_owned(),
            s(out),
            g.src(src),
            format!("-L{}", s(&lib)),
        ];
        a.extend_from_slice(extra);
        g.cc(&a);
    };
    let stub = || {
        vec![
            "-Wl,--export-dynamic-symbol=__reloc_start".to_owned(),
            format!("-Wl,--dynamic-linker={}", g.padded_interp()),
        ]
    };
    let with = |mut a: Vec<String>, b: Vec<String>| {
        a.extend(b);
        a
    };
    shared(&lib.join("libbar.so"), "bar.c", &[], &[&g_dot, &p223, &gl]);
    shared(
        &lib.join("libv.so"),
        "bar.c",
        &[format!("-Wl,--version-script={}", g.src("vers.map"))],
        &[&g_dot, &p223, &gl],
    );
    shared(&lib.join("libnopad.so"), "bar.c", &[], &[&gl]);
    shared(&lib.join("libmerge.so"), "merge.c", &[], &[&g_dot, &p223]);
    shared(&side.join("libgone.so"), "gone.c", &[], &[]);
    let full = [in_lib.as_str(), &g_dot, &p223, &gl];
    exe(
        &bin.join("foo"),
        "foo.c",
        &with(with(vec!["-lbar".to_owned()], Gcc::rpath(&full)), stub()),
    );
    exe(
        &bin.join("plain"),
        "plain.c",
        &with(
            vec!["-lbar".to_owned()],
            Gcc::rpath(&[&format!("{}/lib", baked.display()), &gl]),
        ),
    );
    exe(
        &bin.join("vfoo"),
        "foo.c",
        &with(with(vec!["-lv".to_owned()], Gcc::rpath(&full)), stub()),
    );
    exe(
        &bin.join("rpath"),
        "foo.c",
        &with(
            vec![
                "-lbar".to_owned(),
                "-Wl,--disable-new-dtags".to_owned(),
                format!("-Wl,-rpath,{}", full.join(":")),
            ],
            stub(),
        ),
    );
    exe(&bin.join("norunpath"), "plain.c", &["-lbar".to_owned()]);
    g.cc(&[
        "-nostdlib".to_owned(),
        "-static".to_owned(),
        "-o".to_owned(),
        s(&bin.join("static")),
        g.src("start.c"),
    ]);
    exe(
        &bin.join("needgone"),
        "usegone.c",
        &with(
            vec![
                format!("-L{}", s(&side)),
                "-lgone".to_owned(),
                "-lbar".to_owned(),
            ],
            Gcc::rpath(&[&in_lib, &g_dot, &p223]),
        ),
    );
    // RELOCSTB: an external stub installer's header right before e_entry
    exe(
        &bin.join("relocstb"),
        "plain.c",
        &with(
            with(vec!["-lbar".to_owned()], Gcc::rpath(&full)),
            vec![format!("-Wl,--dynamic-linker={}", g.padded_interp())],
        ),
    );
    let mut b = std::fs::read(bin.join("relocstb")).unwrap();
    let l = ElfLayout::parse(&b);
    let (at, _) = l.file_offset(&b, l.entry - 16).unwrap();
    b[at..at + 8].copy_from_slice(b"RELOCSTB");
    std::fs::write(bin.join("relocstb"), &b).unwrap();
    ElfSet { tree, side }
}
