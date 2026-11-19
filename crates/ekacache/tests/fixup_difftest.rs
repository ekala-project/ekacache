//! reloc-fixup against legacy (`implementation/10-jig-reloc-fixup.md §Tests`,
//! `implementation/12-testing.md`): the same trees, at the same path, through legacy's
//! `jig reloc-fixup` and ours; stdout, stderr, exit status and every resulting file must be
//! identical. Gated on `JIG_LEGACY_JIG` (the legacy binary); the ELF cases need gcc.
#![allow(clippy::unwrap_used)]

#[path = "fixtures/fixup/common.rs"]
mod common;

use std::path::Path;

use common::{
    DEST, ElfLayout, Rng, Sink, arg, build_elf_set, chmod, copy_tree, difftest, gcc, get,
    interesting, legacy, past_eof, put, scratch, write,
};

/// §Test vectors, *CLI*, and more argument spellings. No tree needed.
#[test]
fn cli() {
    let Some(old) = legacy() else { return };
    let dir = scratch("cli");
    let work = dir.join("w");
    write(&dir.join("file"), b"x");
    let none = |_: &Path| {};
    let d = dir.to_str().unwrap();
    let cases: Vec<Vec<String>> = vec![
        vec![],
        vec!["/tmp/x".into()],
        vec!["/tmp/x".into(), "--bogus".into(), "1".into()],
        vec!["/tmp/x".into(), "--bogus".into()],
        vec![
            "/tmp/nonexistent".into(),
            "--dest".into(),
            "/nix/store/abc".into(),
        ],
        vec!["/tmp/x".into(), "--dest=/nix/store/abc".into(), "1".into()],
        vec!["/tmp/x".into(), "--dest".into(), "/nix/store".into()],
        vec!["/tmp/x".into(), "--dest".into(), "/nix/store/".into()],
        vec!["/tmp/x".into(), "--dest".into(), "/nix/storex/y".into()],
        vec!["/tmp/x".into(), "--dest".into(), "nix/store/y".into()],
        vec!["/tmp/x".into(), "--dest".into(), "/nix/store/../etc".into()],
        vec![
            "/tmp/x".into(),
            "--dest".into(),
            "/nix/store//a/./b/".into(),
        ],
        vec!["/nix/store/x".into()],
        vec!["/nix/store/x/../../y".into()],
        vec!["--dest".into(), "/nix/store/x".into()],
        vec![
            "/tmp/x".into(),
            "--dest".into(),
            "/nix/store/a".into(),
            "--dest".into(),
            "/tmp/b".into(),
        ],
        vec![
            format!("{d}/file"),
            "--dest".into(),
            "/nix/store/abc".into(),
        ],
        vec![
            format!("{d}/file"),
            "--dest".into(),
            "/nix/store/abc".into(),
            "--deny".into(),
        ],
        vec![
            format!("{d}/file"),
            "--sdk".into(),
            "/x".into(),
            "--dest".into(),
            "/nix/store/abc".into(),
            "--bogus".into(),
        ],
    ];
    for c in &cases {
        let args: Vec<Vec<u8>> = c.iter().map(arg).collect();
        difftest(
            &old,
            &format!("{c:?}"),
            &work,
            &dir,
            &args,
            &none,
            Sink::File,
        );
    }
}

/// §Test vectors, *deny check*, plus the exemptions and `.debug`.
#[test]
fn deny() {
    let Some(old) = legacy() else { return };
    let dir = scratch("deny");
    let work = dir.join("w");
    let w = arg(&work);
    let prep = |w: &Path| {
        write(&w.join("a.txt"), b"abc\ndef");
        write(&w.join("sub/b.bin"), b"\x01xyz");
        write(&w.join("etc/jig.json"), b"bc abc");
        write(&w.join("etc/roots"), b"bc");
        write(&w.join("etc/rootsx"), b"bc");
        write(&w.join("x.debug"), b"bc");
        write(&w.join(".debug"), b"bc");
        write(&w.join("long"), &[b'b'; 300]);
        write(&w.join("high"), b"bc\x80\xff\x7fz");
    };
    for deny in [
        vec!["/nix/store/foo/"],
        vec!["x/bc"],
        vec!["bc", "bc", "zz", "b"],
        vec!["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x"],
    ] {
        let mut args = vec![w.clone(), arg("--dest"), arg(DEST)];
        for d in &deny {
            args.push(arg("--deny"));
            args.push(arg(d));
        }
        difftest(
            &old,
            &format!("{deny:?}"),
            &work,
            &dir,
            &args,
            &prep,
            Sink::File,
        );
    }
}

