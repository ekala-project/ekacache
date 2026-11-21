//! rustc mode: `ec rustcwrap <rustc> <args…>` (cargo's `RUSTC_WRAPPER`) and the launcher for a
//! program named `rustc` (`behaviors/ec-rustc-mode.md`, `rustc_mode.cc`,
//! `implementation/07-ec-rustc.md`).
//!
//! Library crates are cached with the two-key scheme of `behaviors/ec-keys.md §Manifest`: k1 is
//! `rs/<hex>` over the fields of `key` (§Request key (k1)), the crate root is the primary
//! source, the dep-info's first rule gives the manifest inputs (`depinfo`, §Dep-info parsing),
//! and every output it names is stored as one blob (`pack`, §Output blob). `run` follows
//! §Invocation, §Uncached runs, §Lookup and §Compile and store.
//!
//! Adopted changes: Q-RS-3 (`# env-dep:` variables are `$env:` manifest lines, `rs-schema=8`),
//! Q-RS-5 (`--crate-type` values split on `,`, `parse`), Q-CC-2 / Q-CLIENT-2 (the slot is
//! released after the PUTs, and every PUT reply is checked), Q-KEY-3 (an input without an
//! identity makes the result unstorable), Q-RS-11 (restore is confined to the out-dir, `pack`).
//! Kept quirks: Q-RS-1 (failures are never cached), Q-RS-2 (no `MaskOut`), Q-RS-4 (an empty
//! blob is stored and replayed), Q-RS-6 (a glued `-L` is in the key), Q-RS-7 (out-dir and
//! metadata are not), Q-RS-8 (`@` rewriting on restore).

pub mod depinfo;
pub mod key;
pub mod pack;
pub mod parse;

use std::io::Write;

use ekacache_core::env::Env;
use ekacache_core::fspath;
use ekacache_core::keys::{RequestKey, slot};
use ekacache_core::manifest::{Unstorable, env_line};

use crate::Ctx;
use crate::client::Client;
use crate::log::{self, Outcome, Stopwatch};
use crate::process::{self, Capture};
use crate::slot as slot_guard;
use crate::twokey;
use crate::util::{on_path, read_file};

use self::key::KeyInputs;
use self::parse::{RustInvocation, parse_rust_invocation};

/// The `$EKACACHE_LOG` tool name.
const TOOL: &str = "rustc";

/// `RunRustcMode(args, socket)` (`rustc_mode.cc:277-358`); `args` = `[<rustc>, <rustc args…>]`.
pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let clock = Stopwatch::start();
    let Some((rustc, rest)) = args.split_first() else {
        write_stderr(b"rustcwrap: usage: rustcwrap <rustc> args...\n");
        return 1;
    };
    let inv = parse_rust_invocation(rest);
    let label = if inv.crate_name.is_empty() {
        inv.source.clone()
    } else {
        concat(&inv.crate_name, &inv.extra_filename)
    };

    let mut client = Client::new();
    let source = if inv.cacheable {
        read_file(&inv.source)
    } else {
        None
    };
    let have_source = source.is_some();
    let source = match source {
        Some(s) if client.connect(&ctx.socket) => s,
        _ => return run_uncached(&mut client, ctx, rustc, &inv, have_source, &label, &clock),
    };

    let tool_path = on_path(rustc, &ctx.env);
    let stdlib = key::stdlib_ids(&mut client, &ctx.store, &tool_path, &inv.sysroot);
    let Ok(cwd) = std::env::current_dir() else {
        // `fs::current_path()` throws and nothing catches it: legacy terminates
        std::process::abort()
    };
    let cwd = cwd.into_os_string().into_encoded_bytes();
    let fields = key::key_fields(
        &ctx.store,
        &inv,
        &KeyInputs {
            tool_path: &tool_path,
            stdlib: &stdlib,
            cwd: &cwd,
            env: ctx.env.env(),
            source: &source,
        },
    );
    let k1 = key::request_key(&fields);

    // the log's subject says how far a miss got
    let env = ctx.env.env();
    let result = twokey::find_result_with_env(&mut client, &ctx.store, &k1, &|name: &[u8]| {
        getenv(env, name)
    });
    let mut missed = label.clone();
    missed.push(b' ');
    match &result {
        Ok(_) => missed.extend_from_slice(b"object-gone"),
        Err(why) => missed.extend_from_slice(why),
    }
    if let Ok(k2) = &result
        && let Some(blob) = client.get(&slot::object(k2))
        && pack::unpack_files(&blob, &inv.out_dir, &inv.extra_filename)
    {
        let stderr = client.get(&slot::stderr(k2)).unwrap_or_default();
        write_stderr(&stderr);
        log::outcome(&ctx.env, TOOL, Outcome::Hit, &label, &clock);
        return 0;
    }
    // a partial unpack leaves its files; rustc overwrites them
    let (status, outcome, subject) = compile_and_store(
        &mut client,
        ctx,
        rustc,
        &inv,
        &Lookup {
            k1: &k1,
            cwd: &cwd,
            label: &label,
            missed,
        },
    );
    log::outcome(&ctx.env, TOOL, outcome, &subject, &clock);
    status
}

