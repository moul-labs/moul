//! CRC-64 with the ISO polynomial, as Go's `hash/crc64` computes it with the
//! `crc64.ISO` table. LTX checksums files and pages with it, and `moul-vfs`
//! checksums its commit log with it.

const POLY: u64 = 0xD800_0000_0000_0000;

/// Slicing-by-8 tables: `TABLES[0]` is the classic byte table, and
/// `TABLES[k]` advances a byte through `k` more zero bytes.
const TABLES: [[u64; 256]; 8] = {
    let mut t = [[0u64; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u64;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        t[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

/// A running CRC-64/ISO checksum.
#[derive(Clone, Copy, Debug)]
pub struct Crc64(u64);

impl Default for Crc64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc64 {
    /// Starts an empty checksum.
    pub const fn new() -> Self {
        Self(!0)
    }

    /// Adds `bytes` to the checksum.
    pub fn update(&mut self, bytes: &[u8]) {
        let t = &TABLES;
        let mut crc = self.0;
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            crc ^= u64::from_le_bytes(*c);
            crc = t[7][(crc & 0xFF) as usize]
                ^ t[6][((crc >> 8) & 0xFF) as usize]
                ^ t[5][((crc >> 16) & 0xFF) as usize]
                ^ t[4][((crc >> 24) & 0xFF) as usize]
                ^ t[3][((crc >> 32) & 0xFF) as usize]
                ^ t[2][((crc >> 40) & 0xFF) as usize]
                ^ t[1][((crc >> 48) & 0xFF) as usize]
                ^ t[0][(crc >> 56) as usize];
        }
        for &b in rest {
            crc = t[0][((crc as u8) ^ b) as usize] ^ (crc >> 8);
        }
        self.0 = crc;
    }

    /// The checksum of everything added so far.
    pub const fn finish(&self) -> u64 {
        !self.0
    }

    /// The checksum of `bytes`.
    pub fn checksum(bytes: &[u8]) -> u64 {
        let mut crc = Self::new();
        crc.update(bytes);
        crc.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Crc64, TABLES};

    #[test]
    fn matches_go_crc64_iso() {
        // crc64.Checksum([]byte("123456789"), crc64.MakeTable(crc64.ISO)) in Go.
        assert_eq!(Crc64::checksum(b"123456789"), 0xB909_56C7_75A4_1001);
        assert_eq!(Crc64::checksum(b""), 0);
    }

    #[test]
    fn slicing_matches_the_bytewise_definition() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7919 % 256) as u8).collect();
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 999, 1000] {
            let mut slow = !0u64;
            for &b in &data[..len] {
                slow = TABLES[0][((slow as u8) ^ b) as usize] ^ (slow >> 8);
            }
            assert_eq!(Crc64::checksum(&data[..len]), !slow, "length {len}");
        }
    }

    #[test]
    fn incremental_equals_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut crc = Crc64::new();
        for chunk in data.chunks(77) {
            crc.update(chunk);
        }
        assert_eq!(crc.finish(), Crc64::checksum(&data));
    }
}
