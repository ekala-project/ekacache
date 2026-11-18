//! reloc-fixup (`behaviors/jig-reloc-fixup.md`). Phase P8a.

use crate::Ctx;

pub fn run(args: &[Vec<u8>], ctx: &mut Ctx) -> i32 {
    let _ = (args, ctx);
    crate::not_yet("reloc-fixup")
}
