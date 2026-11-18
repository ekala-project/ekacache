//! `jig cache` and `jig slot` (`behaviors/jig-cache-commands.md`). Phase P4.

use crate::Ctx;

pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let _ = (args, ctx);
    crate::not_yet("cache")
}

pub fn slot(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let _ = (args, ctx);
    crate::not_yet("slot")
}
