//! Shared by the fuzz targets (included with `#[path]`): the Go oracle tables and running the
//! ec binary (implementation/12-testing.md §L5).
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Hex as the oracle tables write their inputs.
pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("oracle hex"))
        .collect()
}

/// `tests/vectors/<name>`: a JSON array of objects with a hex `in`, keyed by the decoded input.
pub fn oracle(text: &str) -> HashMap<Vec<u8>, serde_json::Value> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(text).expect("oracle json");
    rows.into_iter()
        .map(|r| (unhex(r["in"].as_str().expect("oracle in")), r))
        .collect()
}

/// `EC_RUST_EC`, which the process targets need.
pub fn binary() -> PathBuf {
    std::env::var_os("EC_RUST_EC")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .expect("set EC_RUST_EC to a release ec binary")
}

/// What a run of ec shows: exit status or signal, stdout, stderr.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `bin` as `argv0` (ec dispatches on it) in an empty environment plus `env`.
pub fn run(bin: &Path, argv0: &str, args: &[&[u8]], env: &[(&str, &str)], stdin: &[u8]) -> Outcome {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(bin);
    cmd.arg0(argv0)
        .args(args.iter().map(|a| std::ffi::OsStr::from_bytes(a)))
        .env_clear()
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn ec");
    let mut input = child.stdin.take().expect("stdin");
    let data = stdin.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().expect("wait for ec");
    let _ = feeder.join();
    Outcome {
        code: out.status.code(),
        signal: out.status.signal(),
        stdout: out.stdout,
        stderr: out.stderr,
    }
}

/// A Rust panic, as opposed to a deliberate abort (Q-FIX-10 and friends): with
/// `panic = "abort"` both die by SIGABRT, and only the panic prints this.
pub fn assert_no_panic(o: &Outcome) {
    assert!(
        !o.stderr.windows(11).any(|w| w == b"panicked at"),
        "ec panicked: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}
