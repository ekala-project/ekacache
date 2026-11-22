//! `CompileAndStore` (`behaviors/ec-cc-mode.md §CompileAndStore`, `cc_mode.cc:296-431`): run
//! the real compiler, learn its inputs from the depfiles and the absent-path log, store what is
//! replayable.
//!
//! Called under the job slot (`super::run`): ADOPTED Q-CC-2 moves DONE after the last PUT, so
//! the slot covers the run, the forwarded output, the manifest's hashing and the upload.

use std::collections::HashSet;

use ekacache_core::bytes::{contains, find_byte_from, itoa, split};
use ekacache_core::fspath;
use ekacache_core::keys::{RequestKey, slot};
use ekacache_core::manifest::{Unstorable, parse_depfile};
use ekacache_core::store_path::{Store, is_store_path};

use super::lookup::{Miss, forward_stdout};
use super::primary::mentions_build_time;
use super::uncacheable::mentions_incbin;
use super::{Invocation, is_assembly, is_plain_assembly, stderr};
use crate::client::Client;
use crate::log::{self, Outcome};
use crate::process::{self, Capture, ProcEnv, RunResult, deterministic};
use crate::sys;
use crate::twokey::build_manifest;
use crate::util::read_file;

/// The real compiler run plus what it read (`Observed`).
struct Observed {
    run: RunResult,
    /// The preprocessor depfile: `""` for a pure link, `None` when unreadable.
    dep_text: Option<Vec<u8>>,
    /// lld's `--dependency-file`, links only.
    link_dep_text: Option<Vec<u8>>,
    inputs: Vec<Vec<u8>>,
    /// Looked up and not found (the patched clang's `$EKACACHE_ABSENT_LOG`).
    absent: Vec<Vec<u8>>,
}

/// `IsClangDriver`: the compiler's basename (not argv0) contains `clang`.
fn is_clang_driver(compiler: &[u8]) -> bool {
    contains(fspath::filename(compiler), b"clang")
}

/// `MissingInput`: clang's `file not found`, GCC's `No such file or directory`. Something absent
/// now may exist in a later build.
fn missing_input(stderr_text: &[u8]) -> bool {
    contains(stderr_text, b"file not found") || contains(stderr_text, b"No such file or directory")
}

/// libstdc++'s `fs::temp_directory_path(ec)`: the first of `$TMPDIR`, `$TMP`, `$TEMP`,
/// `$TEMPDIR` that is set (even empty), else `/tmp`; `""` unless it is a directory.
fn temp_directory_path(env: &ProcEnv) -> Vec<u8> {
    let dir = ["TMPDIR", "TMP", "TEMP", "TEMPDIR"]
        .iter()
        .find_map(|name| env.get(name))
        .unwrap_or(b"/tmp");
    if std::fs::metadata(ekacache_core::bytes::os_path(dir)).is_ok_and(|m| m.is_dir()) {
        dir.to_vec()
    } else {
        Vec::new()
    }
}

fn with_suffix(base: &[u8], suffix: &[u8]) -> Vec<u8> {
    [base, suffix].concat()
}

/// `RunObserved(cache, compiler, inv)` (`:306-381`).
fn run_observed(compiler: &[u8], inv: &Invocation, env: &mut ProcEnv) -> Observed {
    let mut args = inv.args.clone();
    let mut out_dir = fspath::parent_path(&inv.output);
    if out_dir.is_empty() {
        out_dir = b".".to_vec();
    }
    let tmp_base = fspath::join(
        &out_dir,
        format!(".ekacache{}", std::process::id()).as_bytes(),
    );
    let links = inv.links();
    let own_depfile = !inv.wants_depfile && !inv.link;
    let depfile = if own_depfile {
        with_suffix(&tmp_base, b".d")
    } else {
        inv.depfile.clone()
    };
    let link_depfile = with_suffix(&tmp_base, b".link.d");
    let absent_log = with_suffix(&tmp_base, b".absent");
    // nothing to preprocess: -MD would be unused
    let plain_asm = is_plain_assembly(&inv.source);
    if own_depfile && !plain_asm {
        args.extend([b"-MD".to_vec(), b"-MF".to_vec(), depfile.clone()]);
    } else if !own_depfile && !inv.link && !is_assembly(&inv.source) {
        // -MMD omits -isystem headers, which the manifest must see. Q-CC-10: not for assembly
        if is_clang_driver(compiler) {
            args.extend([b"-Xclang".to_vec(), b"-sys-header-deps".to_vec()]);
        } else {
            for arg in &mut args {
                if arg == b"-MMD" {
                    *arg = b"-MD".to_vec();
                } else if let Some(rest) = arg.strip_prefix(b"-Wp,-MMD,") {
                    *arg = with_suffix(b"-Wp,-MD,", rest);
                }
            }
        }
    }
    if inv.to_stdout {
        args.extend([b"-o".to_vec(), inv.output.clone()]);
    }
    if links {
        args.push(with_suffix(b"-Wl,--dependency-file=", &link_depfile));
    }

    env.set("EKACACHE_ABSENT_LOG", absent_log.clone());
    let run = process::run(compiler, &args, Capture::Stderr, env);
    env.remove("EKACACHE_ABSENT_LOG");
    stderr(&run.captured);
    let dep_text = if inv.link {
        Some(Vec::new())
    } else if own_depfile && plain_asm {
        Some(with_suffix(b"o: ", &inv.source))
    } else {
        read_file(&depfile)
    };
    let link_dep_text = if links {
        read_file(&link_depfile)
    } else {
        None
    };
    let absent = read_file(&absent_log)
        .map(|text| {
            split(&text, b'\n', false)
                .into_iter()
                .map(<[u8]>::to_vec)
                .collect()
        })
        .unwrap_or_default();
    sys::remove(&absent_log);
    if own_depfile {
        sys::remove(&depfile);
    }
    sys::remove(&link_depfile);
    let mut inputs = dep_text.as_deref().map(parse_depfile).unwrap_or_default();
    if let Some(text) = &link_dep_text {
        // the driver's temp object sits directly in $TMPDIR; the source tree may be below it
        let tmp = with_suffix(&temp_directory_path(env), b"/");
        inputs.extend(parse_depfile(text).into_iter().filter(|input| {
            !(input.starts_with(&tmp) && find_byte_from(input, b'/', tmp.len()).is_none())
        }));
    }
    Observed {
        run,
        dep_text,
        link_dep_text,
        inputs,
        absent,
    }
}

