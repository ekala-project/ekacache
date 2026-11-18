//! The bounds-checked view of a binary held in memory (`behaviors/jig-reloc-fixup.md
//! §Conventions used in this document`, *Image primitives*; `fixup_mode.h` `BinaryImage`).
//!
//! Every read and write of A5/A6 goes through these primitives with an explicit offset, so the
//! failure rules are exactly the documented ones: an access of `size` bytes at `off` fails iff
//! `off > L` or `L − off < size`. The image never changes length.

/// The file's bytes `B` (length `L`).
pub struct Image {
    pub bytes: Vec<u8>,
}

impl Image {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// `L`.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// The byte range of an access of `size` bytes at `off`, `None` iff `off > L` or
    /// `L − off < size`.
    fn span(&self, off: u64, size: u64) -> Option<std::ops::Range<usize>> {
        let l = self.len();
        // `off <= L` is checked first, so `L − off` cannot wrap, nor can `off + size` exceed L
        if off > l || l.wrapping_sub(off) < size {
            return None;
        }
        Some(off as usize..off.wrapping_add(size) as usize)
    }

    /// `Read<T>(off)` for a `T` of `N` bytes.
    #[must_use]
    pub fn read<const N: usize>(&self, off: u64) -> Option<[u8; N]> {
        let r = self.span(off, N as u64)?;
        self.bytes.get(r)?.try_into().ok()
    }

    /// `Write<T>(off, v)` and `Overwrite(off, data)`: the same rule, `false` on failure.
    pub fn overwrite(&mut self, off: u64, data: &[u8]) -> bool {
        let Some(r) = self.span(off, data.len() as u64) else {
            return false;
        };
        match self.bytes.get_mut(r) {
            Some(dst) => {
                dst.copy_from_slice(data);
                true
            }
            None => false,
        }
    }

    pub fn write_u32_le(&mut self, off: u64, v: u32) -> bool {
        self.overwrite(off, &v.to_le_bytes())
    }

    pub fn write_u64_le(&mut self, off: u64, v: u64) -> bool {
        self.overwrite(off, &v.to_le_bytes())
    }

    /// `CString(off)`: `""` if `off ≥ L`, else the bytes up to the first NUL or the end.
    #[must_use]
    pub fn cstring(&self, off: u64) -> &[u8] {
        if off >= self.len() {
            return b"";
        }
        let rest = self.bytes.get(off as usize..).unwrap_or_default();
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        &rest[..end]
    }

    /// `WritePadded(off, cap, text)`: fails iff `len(text)+1 > cap`, `off > L` or `L − off < cap`;
    /// otherwise `B[off .. off+cap)` = `text` + NULs.
    pub fn write_padded(&mut self, off: u64, cap: u64, text: &[u8]) -> bool {
        let n = text.len() as u64;
        // `len(text) + 1` cannot wrap: a slice is never 2^64 bytes long
        if n.wrapping_add(1) > cap {
            return false;
        }
        let Some(r) = self.span(off, cap) else {
            return false;
        };
        let Some(dst) = self.bytes.get_mut(r) else {
            return false;
        };
        let (head, tail) = dst.split_at_mut(text.len());
        head.copy_from_slice(text);
        tail.fill(0);
        true
    }

    /// `IsElf64LittleEndian`: `7F 'E' 'L' 'F'`, `L > 5`, ELFCLASS64, ELFDATA2LSB.
    #[must_use]
    pub fn is_elf64_le(&self) -> bool {
        self.bytes.starts_with(b"\x7fELF")
            && self.bytes.len() > 5
            && self.bytes[4] == 2
            && self.bytes[5] == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TestBinaryImage` (`legacy/jig/src/jig_test.cc:500`), §Test vectors.
    #[test]
    fn test_binary_image() {
        let mut bytes = vec![0u8; 64];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        let mut elf = Image::new(bytes);
        assert!(elf.is_elf64_le());
        assert!(elf.write_u32_le(16, 0xdead_beef));
        assert_eq!(elf.read(16).map(u32::from_le_bytes), Some(0xdead_beef));
        assert_eq!(elf.read::<8>(60), None); // would run past the end
        assert_eq!(elf.read::<2>(1000), None);
        assert!(!elf.write_u64_le(57, 1));
        assert!(elf.write_padded(32, 8, b"abc"));
        assert_eq!(elf.cstring(32), b"abc");
        assert!(!elf.write_padded(32, 3, b"abc")); // no room for the NUL
        assert!(!elf.write_padded(60, 8, b"abc"));
        assert_eq!(elf.cstring(5000), b"");
        assert!(!Image::new(b"short".to_vec()).is_elf64_le());
    }

    #[test]
    fn edges() {
        let mut img = Image::new(vec![1, 2, 3, 0, 5]);
        // an access ending exactly at L succeeds, one byte more fails
        assert_eq!(img.read::<2>(3), Some([0, 5]));
        assert_eq!(img.read::<2>(4), None);
        assert_eq!(img.read::<0>(5), Some([]));
        assert_eq!(img.read::<0>(6), None);
        assert_eq!(img.read::<8>(u64::MAX), None);
        assert_eq!(img.cstring(0), &[1, 2, 3]);
        assert_eq!(img.cstring(4), &[5]); // no NUL: to the end
        assert_eq!(img.cstring(5), b"");
        assert!(img.write_padded(0, 3, b"ab"));
        assert_eq!(img.bytes, [b'a', b'b', 0, 0, 5]);
        assert!(!img.write_padded(u64::MAX, 1, b""));
        assert!(img.overwrite(5, b""));
        assert!(!img.overwrite(6, b""));
        assert!(!Image::new(b"\x7fELF\x02".to_vec()).is_elf64_le());
        assert!(!Image::new(b"\x7fELF\x01\x01".to_vec()).is_elf64_le());
        assert!(Image::new(b"\x7fELF\x02\x01".to_vec()).is_elf64_le());
    }
}
