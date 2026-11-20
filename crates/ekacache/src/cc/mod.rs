//! cc mode, ec's C/C++ compiler front end (`behaviors/ec-cc-mode.md`, `cc_mode.cc`).
//!
//! `run` is the ten-step pipeline of §Pipeline: expand response files, load the driver conf
//! (`behaviors/ec-driver-policy.md`), classify the arguments (`parse`), apply the adopted
//! uncacheable rules A–D (`uncacheable`), compute the primary identity and k1 (`primary`,
//! `key`), then replay a hit (`lookup`), compile and store a miss (`store`) or run the compiler
//! unchanged (`uncached`).

mod key;
mod lookup;
mod parse;
mod primary;
mod store;
mod uncacheable;
mod uncached;

use std::io::Write;

use crate::Ctx;
use crate::client::Client;
use crate::driver::{self, BinFmt, Language};
use crate::log::{self, Outcome, Stopwatch};
use crate::slot;
use crate::util::expand_response_files;

pub use key::request_key;
pub use parse::{
    has_linker_side_output, is_assembly, is_no_output_option, is_object_input, is_plain_assembly,
    is_source_file, parse_invocation,
};
pub use primary::mentions_build_time;
pub use uncacheable::{adopted_rules_refuse, foreign_temp_component, mentions_incbin};

/// What one compiler invocation is (`Invocation`, `cc_mode.h`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Passed to the real compiler.
    pub args: Vec<Vec<u8>>,
    /// What influences the output: all but `-o`, the depfile options, the source.
    pub key_args: Vec<Vec<u8>>,
    /// The one translation unit, or the output name of a link (the log label).
    pub source: Vec<u8>,
    /// Object, archive and shared-object arguments of a link.
    pub inputs: Vec<Vec<u8>>,
    /// `-include-pch` files.
    pub pch: Vec<Vec<u8>>,
    /// The object for `-c`, the text for `-E`/`-S`, else the executable or shared object.
    pub output: Vec<u8>,
    /// `-c`, `-S` or `-E`.
    pub compile_only: bool,
    /// `-E` without `-o` (or with `-o -`).
    pub to_stdout: bool,
    /// One source straight to an executable, no object inputs.
    pub link_one: bool,
    /// Objects only.
    pub link: bool,
    pub cacheable: bool,
    /// Asks the compiler something instead of building.
    pub query: bool,
    /// The build system asked for a depfile.
    pub wants_depfile: bool,
    pub depfile: Vec<u8>,
    /// `-MT`/`-MQ`.
    pub depfile_target: Vec<u8>,
}

impl Default for Invocation {
    fn default() -> Self {
        Self {
            args: Vec::new(),
            key_args: Vec::new(),
            source: Vec::new(),
            inputs: Vec::new(),
            pch: Vec::new(),
            output: Vec::new(),
            compile_only: false,
            to_stdout: false,
            link_one: false,
            link: false,
            cacheable: true,
            query: false,
            wants_depfile: false,
            depfile: Vec::new(),
            depfile_target: Vec::new(),
        }
    }
}

impl Invocation {
    /// `link || link_one`.
    #[must_use]
    pub fn links(&self) -> bool {
        self.link || self.link_one
    }
}

fn stderr(bytes: &[u8]) {
    let _ = std::io::stderr().write_all(bytes);
}