/// `RunUncached` (`rustc_mode.cc:185-205`): under a slot unless there is no source (a probe),
/// stdio inherited, logged with why it was not cached.
fn run_uncached(
    client: &mut Client,
    ctx: &mut Ctx,
    rustc: &[u8],
    inv: &RustInvocation,
    have_source: bool,
    label: &[u8],
    clock: &Stopwatch,
) -> i32 {
    let status = if inv.source.is_empty() {
        process::run(rustc, &inv.args, Capture::Inherit, &ctx.env).status
    } else {
        slot_guard::hold(client, &ctx.socket, &mut ctx.env, |_, env| {
            process::run(rustc, &inv.args, Capture::Inherit, env).status
        })
    };
    let outcome = if inv.query {
        Outcome::PlainQuery
    } else if !inv.cacheable {
        if inv.links {
            Outcome::PlainLink
        } else {
            Outcome::PlainCompile
        }
    } else if !have_source {
        Outcome::PlainNoSource
    } else {
        Outcome::PlainNoSocket
    };
    log::outcome(&ctx.env, TOOL, outcome, label, clock);
    status
}

/// What the lookup left for the compile.
struct Lookup<'a> {
    k1: &'a RequestKey,
    cwd: &'a [u8],
    label: &'a [u8],
    /// `<label> <why>`.
    missed: Vec<u8>,
}

/// The compile and the store (`rustc_mode.cc:332-357`): status, outcome and log subject.
///
/// ADOPTED Q-CC-2 / Q-CLIENT-2: the slot covers the compile and the three PUTs, and DONE goes
/// out after them (legacy releases it before storing). Each PUT reply is checked; at the first
/// refused one `ec: cache put <key> failed` is printed, the remaining PUTs are skipped, and
/// the outcome is `compiled-unstored`. stderr is forwarded right after the compile, as before.
fn compile_and_store(
    client: &mut Client,
    ctx: &mut Ctx,
    rustc: &[u8],
    inv: &RustInvocation,
    l: &Lookup<'_>,
) -> (i32, Outcome, Vec<u8>) {
    let store = &ctx.store;
    slot_guard::hold(client, b"", &mut ctx.env, |client, env| {
        let run = process::run(rustc, &inv.args, Capture::Stderr, env);
        write_stderr(&run.captured);
        if run.status != 0 {
            // Q-RS-1: failures are never cached
            return (run.status, Outcome::MissFail, l.missed.clone());
        }
        // with cargo the dep-info is exactly <out-dir>/<crate><extra>.d; scanning would race
        // sibling variants
        let mut dep_name = concat(&inv.crate_name, &inv.extra_filename);
        dep_name.extend_from_slice(b".d");
        let Some(dep_text) = read_file(&fspath::join(&inv.out_dir, &dep_name)) else {
            return (0, Outcome::MissUnstored, l.missed.clone());
        };
        let info = depinfo::parse_dep_info(&dep_text, l.cwd);
        let primary = fspath::absolute_in(&inv.source, l.cwd)
            .map(|p| fspath::normalize(&p))
            .unwrap_or_default();
        // ADOPTED Q-RS-3: the values in the environment rustc inherited
        let env_lines: Vec<Vec<u8>> = info
            .env_deps
            .iter()
            .map(|name| env_line(name, getenv(env.env(), name).as_deref()))
            .collect();
        let manifest = match twokey::build_manifest_with_env(
            client,
            store,
            l.k1,
            &info.inputs,
            &primary,
            &[] as &[&[u8]],
            &env_lines,
        ) {
            Ok(m) => m,
            Err(Unstorable::Unreadable(path_key)) => {
                // ADOPTED Q-KEY-3: nothing is stored
                let mut subject = concat(l.label, b" not-cached:unreadable:");
                subject.extend_from_slice(&path_key);
                return (0, Outcome::MissUnstored, subject);
            }
        };
        let k2 = &manifest.result_key;
        let stored = put(client, &slot::manifest(l.k1), &manifest.text)
            && put(
                client,
                &slot::object(k2),
                &pack::pack_files(&info.outputs, &inv.extra_filename),
            )
            // Q-RS-2: stderr is stored raw
            && put(client, &slot::stderr(k2), &run.captured);
        let outcome = if stored {
            Outcome::MissStored
        } else {
            Outcome::MissUnstored
        };
        (0, outcome, l.missed.clone())
    })
}

/// A checked PUT (ADOPTED Q-CLIENT-2).
fn put(client: &mut Client, key: &[u8], value: &[u8]) -> bool {
    let ok = client.put(key, value);
    if !ok {
        log::put_failed(key);
    }
    ok
}

/// `getenv(name)` for a byte-string name.
fn getenv(env: &Env, name: &[u8]) -> Option<Vec<u8>> {
    env.pairs()
        .iter()
        .find(|(k, _)| k.as_slice() == name)
        .map(|(_, v)| v.clone())
}

fn concat(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(a.len() + b.len());
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    v
}

fn write_stderr(bytes: &[u8]) {
    let _ = std::io::stderr().write_all(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn getenv_is_first_match_and_distinguishes_empty() {
        let env = Env::from_pairs(&[("A", "1"), ("B", ""), ("A", "2")]);
        assert_eq!(getenv(&env, b"A"), Some(b"1".to_vec()));
        assert_eq!(getenv(&env, b"B"), Some(Vec::new()));
        assert_eq!(getenv(&env, b"C"), None);
    }
}
