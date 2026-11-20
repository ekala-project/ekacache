//! End to end: a real `go build` twice in launcher mode (`ec <go> build`, which sets
//! `GOCACHEPROG="<ec> gocacheprog"`), on a real ecd (`implementation/08-ec-gocache-and-
//! cache-cmds.md §Part 2 §Tests`).
//!
//! Needs `go` on `PATH` and an ecd (`$EC_RUST_ECD`); skipped without. The second build, with
//! an empty `GOCACHE` and DiskPath directory, must get every package from ecd (`go\tcached`
//! rows, no compiler run under `-x`) and link an identical binary.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]

mod common;
mod gocache_support;

use std::path::PathBuf;
use std::process::Command;

use gocache_support::{EC, log_rows};

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

const MAIN: &str = r#"package main

import (
	"fmt"
	"os"

	"example.com/m/greet"
)

func main() {
	fmt.Fprintln(os.Stdout, greet.Hello(len(os.Args)))
}
"#;

const GREET: &str = r#"package greet

import "strings"

// Hello greets n times.
func Hello(n int) string { return strings.Repeat("hello ", n) }
"#;

#[test]
fn go_build_twice_hits() {
    let Some(go) = on_path("go") else {
        eprintln!("go is not on PATH: skipped");
        return;
    };
    let Some(daemon) = common::daemon("EC_RUST_ECD") else {
        eprintln!("EC_RUST_ECD is unset: skipped");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path();
    let module = t.join("mod");
    std::fs::create_dir_all(module.join("greet")).unwrap();
    std::fs::write(module.join("go.mod"), "module example.com/m\n\ngo 1.21\n").unwrap();
    std::fs::write(module.join("main.go"), MAIN).unwrap();
    std::fs::write(module.join("greet/greet.go"), GREET).unwrap();

    // one build (`-x`) with a fresh GOCACHE, DiskPath directory and EKACACHE_LOG: the log rows, and
    // the tools go ran
    let build = |n: u32| -> (Vec<String>, Vec<String>) {
        let dir = |name: &str| t.join(format!("{name}{n}"));
        let log = dir("log");
        let mut cmd = Command::new(EC);
        cmd.arg(&go)
            .args(["build", "-x", "-o"])
            .arg(dir("out"))
            .arg(".")
            .current_dir(&module)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", t)
            .env("GOCACHE", dir("gocache"))
            .env("GOPATH", t.join("gopath"))
            .env("GOTOOLCHAIN", "local")
            .env("GOENV", "off")
            .env("GOPROXY", "off")
            .env("GOFLAGS", "")
            .env("CGO_ENABLED", "0")
            .env("EKACACHE_SOCK", &daemon.sock)
            .env("GOCACHEPROG_DIR", dir("dp"))
            .env("EKACACHE_LOG", &log);
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "build {n}: {stderr}");
        let tools = stderr
            .lines()
            // `[ENV=… ]<goroot>/pkg/tool/<os_arch>/<tool> args…`
            .filter_map(|l| {
                let word = l.split(' ').find(|w| w.contains("/pkg/tool/"))?;
                Some(word.rsplit('/').next()?.to_owned())
            })
            .collect();
        (log_rows(&log), tools)
    };
    let compiled = |rows: &[String]| -> Vec<String> {
        let mut v: Vec<String> = rows
            .iter()
            .filter(|r| r.starts_with("go\tcompiled\t"))
            .cloned()
            .collect();
        v.sort();
        v
    };
    let cached = |rows: &[String]| {
        rows.iter()
            .filter(|r| r.starts_with("go\tcached\t"))
            .count()
    };
    let bin = |n: u32| std::fs::read(t.join(format!("out{n}"))).unwrap();

    let (first, tools) = build(1);
    assert!(first.iter().all(|r| r.starts_with("go\t")), "{first:?}");
    assert!(tools.iter().any(|t| t == "compile"), "{tools:?}");
    assert!(compiled(&first).len() > 10, "{first:?}");

    // warm: no compiler runs and every package comes from ecd. cmd/go itself still puts a
    // few entries on every build: the link step always runs (go build does not cache
    // executables) and `updateBuildID` re-puts the linker's `link-stdout`, and two small
    // index entries are keyed differently from build to build (go 1.25). Those are the only
    // compiled rows.
    let (second, tools) = build(2);
    assert!(
        !tools.iter().any(|t| t == "compile" || t == "asm"),
        "{tools:?}"
    );
    assert!(tools.iter().any(|t| t == "link"), "{tools:?}");
    assert!(cached(&second) > 10, "{second:?}");
    assert!(compiled(&second).len() <= 3, "{second:?}");
    assert_eq!(bin(1), bin(2));
    // the hits wrote DiskPath files for go to read
    assert!(std::fs::read_dir(t.join("dp2")).unwrap().count() > 10);
}
