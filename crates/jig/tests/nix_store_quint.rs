//! Model-based tests for `jig nix-store` (`implementation/09-jig-nix-store.md §Tests` 4,
//! `implementation/12-testing.md §L4`): `spec/builder_rpc.qnt` traces drive the fake daemon, and
//! the client's observable sequence must match each trace: the bytes it wrote (the model's
//! `written`), its exit code, whether it printed a result, whether it printed a daemon error.
//! With `JIG_LEGACY_JIG` set, each replay is also compared with the legacy jig.
//!
//! The traces in `fixtures/nix_store/itf/` come from `fixtures/nix_store/builder_rpc_mbt.qnt`
//! (how they were made is described there). The concrete daemon behaviour for each abstract
//! step is chosen below; every replay runs with SIGPIPE ignored, so that a failed write is the
//! model's exit 1 (Q-NIX-7 is tested in `nix_store_rpc.rs`).
#![allow(clippy::unwrap_used)]

mod nix_store_support;

use serde_json::Value;

use nix_store_support::{
    Inv, LAST, MAGIC_1, MAGIC_2, RESULT, START_ACTIVITY, STOP_ACTIVITY, Script, Step, V1_38, aterm,
    aterm_json, cat, check, client_add, client_features, client_submit, daemon_info, error,
    fetch_one, fetch_one_aterm, next, path_info, strace, string, strings, word,
};

const DAEMON_PATH: &[u8] = b"/nix/store/00000000000000000000000000000000-from-daemon";
const SUBMIT_PATH: &[u8] = b"/nix/store/abc-x.drv";

fn tag(v: &Value) -> &str {
    v["tag"].as_str().unwrap()
}

fn int(v: &Value) -> i64 {
    v["#bigint"].as_str().unwrap().parse().unwrap()
}

/// A pick: `None`, a variant name, a boolean or an integer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pick<'a> {
    None,
    Tag(&'a str),
    Bool(bool),
    Int(i64),
}

fn raw<'a>(picks: &'a Value, k: &str) -> Pick<'a> {
    let Some(p) = picks.get(k) else {
        return Pick::None;
    };
    if tag(p) == "None" {
        return Pick::None;
    }
    let v = &p["value"];
    if let Some(b) = v.as_bool() {
        Pick::Bool(b)
    } else if v.get("#bigint").is_some() {
        Pick::Int(int(v))
    } else {
        Pick::Tag(tag(v))
    }
}

/// The daemon behaviour a step chose: `stepClient(i, m, w)`. In `builder_rpc_mbt.qnt`'s
/// `mostlyGood`, `g != 0` is a well-behaved step regardless of the other picks.
struct Picks<'a> {
    i: &'a str,
    m: &'a str,
    w: bool,
}

fn effective(picks: &Value) -> Picks<'_> {
    match raw(picks, "g") {
        Pick::Int(g) if g != 0 => Picks {
            i: "Good",
            m: match g {
                1 => "Next",
                2 => "Activity",
                _ => "Last",
            },
            w: true,
        },
        _ => Picks {
            i: match raw(picks, "i") {
                Pick::Tag(t) => t,
                p => panic!("pick i: {p:?}"),
            },
            m: match raw(picks, "m") {
                Pick::Tag(t) => t,
                p => panic!("pick m: {p:?}"),
            },
            w: match raw(picks, "w") {
                Pick::Bool(b) => b,
                p => panic!("pick w: {p:?}"),
            },
        },
    }
}

/// The concrete run for a trace.
struct Replay {
    inv: Inv,
    script: Script,
    /// The bytes of the model's `WOp7` / `WOp1000`.
    op: Vec<u8>,
    /// Whether the result printed in `Done` is non-empty (`submit` prints nothing).
    prints: bool,
}

/// `ShutRd` just before the daemon's last send: the client's next write fails.
fn fail_next_write(script: &mut Script) {
    let at = script
        .iter()
        .rposition(|s| matches!(s, Step::Send(_)))
        .unwrap_or(0);
    script.insert(at, Step::ShutRd);
}