/// `std::print("{}", text); std::fflush(stdout)`.
fn stdout_flushed(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

/// `RunCcMode(argv0, raw_args, socket_path, launched)` (`:645-706`). `name` is the mode name
/// (the basename of `argv[0]` or `ec`'s first argument); `launched` is the program of launcher
/// mode, which selects the conf-less driver conf.
pub fn run(name: &[u8], args: &[Vec<u8>], launched: Option<&[u8]>, ctx: &mut Ctx) -> i32 {
    let clock = Stopwatch::start();
    // step 1: ghc puts the whole link behind one @rsp
    let user_args = expand_response_files(args);
    // step 2
    let conf = match launched {
        None => driver::load_driver_conf(&ctx.env),
        Some(program) => Some(driver::conf_less(program)),
    };
    let Some(conf) = conf else {
        stderr(b"ec: no etc/ekacache.json next to the binary and EKACACHE_CC unset\n");
        return 1;
    };
    let name = ekacache_core::fspath::filename(name);
    let lang = Language::of(name);
    if lang == Language::Fortran && conf.fc.is_empty() {
        let mut msg = b"ec: ".to_vec();
        msg.extend_from_slice(name);
        msg.extend_from_slice(b" called but etc/ekacache.json names no fc\n");
        stderr(&msg);
        return 1;
    }
    let compiler = if lang == Language::Fortran {
        conf.fc.clone()
    } else {
        conf.cc.clone()
    };

    // step 3: classify on what the build system said
    let mut inv = parse_invocation(&user_args, &ctx.env);
    // step 4: a cached link learns its inputs from ld.lld's --dependency-file
    if (inv.links() && conf.binfmt != BinFmt::Elf) || lang == Language::Fortran {
        inv.cacheable = false;
    }
    // ADOPTED Q-CC-13..16 (rules A, B, D): side outputs, hidden inputs, foreign temp files
    if adopted_rules_refuse(&user_args, &inv.source) {
        inv.cacheable = false;
    }
    // step 5
    if conf.present {
        let prefix_map = ctx.env.get_or("PKGS_PREFIX_MAP", b"").to_vec();
        inv.args = driver::build_driver_args(&conf, lang, &user_args, &prefix_map);
        let injected: Vec<Vec<u8>> = inv
            .args
            .iter()
            .filter(|a| !user_args.contains(a))
            .cloned()
            .collect();
        inv.key_args.extend(injected);
    }

    let mut client = Client::new();
    // step 6
    let mut primary = None;
    if inv.cacheable {
        primary = primary::primary_identity(&inv, &ctx.store);
        if let Some(p) = &primary {
            inv.cacheable = inv.link || !mentions_build_time(&p.text);
            // ADOPTED Q-CC-12 (rule C, before lookup): `.incbin` data is no depfile input
            if !inv.link && is_assembly(&inv.source) && mentions_incbin(p.source_bytes()) {
                inv.cacheable = false;
            }
        }
    }
    let ldflags: &[Vec<u8>] = if conf.present {
        &conf.package.ldflags
    } else {
        &[]
    };
    // step 7
    let has_primary = primary.is_some();
    let Some(mut primary) = primary.filter(|_| inv.cacheable) else {
        return uncached::run_uncached(
            &mut client,
            ctx,
            &compiler,
            &inv,
            has_primary,
            &user_args,
            ldflags,
            &clock,
        );
    };
    if !client.connect(&ctx.socket) {
        return uncached::run_uncached(
            &mut client,
            ctx,
            &compiler,
            &inv,
            true,
            &user_args,
            ldflags,
            &clock,
        );
    }

    // step 8
    primary.text.extend_from_slice(b"\ntoolchain=");
    let ids = primary::toolchain_ids(&mut client, &ctx.store, &compiler, &inv, &ctx.env);
    primary.text.extend_from_slice(&ids);
    let cwd = key::current_dir();
    let k1 = request_key(&ctx.store, &compiler, &inv, &primary.text, &ctx.env, &cwd);
    // step 9
    let miss = match lookup::lookup(&mut client, &ctx.store, &k1, &inv) {
        Ok(hit) => {
            let status = lookup::replay(&hit, &inv, &ctx.store);
            let outcome = if status == 0 {
                Outcome::Hit
            } else {
                Outcome::HitFail
            };
            log::outcome(&ctx.env, "cc", outcome, &inv.source, &clock);
            return status;
        }
        Err(miss) => miss,
    };
    // step 10, under the slot (ADOPTED Q-CC-2: DONE after the last PUT)
    let Ctx {
        env, store, socket, ..
    } = ctx;
    let (status, outcome, subject) = slot::hold(&mut client, socket, env, |client, env| {
        store::compile_and_store(client, store, env, &compiler, &k1, &inv, &miss)
    });
    log::outcome(env, "cc", outcome, &subject, &clock);
    status
}
