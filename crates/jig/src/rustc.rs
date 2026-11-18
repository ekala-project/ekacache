//! rustc mode (`behaviors/jig-rustc-mode.md`). Phase P6.

use crate::Ctx;

pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let _ = (args, ctx);
    crate::not_yet("rustcwrap")
}
