//! nix-store mode (`behaviors/jig-nix-store.md`). Phase P8b.

use crate::Ctx;

pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let _ = (args, ctx);
    crate::not_yet("nix-store")
}