/// ADOPTED Q-CLIENT-2: sequential PUTs, each reply checked. The first failure is reported
/// (`ec: cache put <key> failed`, as in rustc and gocacheprog modes) and the rest are skipped;
/// the compile is then logged as not stored.
struct Puts<'a> {
    client: &'a mut Client,
    failed: bool,
}

impl Puts<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) {
        if !self.failed && !self.client.put(key, value) {
            log::put_failed(key);
            self.failed = true;
        }
    }
}

fn subject(source: &[u8], tail: &[&[u8]]) -> Vec<u8> {
    let mut s = source.to_vec();
    s.push(b' ');
    for part in tail {
        s.extend_from_slice(part);
    }
    s
}

/// The build-time check and rule C before storing.
struct Scan {
    build_time: bool,
    incbin: bool,
}

/// The build-time check (`behaviors/ec-cc-mode.md §Build-time check`) and ADOPTED Q-CC-12 (rule
/// C, before storing), reading each file once: the inputs whose ids `build_manifest` would hash
/// from their bytes get those ids remembered from the same read (`Store::content_id`).
///
/// - `build_time` (non-links): an input that is not a store path is readable and mentions
///   `__DATE__`/`__TIME__`/`__TIMESTAMP__`. Store headers do not use them; the build tree's
///   (version.h) might.
/// - `incbin`: the lookup's rule C said not to store, or (assembler compiles) the primary
///   source or an input mentions `.incbin`.
///
/// Once `build_time` holds nothing is stored and the scan stops (`build_time` is decided before
/// `incbin` on both paths of `compile_and_store`); once `incbin` holds no ids are remembered.
fn scan_inputs(store: &Store, inv: &Invocation, inputs: &[Vec<u8>], miss: &Miss) -> Scan {
    let check_build_time = !inv.links();
    let check_incbin = miss.store && !inv.link && is_assembly(&inv.source);
    let mut scan = Scan {
        build_time: false,
        incbin: !miss.store,
    };
    // the primary source is read for rule C even when the depfile does not list it
    let source = (check_incbin && !inputs.contains(&inv.source)).then_some(&inv.source);
    // a path seen before gives the same answers
    let mut done: HashSet<&[u8]> = HashSet::new();
    for (path, is_input) in source
        .into_iter()
        .map(|p| (p, false))
        .chain(inputs.iter().map(|p| (p, true)))
    {
        let wants_build_time = is_input && check_build_time && !is_store_path(path);
        let wants_incbin = check_incbin && !scan.incbin;
        if (!wants_build_time && !wants_incbin) || !done.insert(path) {
            continue;
        }
        let Some(bytes) = read_file(path) else {
            continue;
        };
        if wants_build_time && mentions_build_time(&bytes) {
            scan.build_time = true;
            break;
        }
        if wants_incbin && mentions_incbin(&bytes) {
            scan.incbin = true;
        }
        let hashed_locally =
            !is_store_path(path) || (store.by_content() && !store.daemon_may_identify(path));
        if is_input && !scan.incbin && hashed_locally && path != &inv.source {
            store.remember_identity(path, store.content_id(path, &bytes));
        }
    }
    scan
}

