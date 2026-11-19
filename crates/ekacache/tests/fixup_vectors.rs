//! reloc-fixup through the `jig` binary against the spec's test vectors
//! (`behaviors/jig-reloc-fixup.md §Test vectors`: CLI, deny check, ELF end to end). CLI and
//! deny read `tests/vectors/jig-reloc-fixup.toml`; the ELF end-to-end binaries come from this
//! machine's gcc, so those shapes stay here (the file has them as doc-only entries).
#![allow(clippy::unwrap_used)]

#[path = "fixtures/fixup/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use ekacache_testkit::vectors::VectorFile;

use common::{DEST, Sink, arg, build_elf_set, copy_tree, gcc, nix_store, run_mode, scratch, write};

fn fixup(cwd: &Path, args: &[Vec<u8>]) -> common::Run {
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

fn vectors() -> VectorFile {
    VectorFile::load("jig-reloc-fixup")
}

/// §Test vectors, *CLI*: the `cli` rows of `tests/vectors/jig-reloc-fixup.toml`.
#[test]
fn cli() {
    if !nix_store() {
        return;
    }
    let file = vectors();
    let dir = scratch("v-cli");
    std::fs::create_dir_all(dir.join("d")).unwrap();
    let d = dir.join("d").to_str().unwrap().to_owned();
    let usage = format!("{}\n", file.top.str("usage"));
    assert_eq!(usage.as_bytes(), [ekacache::fixup::USAGE, b"\n"].concat());
    for v in file.of_kind("cli", 6) {
        let at = v.label();
        let args: Vec<Vec<u8>> = v
            .strs("args")
            .iter()
            .map(|a| arg(a.replace("<dir>", &d)))
            .collect();
        let want = if v.opt_bool("usage") == Some(true) {
            usage.as_str()
        } else {
            v.str("stderr")
        };
        let r = fixup(&dir, &args);
        assert_eq!(text(&r.stderr), want, "{at}: {:?}", v.strs("args"));
        assert_eq!(
            r.code,
            Some(i32::try_from(v.int("status")).unwrap()),
            "{at}"
        );
        assert!(r.stdout.is_empty(), "{at}");
    }
    // the normalized dest is shown
    let r = fixup(
        &dir,
        &[arg("/tmp/x"), arg("--dest"), arg("/nix/store/../etc/")],
    );
    assert_eq!(
        text(&r.stderr),
        "reloc-fixup: /nix/etc/ is not under /nix/store\n"
    );
}

/// §Test vectors, *deny check*: the `deny` rows over the file's `deny_files` tree.
#[test]
fn deny() {
    if !nix_store() {
        return;
    }
    let file = vectors();
    let dir = scratch("v-deny");
    let p = dir.join("t");
    let names = file.top.strs("deny_files");
    let contents = file.top.byte_strs("deny_contents");
    assert_eq!(names.len(), contents.len());
    for (name, body) in names.iter().zip(&contents) {
        write(&p.join(name), body);
    }
    for v in file.of_kind("deny", 3) {
        let r = fixup(
            &dir,
            &[
                arg(&p),
                arg("--dest"),
                arg(DEST),
                arg("--deny"),
                arg(v.str("needle")),
            ],
        );
        assert_eq!(
            r.code,
            Some(i32::try_from(v.int("status")).unwrap()),
            "{}",
            v.label()
        );
        let mut lines: Vec<String> = text(&r.stderr).lines().map(str::to_owned).collect();
        lines.sort();
        let mut want: Vec<String> = v.strs("stderr").iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        assert_eq!(lines, want, "{}", v.label());
    }
}

/// The loader gcc links `bin/plain` against (it has no `--dynamic-linker`; `bin/foo` takes
/// the driver's, which is x86-64's on every host, as in legacy).
const HOST_LD: &str = if cfg!(target_arch = "aarch64") {
    "ld-linux-aarch64.so.1"
} else {
    "ld-linux-x86-64.so.2"
};

/// §Test vectors, *ELF end to end*, with this machine's glibc for `<G>`.
#[test]
fn elf_end_to_end() {
    if !nix_store() {
        return;
    }
    let Some(g) = gcc() else { return };
    let dir = scratch("v-e2e");
    let out2 = dir.join("out2");
    let set = build_elf_set(g, &dir.join("set"), &out2);
    copy_tree(&set.tree, &out2);
    let gname = g
        .glibc_lib
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let gr = format!("$ORIGIN/../../{gname}/lib");
    let lines =
        |r: &common::Run| -> Vec<String> { text(&r.stdout).lines().map(str::to_owned).collect() };
    let line_of = |all: &[String], rel: &str| -> String {
        all.iter()
            .find(|l| l.starts_with(&format!("{rel}  ")))
            .unwrap_or_else(|| panic!("no line for {rel} in {all:?}"))
            .clone()
    };

    let foo_before = std::fs::read(out2.join("bin/foo")).unwrap();
    let reloc_start = common::dynsym_value(&foo_before, b"__reloc_start").unwrap();
    let r = fixup(&dir, &[arg(&out2), arg("--dest"), arg(DEST)]);
    let all = lines(&r);
    assert_eq!(
        line_of(&all, "lib/libbar.so"),
        format!("lib/libbar.so  RUNPATH {gr}/:{gr}")
    );
    assert_eq!(
        line_of(&all, "bin/foo"),
        format!(
            "bin/foo  RUNPATH {gr}/:{gr}  NEEDED $ORIGIN/../lib//libbar.so  INTERP ../../{gname}/lib/ld-linux-x86-64.so.2 (PT_NULL, entry=__reloc_start)"
        )
    );
    // no stub: PT_INTERP stays (the long scratch path of `out2/lib` leaves room for the
    // direct NEEDED)
    assert_eq!(
        line_of(&all, "bin/plain"),
        format!(
            "bin/plain  RUNPATH {gr}  NEEDED $ORIGIN/../lib/libbar.so  INTERP {}/{HOST_LD} kept (no stub)",
            g.g()
        )
    );
    // `.interp` is at 0x318 on x86-64; the offset follows the target's section layout
    let warn_tail = format!(": {}/{HOST_LD}", g.g());
    assert!(
        all.iter().any(|l| {
            l.strip_prefix("  WARN absolute store ref in bin/plain @0x")
                .and_then(|rest| rest.strip_suffix(warn_tail.as_str()))
                .is_some_and(|off| {
                    !off.is_empty()
                        && off.bytes().all(|b| b.is_ascii_hexdigit())
                        && (cfg!(not(target_arch = "x86_64")) || off == "318")
                })
        }),
        "no WARN line for bin/plain in {all:?}"
    );
    // the RELOCSTB header makes e_entry itself the stub
    assert!(line_of(&all, "bin/relocstb").ends_with(" (PT_NULL, entry=__reloc_start)"));
    let stderr = text(&r.stderr);
    let lib = out2.join("lib").display().to_string();
    assert!(stderr.contains(&format!(
        "{lib}/libnopad.so: RUNPATH does not fit ({} > {}): {gr}\n",
        gr.len(),
        g.g().len()
    )));
    assert!(stderr.contains(&format!(
        "{lib}/libmerge.so: symbol '__________' shares bytes with RUNPATH (suffix-merged .dynstr, not lld?)\n"
    )));
    assert!(stderr.contains(&format!(
        "{}/bin/needgone: NEEDED libgone.so not found in RUNPATH [{}/lib/.:{}/.:{}] nor under {}\n",
        out2.display(),
        out2.display(),
        g.g(),
        common::pad(223),
        out2.display()
    )));
    assert_eq!(r.code, Some(1));

    // PT_INTERP is PT_NULL now and e_entry is __reloc_start's value
    let foo = std::fs::read(out2.join("bin/foo")).unwrap();
    assert_eq!(foo.len(), foo_before.len());
    let l = common::ElfLayout::parse(&foo);
    assert_eq!(l.entry, reloc_start);
    assert!((0..l.phnum).all(|i| common::get(&foo, l.ph(i), 4, false) != 3));
    // the direct NEEDED went into the old RUNPATH slot
    assert!(foo.windows(26).any(|w| w == b"$ORIGIN/../lib//libbar.so\0"));

    // a second run is quiet (the files that failed fail again)
    let r2 = fixup(&dir, &[arg(&out2), arg("--dest"), arg(DEST)]);
    let again = lines(&r2);
    assert!(again.iter().all(|l| l.starts_with("  WARN ")), "{again:?}");

    // relocated: the `out2/lib/.` entry is outside the new prefix, libbar is found through
    // the own lib dir, spelled without the trailing slash
    let run2 = dir.join("run2");
    copy_tree(&set.tree, &run2);
    let r = fixup(&dir, &[arg(&run2), arg("--dest"), arg(DEST)]);
    assert!(line_of(&lines(&r), "bin/foo").contains("  NEEDED $ORIGIN/../lib/libbar.so  "));

    // a read-only bin/: the write-back fails with the clobbered errno (Q-FIX-4)
    let run3 = dir.join("run3");
    copy_tree(&set.tree, &run3);
    std::fs::set_permissions(run3.join("bin"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = fixup(&dir, &[arg(&run3), arg("--dest"), arg(DEST)]);
    std::fs::set_permissions(run3.join("bin"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(text(&r.stderr).contains(&format!(
        "{}/bin/foo: cannot write back: No such file or directory\n",
        run3.display()
    )));
    assert_eq!(r.code, Some(1));
    common::wipe(&dir);
}