/// The golden tree: every driver convention and its breakages, plus symlinks, hard links,
/// read-only files and directories, unreadable entries and exempt names, under several
/// invocations.
#[test]
fn elf_tree() {
    let Some(old) = legacy() else { return };
    let Some(g) = gcc() else { return };
    let dir = scratch("elftree");
    let work = dir.join("w");
    let set = build_elf_set(g, &dir.join("set"), &work);
    let t = &set.tree;
    let gl = g.g();
    // the extras of the golden tree
    std::fs::copy(t.join("bin/foo"), t.join("bin/foo.debug")).unwrap();
    std::fs::hard_link(t.join("bin/foo"), t.join("bin/hard")).unwrap();
    std::os::unix::fs::symlink("foo", t.join("bin/link")).unwrap();
    std::os::unix::fs::symlink("lib", t.join("libdir")).unwrap();
    write(&t.join("etc/jig.json"), gl.as_bytes());
    write(&t.join("etc/other.txt"), gl.as_bytes());
    write(&t.join("share/x.so.txt"), b"");
    std::fs::create_dir_all(t.join("lib/debug")).unwrap();
    std::fs::copy(t.join("lib/libbar.so"), t.join("lib/debug/libq.so")).unwrap();
    std::fs::copy(t.join("bin/plain"), t.join("bin/ro.bin")).unwrap();
    chmod(&t.join("bin/ro.bin"), 0o444);
    std::fs::create_dir_all(t.join("ro")).unwrap();
    std::fs::copy(t.join("bin/foo"), t.join("ro/foo")).unwrap();
    std::fs::create_dir_all(t.join("secret")).unwrap();
    std::fs::copy(t.join("bin/foo"), t.join("secret/foo")).unwrap();
    std::fs::copy(t.join("bin/plain"), t.join("bin/unreadable")).unwrap();
    let tree = t.clone();
    let prep = move |w: &Path| {
        copy_tree(&tree, w);
        chmod(&w.join("ro"), 0o555);
        chmod(&w.join("secret"), 0);
        chmod(&w.join("bin/unreadable"), 0);
    };
    let w = arg(&work);
    let glibc_store = arg(g.glibc_lib.parent().unwrap());
    let invocations: Vec<(&str, std::path::PathBuf, Vec<Vec<u8>>)> = vec![
        (
            "dest",
            dir.clone(),
            vec![w.clone(), arg("--dest"), arg(DEST)],
        ),
        (
            "deny",
            dir.clone(),
            vec![
                w.clone(),
                arg("--deny"),
                glibc_store.clone(),
                arg("--dest"),
                arg(DEST),
                arg("--deny"),
                arg("/nix/store/x/"),
            ],
        ),
        (
            "relative prefix",
            dir.clone(),
            vec![arg("w/"), arg("--dest"), arg(format!("{DEST}/"))],
        ),
        (
            "trailing slashes",
            dir.clone(),
            vec![
                arg(format!("{}/", work.display())),
                arg("--dest"),
                arg(format!("{DEST}//")),
            ],
        ),
        (
            "deep dest",
            dir.clone(),
            vec![
                w.clone(),
                arg("--dest"),
                arg("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bar/sub/dir"),
            ],
        ),
        (
            "store root dest",
            dir.clone(),
            vec![w.clone(), arg("--dest"), arg("/nix/store/")],
        ),
        (
            "sdk and unpaired",
            dir.clone(),
            vec![
                arg("w"),
                arg("--sdk"),
                arg("/sdk"),
                arg("--dest"),
                arg(DEST),
                arg("--deny"),
            ],
        ),
    ];
    for (name, cwd, args) in &invocations {
        let l = difftest(&old, name, &work, cwd, args, &prep, Sink::File);
        if *name == "dest" {
            // the tree exercised what it is meant to
            let out = String::from_utf8_lossy(&l.stdout);
            let err = String::from_utf8_lossy(&l.stderr);
            assert!(out.contains("bin/foo  RUNPATH "), "{out}");
            assert!(out.contains("NEEDED $ORIGIN/../lib//libbar.so"), "{out}");
            assert!(out.contains("bin/relocstb  RUNPATH "), "{out}");
            assert!(out.contains("kept (no stub)"), "{out}");
            assert!(out.contains("  WARN absolute store ref in "), "{out}");
            assert!(err.contains("RUNPATH does not fit"), "{err}");
            assert!(err.contains("shares bytes with RUNPATH"), "{err}");
            assert!(err.contains("NEEDED libgone.so not found"), "{err}");
            assert!(
                err.contains("cannot write back: No such file or directory"),
                "{err}"
            );
            assert_eq!(l.code, Some(1));
        }
    }
    // a second run over legacy's result changes nothing
    let legacy_bin = old.clone();
    let prep2 = move |w: &Path| {
        prep(w);
        let r = common::run_mode(
            &legacy_bin,
            w.parent().unwrap(),
            &[arg(w), arg("--dest"), arg(DEST)],
            Sink::File,
            std::time::Duration::from_secs(60),
        );
        assert_eq!(r.code, Some(1));
    };
    let l = difftest(
        &old,
        "second run",
        &work,
        &dir,
        &[w.clone(), arg("--dest"), arg(DEST)],
        &prep2,
        Sink::File,
    );
    assert!(!String::from_utf8_lossy(&l.stdout).contains("RUNPATH"));
    // the same binaries relocated: in-prefix RUNPATH entries now point outside the prefix
    let work2 = dir.join("w2");
    let tree3 = t.clone();
    let prep3 = move |w: &Path| {
        copy_tree(&tree3, w);
        chmod(&w.join("ro"), 0o555);
        chmod(&w.join("secret"), 0);
    };
    let l = difftest(
        &old,
        "relocated",
        &work2,
        &dir,
        &[arg(&work2), arg("--dest"), arg(DEST)],
        &prep3,
        Sink::File,
    );
    assert!(String::from_utf8_lossy(&l.stdout).contains("NEEDED $ORIGIN/../lib/libbar.so"));
    let _ = &set.side;
}

