//! reloc-fixup through the `jig` binary against the spec's test vectors
//! (`behaviors/jig-reloc-fixup.md §Test vectors`: CLI, deny check, ELF end to end).
#![allow(clippy::unwrap_used)]

#[path = "fixtures/fixup/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

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

/// §Test vectors, *CLI*.
#[test]
fn cli() {
    if !nix_store() {
        return;
    }
    let dir = scratch("v-cli");
    std::fs::create_dir_all(dir.join("d")).unwrap();
    let d = dir.join("d").to_str().unwrap().to_owned();
    let usage = "usage: reloc-fixup <prefix> [--dest <store path>] [--sdk <dir>] [--deny <store path>]...\n";
    let cases: [(&[&str], &str, i32); 6] = [
        (&[], usage, 2),
        (
            &["/tmp/x"],
            "reloc-fixup: /tmp/x is not under /nix/store\n",
            2,
        ),
        (
            &["/tmp/x", "--bogus", "1"],
            "reloc-fixup: unknown argument --bogus\n",
            2,
        ),
        (
            &["/tmp/x", "--bogus"],
            "reloc-fixup: /tmp/x is not under /nix/store\n",
            2,
        ),
        (&["/tmp/nonexistent", "--dest", "/nix/store/abc"], "", 0),
        (&[&d, "--dest", "/nix/store/abc", "--deny"], "", 0),
    ];
    for (argv, stderr, code) in cases {
        let args: Vec<Vec<u8>> = argv.iter().map(arg).collect();
        let r = fixup(&dir, &args);
        assert_eq!(text(&r.stderr), stderr, "{argv:?}");
        assert_eq!(r.code, Some(code), "{argv:?}");
        assert!(r.stdout.is_empty());
    }
    assert_eq!(usage.as_bytes(), [jig::fixup::USAGE, b"\n"].concat());
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

/// §Test vectors, *deny check*.
#[test]
fn deny() {
    if !nix_store() {
        return;
    }
    let dir = scratch("v-deny");
    let p = dir.join("t");
    write(&p.join("a.txt"), b"abc\ndef");
    write(&p.join("sub/b.bin"), b"\x01xyz");
    write(&p.join("etc/jig.json"), b"abc");
    write(&p.join("y.debug"), b"abc");
    let mut ab = vec![b'b'; 150];
    ab[0] = b'a';
    write(&p.join("long"), &ab);
    let run = |needle: &str| {
        let r = fixup(
            &dir,
            &[
                arg(&p),
                arg("--dest"),
                arg(DEST),
                arg("--deny"),
                arg(needle),
            ],
        );
        assert_eq!(r.code, Some(1));
        let mut lines: Vec<String> = text(&r.stderr).lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    // an empty needle hits offset 0 of every file but `etc/jig.json` and `*.debug` (Q-FIX-3)
    assert_eq!(
        run("/nix/store/foo/"),
        [
            "reloc-fixup: a.txt refers to build-platform abc".to_owned(),
            format!(
                "reloc-fixup: long refers to build-platform a{}",
                "b".repeat(99)
            ),
            "reloc-fixup: sub/b.bin refers to build-platform ".to_owned(),
        ]
    );
    assert_eq!(
        run("x/bc"),
        ["reloc-fixup: a.txt refers to build-platform bc"]
    );
    assert_eq!(
        run("bb"),
        [format!(
            "reloc-fixup: long refers to build-platform {}",
            "b".repeat(100)
        )]
    );
}

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
            "bin/plain  RUNPATH {gr}  NEEDED $ORIGIN/../lib/libbar.so  INTERP {}/ld-linux-x86-64.so.2 kept (no stub)",
            g.g()
        )
    );
    assert!(all.contains(&format!(
        "  WARN absolute store ref in bin/plain @0x318: {}/ld-linux-x86-64.so.2",
        g.g()
    )));
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
