//! Uncached runs (`behaviors/ec-cc-mode.md §Uncached runs`, `cc_mode.cc:520-552`): the compiler
//! as given, under a slot when it is real work.

use super::{Invocation, stdout_flushed};
use crate::Ctx;
use crate::client::Client;
use crate::driver::with_package_lib_dirs;
use crate::log::{self, Outcome, Stopwatch};
use crate::process::{self, Capture, ProcEnv};
use crate::slot;

/// `RunUncached(...)`: run, then log the outcome and (unless a query) the `$EKACACHE_LOG_ARGS` line.
#[allow(clippy::too_many_arguments)]
pub fn run_uncached(
    client: &mut Client,
    ctx: &mut Ctx,
    compiler: &[u8],
    inv: &Invocation,
    has_primary: bool,
    user_args: &[Vec<u8>],
    ldflags: &[Vec<u8>],
    clock: &Stopwatch,
) -> i32 {
    let work = |env: &mut ProcEnv| {
        // tools that search instead of link (libtool on mingw) see the dependency dirs too
        if user_args.iter().any(|a| a == b"-print-search-dirs") {
            let run = process::run(compiler, &inv.args, Capture::Stdout, env);
            stdout_flushed(&with_package_lib_dirs(&run.captured, ldflags));
            run.status
        } else {
            process::run(compiler, &inv.args, Capture::Inherit, env).status
        }
    };
    // -print-*, --version and friends are no work worth a slot (glibc runs 600 of them)
    let status = if inv.source.is_empty() && inv.inputs.is_empty() {
        work(&mut ctx.env)
    } else {
        slot::hold(client, &ctx.socket, &mut ctx.env, |_, env| work(env))
    };
    let outcome = if inv.query {
        Outcome::PlainQuery
    } else if !inv.cacheable {
        if inv.compile_only {
            Outcome::PlainCompile
        } else {
            Outcome::PlainLink
        }
    } else if !has_primary {
        Outcome::PlainNoSource
    } else {
        Outcome::PlainNoSocket
    };
    log::outcome(&ctx.env, "cc", outcome, &inv.source, clock);
    log::args(&ctx.env, outcome, user_args);
    status
}