/// Q-FIX-10 aborts on ELF input, and what reaches stdout and stderr before them.
#[test]
fn elf_aborts_and_streams() {
    let Some(old) = legacy() else { return };
    let Some(g) = gcc() else { return };
    let dir = scratch("elfabort");
    let work = dir.join("w");
    let set = build_elf_set(g, &dir.join("set"), &work);
    let w = arg(&work);
    let args = vec![w.clone(), arg("--dest"), arg(DEST)];
    let libbar = std::fs::read(set.tree.join("lib/libbar.so")).unwrap();
    let foo = std::fs::read(set.tree.join("bin/foo")).unwrap();
    let eof = past_eof(&set);

    // the stub check past EOF
    let prep = |w: &Path| {
        write(&w.join("lib/libbar.so"), &libbar);
        write(&w.join("bin/eof"), &eof);
    };
    let l = difftest(&old, "past eof", &work, &dir, &args, &prep, Sink::File);
    assert_eq!(l.signal, Some(libc::SIGABRT));

    // `fs::exists` on a NEEDED in a directory that cannot be searched
    let locked = {
        let side = dir.join("locked");
        std::fs::create_dir_all(&side).unwrap();
        let out = side.join("locked");
        g.cc(&[
            "-o".into(),
            out.to_str().unwrap().into(),
            g.src("plain.c"),
            format!("-L{}", set.tree.join("lib").display()),
            "-lbar".into(),
            "-Wl,--enable-new-dtags".into(),
            format!(
                "-Wl,-rpath,{}/locked:{}/lib/.:{}/.:{}",
                work.display(),
                work.display(),
                g.g(),
                common::pad(100)
            ),
        ]);
        std::fs::read(out).unwrap()
    };
    let prep = |w: &Path| {
        write(&w.join("lib/libbar.so"), &libbar);
        write(&w.join("bin/locked"), &locked);
        std::fs::create_dir_all(w.join("locked")).unwrap();
        chmod(&w.join("locked"), 0);
    };
    let l = difftest(&old, "EACCES", &work, &dir, &args, &prep, Sink::File);
    assert_eq!(l.signal, Some(libc::SIGABRT));

    // ENAMETOOLONG: a NEEDED longer than NAME_MAX
    let long = {
        let side = dir.join("long");
        std::fs::create_dir_all(&side).unwrap();
        let soname = format!("lib{}.so", "l".repeat(300));
        let lib = side.join("liblong.so");
        g.cc(&[
            "-shared".into(),
            "-fPIC".into(),
            "-o".into(),
            lib.to_str().unwrap().into(),
            g.src("gone.c"),
            format!("-Wl,-soname,{soname}"),
        ]);
        let out = side.join("long");
        g.cc(&[
            "-o".into(),
            out.to_str().unwrap().into(),
            g.src("usegone.c"),
            lib.to_str().unwrap().into(),
            format!("-L{}", set.tree.join("lib").display()),
            "-lbar".into(),
            "-Wl,--enable-new-dtags".into(),
            format!("-Wl,-rpath,{}/lib/.:{}", work.display(), common::pad(400)),
        ]);
        std::fs::read(out).unwrap()
    };
    let prep = |w: &Path| {
        write(&w.join("lib/libbar.so"), &libbar);
        write(&w.join("bin/long"), &long);
    };
    let l = difftest(&old, "ENAMETOOLONG", &work, &dir, &args, &prep, Sink::File);
    assert_eq!(l.signal, Some(libc::SIGABRT));

    // stdout beyond one buffer before an abort: what was flushed must agree. The aborting
    // file is put last in readdir order, which is the walk order.
    let many = |w: &Path, abort: bool| {
        for i in 0..60 {
            write(&w.join(format!("many/f{i:02}")), &libbar);
        }
        if abort {
            let mut names: Vec<_> = std::fs::read_dir(w.join("many"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            let last = names.pop().unwrap();
            std::fs::write(last, &eof).unwrap();
        }
    };
    for sink in [Sink::File, Sink::Pipe] {
        let l = difftest(
            &old,
            "flushed before abort",
            &work,
            &dir,
            &args,
            &|w| many(w, true),
            sink,
        );
        assert_eq!(l.signal, Some(libc::SIGABRT));
        assert!(l.stdout.len() >= 4096, "{}", l.stdout.len());
    }
    // a full stdout: ignored at exit, fatal when the buffer is emptied mid-run
    let one = |w: &Path| write(&w.join("lib/libbar.so"), &libbar);
    let l = difftest(
        &old,
        "/dev/full small",
        &work,
        &dir,
        &args,
        &one,
        Sink::Full,
    );
    assert_eq!(l.code, Some(0));
    let l = difftest(
        &old,
        "/dev/full large",
        &work,
        &dir,
        &args,
        &|w| many(w, false),
        Sink::Full,
    );
    assert_eq!(l.signal, Some(libc::SIGABRT));
    // a closed stderr: fatal at the first message only
    let l = difftest(
        &old,
        "stderr closed, quiet",
        &work,
        &dir,
        &args,
        &one,
        Sink::StderrClosed,
    );
    assert_eq!(l.code, Some(0));
    let noisy = |w: &Path| {
        write(&w.join("lib/libbar.so"), &libbar);
        write(&w.join("bin/foo"), &foo);
    };
    let mut deny = args.clone();
    deny.extend([arg("--deny"), arg("/nix/store/x/")]);
    let l = difftest(
        &old,
        "stderr closed, deny",
        &work,
        &dir,
        &deny,
        &noisy,
        Sink::StderrClosed,
    );
    assert_eq!(l.signal, Some(libc::SIGABRT));
}

/// Fields an ELF mutation targets: `(offset, width)`.
fn elf_fields(b: &[u8]) -> Vec<(usize, usize)> {
    let l = ElfLayout::parse(b);
    let mut f = vec![
        (4, 1),
        (5, 1),
        (16, 2),
        (24, 8),
        (32, 8),
        (40, 8),
        (54, 2),
        (56, 2),
        (58, 2),
        (60, 2),
    ];
    for i in 0..l.phnum {
        let at = l.ph(i);
        f.extend([(at, 4), (at + 8, 8), (at + 16, 8), (at + 32, 8)]);
    }
    for i in 0..l.shnum {
        let at = l.sh(i);
        f.extend([
            (at + 4, 4),
            (at + 24, 8),
            (at + 32, 8),
            (at + 40, 4),
            (at + 56, 8),
        ]);
        let (ty, off, size, ent) = l.section(b, i);
        let step = match ty {
            // dynamic (d_tag, d_val) and verneed records
            6 | 0x6fff_fffe => 16,
            11 => ent.max(1), // dynsym: st_name, st_value
            _ => 0,
        };
        if step == 0 {
            continue;
        }
        let mut o = off;
        while o + step <= off + size && o + step <= b.len() {
            match ty {
                6 => f.extend([(o, 8), (o + 8, 8)]),
                11 => f.extend([(o, 4), (o + 8, 8)]),
                _ => f.extend([(o + 4, 4), (o + 12, 4)]),
            }
            o += step;
        }
    }
    f
}

/// String bytes a mutation may overwrite: .dynstr and .interp.
fn elf_strings(b: &[u8]) -> Vec<(usize, usize)> {
    let l = ElfLayout::parse(b);
    let mut out = Vec::new();
    for i in 0..l.shnum {
        let (ty, off, size, _) = l.section(b, i);
        if ty == 3 && off + size <= b.len() {
            out.push((off, size));
        }
    }
    for i in 0..l.phnum {
        let at = l.ph(i);
        if get(b, at, 4, false) == 3 {
            out.push((
                get(b, at + 8, 8, false) as usize,
                get(b, at + 32, 8, false) as usize,
            ));
        }
    }
    out
}

/// One mutant of `seed`: one to three field, string or length mutations.
fn mutate_elf(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut b = seed.to_vec();
    let fields = elf_fields(seed);
    let strings = elf_strings(seed);
    for _ in 0..=rng.below(3) {
        match rng.below(10) {
            0 => {
                let n = rng.below(b.len() as u64 + 1) as usize;
                b.truncate(n);
            }
            1 | 2 if !strings.is_empty() => {
                let (off, size) = *rng.pick(&strings);
                let at = off + rng.below(size.max(1) as u64) as usize;
                let v = *rng.pick(&[0u8, b'/', b':', b'.', b'_', b'x', 0xff]);
                if let Some(c) = b.get_mut(at) {
                    *c = v;
                }
            }
            _ => {
                let (at, width) = *rng.pick(&fields);
                let old = get(&b, at, width, false);
                let v = interesting(rng, width, b.len() as u64, old);
                put(&mut b, at, width, v, false);
            }
        }
    }
    b
}

/// Mutated ELF files from every seed of the set, each next to an intact `lib/libbar.so`.
#[test]
fn elf_mutants() {
    let Some(old) = legacy() else { return };
    let Some(g) = gcc() else { return };
    let dir = scratch("elfmut");
    let work = dir.join("w");
    let set = build_elf_set(g, &dir.join("set"), &work);
    let libbar = std::fs::read(set.tree.join("lib/libbar.so")).unwrap();
    let seeds: Vec<Vec<u8>> = [
        "bin/foo",
        "bin/vfoo",
        "bin/plain",
        "bin/relocstb",
        "lib/libbar.so",
        "lib/libmerge.so",
    ]
    .iter()
    .map(|p| std::fs::read(set.tree.join(p)).unwrap())
    .collect();
    let args = vec![arg(&work), arg("--dest"), arg(DEST)];
    let mut rng = Rng(0x0005_eed0_f1c5_u64);
    let (mut aborts, mut changed) = (0, 0);
    let count = mutant_count(320);
    for i in 0..count {
        let seed = &seeds[i % seeds.len()];
        let m = mutate_elf(seed, &mut rng);
        let prep = |w: &Path| {
            write(&w.join("lib/libbar.so"), &libbar);
            write(&w.join("bin/x"), &m);
        };
        let l = difftest(
            &old,
            &format!("ELF mutant {i}"),
            &work,
            &dir,
            &args,
            &prep,
            Sink::File,
        );
        aborts += usize::from(l.signal.is_some());
        changed += usize::from(l.stdout.windows(7).any(|w| w == b"bin/x  "));
    }
    eprintln!("ELF mutants: {count} compared, {aborts} aborted, {changed} rewritten");
}

/// `JIG_FIXUP_MUTANTS` overrides the number of mutants (for a longer run by hand).
fn mutant_count(default: usize) -> usize {
    std::env::var("JIG_FIXUP_MUTANTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
