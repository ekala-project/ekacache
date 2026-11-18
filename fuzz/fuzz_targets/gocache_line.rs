//! A `jig gocacheprog` session (no jigd: every get misses, every put is refused) fed arbitrary
//! request lines: no panic; every reply line is a JSON object; the exit status is 0 (the
//! input ended) or 1 (a fatal line, Q-GO-1).
#![no_main]

use jig::gocache::{LineReader, Session};
use jig::process::ProcEnv;
use jig_core::env::Env;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let dir = tempfile::tempdir().expect("temp dir");
    let env = ProcEnv::new(Env::from_pairs::<&str, &str>(&[]));
    let mut session = Session {
        client: jig::client::Client::new(),
        dir: dir.path().as_os_str().as_encoded_bytes().to_vec(),
        env: &env,
    };
    let mut out = Vec::new();
    let code = session.serve(&mut LineReader::new(input), &mut out);
    assert!(code == 0 || code == 1, "exit {code}");
    for line in out.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        let v: serde_json::Value = serde_json::from_slice(line).expect("a reply is JSON");
        assert!(v.is_object(), "a reply is an object");
    }
});