fn replay(trace: &Value) -> Replay {
    let states = trace["states"].as_array().unwrap();
    let first = &states[0];
    let verb = tag(&first["verb"]);
    let (args, stdin, op): (&[&[u8]], Vec<u8>, Vec<u8>) = match verb {
        "AddText" => (
            &[b"add-text", b"hello.txt"],
            b"hello".to_vec(),
            client_add(b"hello.txt", &[], b"hello"),
        ),
        "AddDrv" => (
            &[b"add-drv", b"x.drv"],
            aterm_json().to_vec(),
            client_add(b"x.drv", &[], aterm()),
        ),
        "Fetchurls" => {
            let n = usize::try_from(int(&first["fetchLeft"])).unwrap();
            let element = &fetch_one()[1..fetch_one().len() - 1];
            let elements = vec![element; n].join(&b","[..]);
            (
                &[b"fetchurls"],
                cat(&[b"[", &elements, b"]"]),
                client_add(b"foo-1.0.tar.gz.drv", &[], fetch_one_aterm()),
            )
        }
        "Submit" => (
            &[b"submit", SUBMIT_PATH, b"out"],
            Vec::new(),
            client_submit(SUBMIT_PATH, b"out"),
        ),
        "BadVerb" => (&[b"bogus"], Vec::new(), Vec::new()),
        v => panic!("verb {v}"),
    };
    let mut inv = Inv::new(args).stdin(&stdin).sigpipe_ignored();
    let mut s: Script = Vec::new();
    let mut activity = 0;
    for (k, pair) in states.windows(2).enumerate() {
        let pre = tag(&pair[0]["st"]);
        let post = tag(&pair[1]["st"]);
        // the picks that led to a state are recorded in that state
        let p = effective(&pair[1]["mbt::nondetPicks"]);
        // whether this step ended the client
        let ends = match pre {
            "Start" | "Ready" | "Done" => false,
            "SendHello" if p.w => {
                s.push(Step::Recv(16));
                false
            }
            "SendHello" => {
                // the daemon hangs up before the client's first write
                inv = inv.delay_connect();
                s.push(Step::ShutRd);
                true
            }
            "SendFeatures" | "SendPost" | "SendAdd" | "SendSubmit" if p.w => {
                s.push(Step::Recv(match pre {
                    "SendFeatures" => 136,
                    "SendPost" => 16,
                    _ => op.len(),
                }));
                false
            }
            "SendFeatures" | "SendPost" | "SendAdd" | "SendSubmit" => {
                fail_next_write(&mut s);
                true
            }
            "AwaitMagic" | "AwaitVersion" | "AwaitFeatures" | "AwaitInfo" | "AwaitPathInfo"
            | "AwaitSubmitResult"
                if p.i == "Eof" =>
            {
                // part of the expected reply, then EOF
                if pre == "AwaitPathInfo" {
                    s.push(Step::Send(path_info(DAEMON_PATH)[..20].to_vec()));
                }
                true
            }
            "AwaitMagic" | "AwaitVersion" => {
                let good = p.i == "Good";
                s.push(Step::Send(word(match (pre, good) {
                    ("AwaitMagic", true) => MAGIC_2,
                    ("AwaitMagic", false) => MAGIC_1,
                    (_, true) => V1_38,
                    (_, false) => 0x125,
                })));
                !good
            }
            // the client checks none of these; "Wrong" is something unusual but well-formed
            "AwaitFeatures" => {
                s.push(Step::Send(if p.i == "Good" {
                    strings(&[b"submit-output"])
                } else {
                    strings(&[b"", b"unknown-feature"])
                }));
                false
            }
            "AwaitInfo" => {
                s.push(Step::Send(if p.i == "Good" {
                    daemon_info()
                } else {
                    cat(&[&string(b""), &word(7)])
                }));
                false
            }
            "AwaitPathInfo" => {
                s.push(Step::Send(path_info(if p.i == "Good" {
                    DAEMON_PATH
                } else {
                    b"not a store path"
                })));
                false
            }
            "AwaitSubmitResult" => {
                s.push(Step::Send(word(if p.i == "Good" { 1 } else { u64::MAX })));
                false
            }
            "DrainStartup" | "DrainAdd" | "DrainSubmit" => match p.m {
                "Last" => {
                    s.push(Step::Send(word(LAST)));
                    false
                }
                "Next" => {
                    s.push(Step::Send(next(b"note")));
                    false
                }
                "Activity" => {
                    activity += 1;
                    s.push(Step::Send(match activity % 3 {
                        0 => cat(&[&word(STOP_ACTIVITY), &word(1)]),
                        1 => cat(&[
                            &word(START_ACTIVITY),
                            &word(1),
                            &word(0),
                            &word(105),
                            &string(b"building"),
                            &word(1),
                            &word(1),
                            &string(b"field"),
                            &word(0),
                        ]),
                        _ => cat(&[&word(RESULT), &word(1), &word(101), &word(0)]),
                    }));
                    false
                }
                "Error" => {
                    s.push(Step::Send(error(b"boom")));
                    true
                }
                "Unknown" => {
                    s.push(Step::Send(word(0x6461_7461)));
                    true
                }
                "MsgEof" => true,
                m => panic!("pick m {m}"),
            },
            st => panic!("state {st}"),
        };
        assert_eq!(
            ends,
            post == "Exited" && !matches!(pre, "Ready" | "Done"),
            "step {}: {pre} → {post}",
            k + 1
        );
        if post == "Exited" {
            break;
        }
    }
    Replay {
        inv,
        script: s,
        op,
        prints: verb != "Submit",
    }
}

/// The model's `written` as bytes.
fn written(state: &Value, op: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for w in state["written"].as_array().unwrap() {
        out.extend(match tag(w) {
            "WMagic1" => word(MAGIC_1),
            "WVersion" => word(V1_38),
            "WFeatures" => client_features(),
            "WZero" => word(0),
            "WOp7" | "WOp1000" => op.to_vec(),
            t => panic!("written {t}"),
        });
    }
    out
}

#[test]
fn builder_rpc_traces() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/nix_store/itf");
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".itf.json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    let have_strace = strace().is_some();
    for path in paths {
        let trace: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let r = replay(&trace);
        if r.inv.delay_connect && !have_strace {
            eprintln!("{}: needs strace, not replayed", path.display());
            continue;
        }
        let last = trace["states"].as_array().unwrap().last().unwrap();
        assert_eq!(tag(&last["st"]), "Exited", "{}", path.display());
        let o = check(&r.inv, &r.script);
        let name = path.file_name().unwrap().to_string_lossy();
        let code = i32::try_from(int(&last["exitCode"])).unwrap();
        assert_eq!(o.code, Some(code), "{name}: {o:?}");
        assert_eq!(o.sent, written(last, &r.op), "{name}: {o:?}");
        assert_eq!(
            !o.stdout.is_empty(),
            last["stdoutWritten"].as_bool().unwrap() && r.prints,
            "{name}: {o:?}"
        );
        let printed = o.stderr.windows(12).any(|w| w == b"nix-daemon: ");
        assert_eq!(
            printed,
            last["printedDaemonError"].as_bool().unwrap(),
            "{name}: {o:?}"
        );
    }
}