/// `CompileAndStore(cache, compiler, k1, inv, why)`: the compiler's status, the outcome and the
/// `$EKACACHE_LOG` subject (`<source> <why>`, or the adopted not-stored subjects:
/// `<source> not-cached:unreadable:<Key(path)>` (Q-KEY-3) and `<source> incbin` (Q-CC-12)).
pub fn compile_and_store(
    client: &mut Client,
    store: &Store,
    env: &mut ProcEnv,
    compiler: &[u8],
    k1: &RequestKey,
    inv: &Invocation,
    miss: &Miss,
) -> (i32, Outcome, Vec<u8>) {
    let links = inv.links();
    let obs = run_observed(compiler, inv, env);
    let missed = subject(&inv.source, &[&miss.why]);
    let Scan { build_time, incbin } = scan_inputs(store, inv, &obs.inputs, miss);
    let incbin_subject = || subject(&inv.source, &[b"incbin"]);
    let status = obs.run.status;

    if status != 0 {
        forward_stdout(inv, None);
        // replayable only if every input is known
        if obs.dep_text.is_none()
            || links
            || build_time
            || missing_input(&obs.run.captured)
            || !deterministic(&obs.run)
        {
            return (status, Outcome::MissFail, missed);
        }
        if incbin {
            return (status, Outcome::MissFail, incbin_subject());
        }
        let manifest =
            match build_manifest(client, store, k1, &obs.inputs, &inv.source, &obs.absent) {
                Ok(m) => m,
                Err(Unstorable::Unreadable(key)) => {
                    let s = subject(&inv.source, &[b"not-cached:unreadable:", &key]);
                    return (status, Outcome::MissFail, s);
                }
            };
        let mut puts = Puts {
            client,
            failed: false,
        };
        puts.put(&slot::manifest(k1), &manifest.text);
        let status_text =
            u64::try_from(status).map_or_else(|_| status.to_string().into_bytes(), itoa);
        puts.put(&slot::exit_status(&manifest.result_key), &status_text);
        puts.put(
            &slot::stderr(&manifest.result_key),
            &store.mask_out(&obs.run.captured),
        );
        if puts.failed {
            return (status, Outcome::MissFail, missed);
        }
        return (status, Outcome::MissStoredFail, missed);
    }

    let object = read_file(&inv.output);
    forward_stdout(inv, object.as_deref());
    let (Some(dep_text), Some(object)) = (&obs.dep_text, &object) else {
        return (0, Outcome::MissUnstored, missed);
    };
    if (links && obs.link_dep_text.is_none()) || build_time {
        return (0, Outcome::MissUnstored, missed);
    }
    if incbin {
        return (0, Outcome::MissUnstored, incbin_subject());
    }
    let manifest = match build_manifest(client, store, k1, &obs.inputs, &inv.source, &obs.absent) {
        Ok(m) => m,
        Err(Unstorable::Unreadable(key)) => {
            let s = subject(&inv.source, &[b"not-cached:unreadable:", &key]);
            return (0, Outcome::MissUnstored, s);
        }
    };
    let k2 = &manifest.result_key;
    let mut puts = Puts {
        client,
        failed: false,
    };
    puts.put(&slot::manifest(k1), &manifest.text);
    puts.put(&slot::object(k2), &store.mask_out(object));
    puts.put(&slot::stderr(k2), &store.mask_out(&obs.run.captured));
    // content mode: the depfile names this build's store paths; the next build must see its own
    if inv.wants_depfile {
        puts.put(
            &slot::depfile(k2),
            &store.mask_out(&store.mask_for_replay(dep_text)),
        );
    }
    if puts.failed {
        return (0, Outcome::MissUnstored, missed);
    }
    (0, Outcome::MissStored, missed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ekacache_core::env::Env;

    #[test]
    fn predicates() {
        assert!(is_clang_driver(b"/nix/store/x-clang-19/bin/clang++"));
        assert!(is_clang_driver(b"x86_64-linux-clang"));
        assert!(!is_clang_driver(b"/opt/clang/bin/gcc"));
        assert!(missing_input(
            b"a.c:1:10: fatal error: 'x.h' file not found\n"
        ));
        assert!(missing_input(
            b"a.c:1:10: fatal error: x.h: No such file or directory\n"
        ));
        assert!(!missing_input(b"a.c:1:1: error: expected ';'\n"));
    }

    #[test]
    fn temp_directory() {
        let env = |pairs: &[(&str, &str)]| ProcEnv::new(Env::from_pairs(pairs));
        assert_eq!(temp_directory_path(&env(&[])), b"/tmp");
        assert_eq!(temp_directory_path(&env(&[("TMP", "/")])), b"/");
        assert_eq!(
            temp_directory_path(&env(&[("TEMPDIR", "/"), ("TEMP", "/dev")])),
            b"/dev"
        );
        // set but empty, or not a directory: ""
        assert_eq!(
            temp_directory_path(&env(&[("TMPDIR", ""), ("TMP", "/")])),
            b""
        );
        assert_eq!(temp_directory_path(&env(&[("TMPDIR", "/dev/null")])), b"");
    }
}
