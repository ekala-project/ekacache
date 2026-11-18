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

    #[must_use]
    pub fn u8(&self, off: u64) -> Option<u8> {
        self.read::<1>(off).map(|b| b[0])
    }

    #[must_use]
    pub fn u32_le(&self, off: u64) -> Option<u32> {
        self.read(off).map(u32::from_le_bytes)
    }

    #[must_use]
    pub fn u64_le(&self, off: u64) -> Option<u64> {
        self.read(off).map(u64::from_le_bytes)
    }

    /// A big-endian u32 (code-signature blobs; `Big32` in `macho_fixup.cc`).
    #[must_use]
    pub fn u32_be(&self, off: u64) -> Option<u32> {
        self.read(off).map(u32::from_be_bytes)
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

    fn num(s: &str) -> u64 {
        match s.strip_prefix("0x") {
            Some(h) => u64::from_str_radix(h, 16).unwrap(),
            None => s.parse().unwrap(),
        }
    }

    /// One step of a `binary-image` vector: what `op args` gives, spelled as the vectors spell
    /// results.
    fn step(img: &mut Image, op: &str, args: &[&str]) -> String {
        let at = num(args[0]);
        let read = |v: Option<u64>| v.map_or("none".to_owned(), |v| format!("{v:#x}"));
        match op {
            "is_elf64_le" => img.is_elf64_le().to_string(),
            "read_u16" => read(img.read::<2>(at).map(|b| u16::from_le_bytes(b).into())),
            "read_u32" => read(img.u32_le(at).map(u64::from)),
            "read_u64" => read(img.u64_le(at)),
            "write_u32" => img
                .write_u32_le(at, u32::try_from(num(args[1])).unwrap())
                .to_string(),
            "write_u64" => img.write_u64_le(at, num(args[1])).to_string(),
            "write_padded" => img
                .write_padded(at, num(args[1]), args[2].as_bytes())
                .to_string(),
            "cstring" => String::from_utf8(img.cstring(at).to_vec()).unwrap(),
            _ => panic!("unknown op {op}"),
        }
    }

    /// `TestBinaryImage` (`legacy/jig/src/jig_test.cc:500`): the `binary-image` rows of
    /// `tests/vectors/jig-reloc-fixup.toml` (§Test vectors, *From `TestBinaryImage`*).
    #[test]
    fn test_binary_image() {
        let file = jig_testkit::vectors::VectorFile::load("jig-reloc-fixup");
        let mut steps = 0;
        for v in file.of_kind("binary-image", 2) {
            let mut bytes = v.hex("image_hex");
            bytes.resize(v.usize("image_len"), 0);
            let mut img = Image::new(bytes);
            for s in v.strs("steps") {
                let (call, want) = s.split_once(" ->").unwrap();
                let mut words = call.split(' ');
                let op = words.next().unwrap();
                // `is_elf64_le` takes no offset
                let args: Vec<&str> = words.chain(["0"]).collect();
                assert_eq!(
                    step(&mut img, op, &args),
                    want.trim_start(),
                    "{}: {s}",
                    v.label()
                );
                steps += 1;
            }
        }
        assert_eq!(steps, 12);
    }

    #[test]
    fn edges() {
        let mut img = Image::new(vec![1, 2, 3, 0, 5]);
        // an access ending exactly at L succeeds, one byte more fails
        assert_eq!(img.read::<2>(3), Some([0, 5]));
        assert_eq!(img.read::<2>(4), None);
        assert_eq!(img.read::<0>(5), Some([]));
        assert_eq!(img.read::<0>(6), None);
        assert_eq!(img.u64_le(u64::MAX), None);
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
