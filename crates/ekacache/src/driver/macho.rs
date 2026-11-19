//! The Mach-O policy (`behaviors/jig-driver-policy.md` A11): dependents record each dylib's
//! absolute install name, `jig reloc-fixup` respells those `@loader_path`-relative and needs
//! header room to do so. Platform-independent text; exercised on macOS in P9.

/// `MachOLink`: every policy link, shared or not.
pub fn macho_link(out: &mut Vec<Vec<u8>>) {
    out.push(b"-Wl,-headerpad_max_install_names".to_vec());
}
