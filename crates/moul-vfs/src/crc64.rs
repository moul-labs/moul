//! CRC-64 with the ISO polynomial, as Go's `hash/crc64` computes it with the
//! `crc64.ISO` table. LTX uses the same checksum, so `moul-ltx` can share it.

const POLY: u64 = 0xD800_0000_0000_0000;

const TABLE: [u64; 256] = {
    let mut table = [0u64; 256];
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
        table[i] = crc;
        i += 1;
    }
    table
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
        let mut crc = self.0;
        for &b in bytes {
            crc = TABLE[((crc as u8) ^ b) as usize] ^ (crc >> 8);
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
    use super::Crc64;

    #[test]
    fn matches_go_crc64_iso() {
        // crc64.Checksum([]byte("123456789"), crc64.MakeTable(crc64.ISO)) in Go.
        assert_eq!(Crc64::checksum(b"123456789"), 0xB909_56C7_75A4_1001);
        assert_eq!(Crc64::checksum(b""), 0);
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
