//! cc mode (`behaviors/jig-cc-mode.md`, `behaviors/jig-driver-policy.md`). Phase P5.

use crate::Ctx;

pub fn run(name: &[u8], args: &[Vec<u8>], launched: Option<&[u8]>, ctx: &mut Ctx) -> i32 {
    let _ = (name, args, launched, ctx);
    crate::not_yet("cc")
}
