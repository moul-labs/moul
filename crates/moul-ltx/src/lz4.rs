//! LZ4, as LTX uses it: every page is one LZ4 *frame* holding one block.
//!
//! The Go LTX encoder runs each page through `pierrec/lz4`'s frame writer, so
//! a page on the wire is a complete frame: magic, descriptor, blocks, end mark
//! and maybe a content checksum. This module writes the smallest such frame
//! (independent 64 KiB blocks, no checksums, since the LTX file checksum
//! already covers the uncompressed bytes) and reads any frame the format
//! allows, including linked blocks and both kinds of checksum.
//!
//! References: the [block format] and the [frame format].
//!
//! [block format]: https://github.com/lz4/lz4/blob/dev/doc/lz4_Block_format.md
//! [frame format]: https://github.com/lz4/lz4/blob/dev/doc/lz4_Frame_format.md

use crate::{Error, Result};

const FRAME_MAGIC: u32 = 0x184D_2204;
const MIN_MATCH: usize = 4;
/// The last match must start at least this many bytes before the end.
const MF_LIMIT: usize = 12;
/// The last this many bytes are always literals.
const LAST_LITERALS: usize = 5;
const MAX_OFFSET: usize = 65_535;
const HASH_LOG: u32 = 12;

// ---- Block format -------------------------------------------------------------

fn hash(seq: u32) -> usize {
    (seq.wrapping_mul(2_654_435_761) >> (32 - HASH_LOG)) as usize
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn put_length(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

fn put_sequence(out: &mut Vec<u8>, literals: &[u8], m: Option<(usize, usize)>) {
    let lit = literals.len();
    let (offset, match_extra) = match m {
        Some((offset, len)) => (offset, len - MIN_MATCH),
        None => (0, 0),
    };
    let token = ((lit.min(15) as u8) << 4) | match_extra.min(15) as u8;
    out.push(token);
    if lit >= 15 {
        put_length(out, lit - 15);
    }
    out.extend_from_slice(literals);
    if m.is_some() {
        out.extend_from_slice(&(offset as u16).to_le_bytes());
        if match_extra >= 15 {
            put_length(out, match_extra - 15);
        }
    }
}

/// Compresses `src` as one LZ4 block and appends it to `out`. A greedy
/// single-pass matcher: fast and simple, and pages compress well enough.
pub fn compress_block(src: &[u8], out: &mut Vec<u8>) {
    let len = src.len();
    let mut anchor = 0;
    if len > MF_LIMIT {
        // Positions plus one, so zero means empty.
        let mut table = vec![0u32; 1 << HASH_LOG];
        let mf_limit = len - MF_LIMIT;
        let match_limit = len - LAST_LITERALS;
        let mut i = 0;
        while i < mf_limit {
            let seq = read_u32(src, i);
            let h = hash(seq);
            let cand = table[h] as usize;
            table[h] = i as u32 + 1;
            if cand > 0 {
                let cand = cand - 1;
                if i - cand <= MAX_OFFSET && read_u32(src, cand) == seq {
                    let mut m = MIN_MATCH;
                    while i + m < match_limit && src[cand + m] == src[i + m] {
                        m += 1;
                    }
                    put_sequence(out, &src[anchor..i], Some((i - cand, m)));
                    i += m;
                    anchor = i;
                    // Seed the table inside the match, for the next search.
                    if i < mf_limit && i >= 2 {
                        table[hash(read_u32(src, i - 2))] = (i - 2) as u32 + 1;
                    }
                    continue;
                }
            }
            i += 1;
        }
    }
    put_sequence(out, &src[anchor..], None);
}

fn read_length(src: &[u8], at: &mut usize, mut n: usize) -> Result<usize> {
    loop {
        let b = *src
            .get(*at)
            .ok_or_else(|| Error::corrupt("lz4 block ends inside a length"))?;
        *at += 1;
        n = n
            .checked_add(b as usize)
            .ok_or_else(|| Error::corrupt("lz4 length overflows"))?;
        if b != 255 {
            return Ok(n);
        }
    }
}

/// Decompresses one LZ4 block from `src`, appending to `out`. Matches may
/// reach back into what `out` already holds, which is how linked blocks in a
/// frame work. Fails if the block would take `out` past `limit` bytes.
pub fn decompress_block(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<()> {
    let mut at = 0;
    loop {
        let token = *src
            .get(at)
            .ok_or_else(|| Error::corrupt("lz4 block ends before its last sequence"))?;
        at += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            lit = read_length(src, &mut at, lit)?;
        }
        let literals = src
            .get(at..at.saturating_add(lit))
            .ok_or_else(|| Error::corrupt("lz4 literals run past the block"))?;
        if out.len() + lit > limit {
            return Err(Error::corrupt("lz4 block decompresses past its limit"));
        }
        out.extend_from_slice(literals);
        at += lit;
        if at == src.len() {
            return Ok(()); // the last sequence has no match
        }
        let offset = src
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
            .ok_or_else(|| Error::corrupt("lz4 block ends inside an offset"))?;
        at += 2;
        if offset == 0 || offset > out.len() {
            return Err(Error::corrupt(format!(
                "lz4 match offset {offset} reaches before the start of the output"
            )));
        }
        let mut m = (token & 15) as usize;
        if m == 15 {
            m = read_length(src, &mut at, m)?;
        }
        m += MIN_MATCH;
        if out.len() + m > limit {
            return Err(Error::corrupt("lz4 block decompresses past its limit"));
        }
        // A match may overlap what it writes (a run). Copying what is there
        // so far, again and again, doubles the run each time.
        let start = out.len() - offset;
        let mut left = m;
        while left > 0 {
            let n = (out.len() - start).min(left);
            out.extend_from_within(start..start + n);
            left -= n;
        }
    }
}

// ---- xxHash32, for frame checksums --------------------------------------------

const P1: u32 = 2_654_435_761;
const P2: u32 = 2_246_822_519;
const P3: u32 = 3_266_489_917;
const P4: u32 = 668_265_263;
const P5: u32 = 374_761_393;

fn round(acc: u32, lane: u32) -> u32 {
    acc.wrapping_add(lane.wrapping_mul(P2))
        .rotate_left(13)
        .wrapping_mul(P1)
}

/// xxHash32 of `b` with seed 0.
pub fn xxh32(b: &[u8]) -> u32 {
    let len = b.len();
    let mut at = 0;
    let mut h = if len >= 16 {
        let mut v = [P1.wrapping_add(P2), P2, 0, 0u32.wrapping_sub(P1)];
        while at + 16 <= len {
            for (i, lane) in v.iter_mut().enumerate() {
                *lane = round(*lane, read_u32(b, at + 4 * i));
            }
            at += 16;
        }
        v[0].rotate_left(1)
            .wrapping_add(v[1].rotate_left(7))
            .wrapping_add(v[2].rotate_left(12))
            .wrapping_add(v[3].rotate_left(18))
    } else {
        P5
    };
    h = h.wrapping_add(len as u32);
    while at + 4 <= len {
        h = h
            .wrapping_add(read_u32(b, at).wrapping_mul(P3))
            .rotate_left(17)
            .wrapping_mul(P4);
        at += 4;
    }
    while at < len {
        h = h
            .wrapping_add((b[at] as u32).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
        at += 1;
    }
    h ^= h >> 15;
    h = h.wrapping_mul(P2);
    h ^= h >> 13;
    h = h.wrapping_mul(P3);
    h ^ (h >> 16)
}

// ---- Frame format -------------------------------------------------------------

/// Appends `src` to `out` as one LZ4 frame. `src` must fit one 64 KiB block,
/// which every SQLite page does.
pub fn compress_frame(src: &[u8], out: &mut Vec<u8>) {
    assert!(
        src.len() <= 64 * 1024,
        "one frame holds at most 64 KiB here"
    );
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    // Version 01, independent blocks, no checksums, no content size.
    let descriptor = [0b0110_0000, 0b0100_0000];
    out.extend_from_slice(&descriptor);
    out.push((xxh32(&descriptor) >> 8) as u8);
    if !src.is_empty() {
        let size_at = out.len();
        out.extend_from_slice(&[0; 4]);
        compress_block(src, out);
        let mut size = (out.len() - size_at - 4) as u32;
        if size as usize >= src.len() {
            // Not worth it: store the block as it is.
            out.truncate(size_at + 4);
            out.extend_from_slice(src);
            size = src.len() as u32 | 0x8000_0000;
        }
        out[size_at..size_at + 4].copy_from_slice(&size.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn take<'a>(src: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at
        .checked_add(n)
        .filter(|&end| end <= src.len())
        .ok_or_else(|| Error::corrupt("lz4 frame is cut short"))?;
    let s = &src[*at..end];
    *at = end;
    Ok(s)
}

fn take_u32(src: &[u8], at: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        take(src, at, 4)?.try_into().expect("4 bytes"),
    ))
}

/// Reads one LZ4 frame from the start of `src` into `out`, which is cleared
/// first. Returns how many bytes of `src` the frame took. Fails if the frame
/// holds more than `limit` bytes.
pub fn decompress_frame(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize> {
    out.clear();
    let mut at = 0;
    if take_u32(src, &mut at)? != FRAME_MAGIC {
        return Err(Error::corrupt("not an lz4 frame: bad magic number"));
    }
    let desc_start = at;
    let flg = take(src, &mut at, 1)?[0];
    let bd = take(src, &mut at, 1)?[0];
    if flg >> 6 != 1 {
        return Err(Error::corrupt(format!(
            "lz4 frame version {} is not supported",
            flg >> 6
        )));
    }
    if flg & 0b10 != 0 || bd & 0b1000_1111 != 0 {
        return Err(Error::corrupt("lz4 frame sets reserved bits"));
    }
    let block_checksums = flg & 0b1_0000 != 0;
    let content_size = flg & 0b1000 != 0;
    let content_checksum = flg & 0b100 != 0;
    let dict_id = flg & 1 != 0;
    let block_max = match (bd >> 4) & 7 {
        4 => 64 << 10,
        5 => 256 << 10,
        6 => 1 << 20,
        7 => 4 << 20,
        n => {
            return Err(Error::corrupt(format!(
                "lz4 frame has unknown block size code {n}"
            )));
        }
    };
    let mut expected_size = None;
    if content_size {
        let b = take(src, &mut at, 8)?;
        expected_size = Some(u64::from_le_bytes(b.try_into().expect("8 bytes")));
    }
    if dict_id {
        return Err(Error::corrupt(
            "lz4 frames with a dictionary are not supported",
        ));
    }
    let hc = take(src, &mut at, 1)?[0];
    if hc != (xxh32(&src[desc_start..at - 1]) >> 8) as u8 {
        return Err(Error::corrupt("lz4 frame descriptor checksum mismatch"));
    }

    loop {
        let size = take_u32(src, &mut at)?;
        if size == 0 {
            break;
        }
        let stored = size & 0x8000_0000 != 0;
        let size = (size & 0x7FFF_FFFF) as usize;
        if size > block_max {
            return Err(Error::corrupt("lz4 block is larger than the frame allows"));
        }
        let data = take(src, &mut at, size)?;
        if block_checksums && take_u32(src, &mut at)? != xxh32(data) {
            return Err(Error::corrupt("lz4 block checksum mismatch"));
        }
        let before = out.len();
        let block_limit = limit.min(before + block_max);
        if stored {
            if before + size > block_limit {
                return Err(Error::corrupt("lz4 frame holds more than expected"));
            }
            out.extend_from_slice(data);
        } else {
            decompress_block(data, out, block_limit)?;
        }
    }
    if content_checksum && take_u32(src, &mut at)? != xxh32(out) {
        return Err(Error::corrupt("lz4 content checksum mismatch"));
    }
    if expected_size.is_some_and(|n| n != out.len() as u64) {
        return Err(Error::corrupt("lz4 frame content size does not match"));
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Vec<u8>> {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut noise = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng as u8
                })
                .collect()
        };
        let mut page = vec![0u8; 4096];
        page[..100].copy_from_slice(&noise(100));
        page[2000..2600].copy_from_slice(&b"hello world ".repeat(50));
        vec![
            vec![],
            vec![7],
            b"abcdefghijkl".to_vec(),
            b"abcabcabcabcabcabcabcabc".to_vec(),
            vec![0; 512],
            vec![0; 65536],
            noise(4096),
            noise(65536),
            page,
            (0..65536u32).map(|i| (i / 300) as u8).collect(),
        ]
    }

    #[test]
    fn blocks_round_trip() {
        for s in samples() {
            let mut c = Vec::new();
            compress_block(&s, &mut c);
            let mut d = Vec::new();
            decompress_block(&c, &mut d, s.len()).unwrap();
            assert_eq!(d, s);
        }
    }

    #[test]
    fn zeros_compress_well() {
        let mut c = Vec::new();
        compress_frame(&[0; 4096], &mut c);
        assert!(c.len() < 48, "a zero page took {} bytes", c.len());
    }

    #[test]
    fn frames_round_trip_and_report_their_length() {
        for s in samples() {
            let mut c = Vec::new();
            compress_frame(&s, &mut c);
            c.extend_from_slice(b"trailing");
            let mut d = Vec::new();
            let n = decompress_frame(&c, &mut d, s.len()).unwrap();
            assert_eq!(d, s);
            assert_eq!(&c[n..], b"trailing");
        }
    }

    #[test]
    fn decompression_respects_its_limit() {
        let mut c = Vec::new();
        compress_frame(&[1; 4096], &mut c);
        assert!(decompress_frame(&c, &mut Vec::new(), 4095).is_err());
    }

    #[test]
    fn xxh32_matches_reference_values() {
        assert_eq!(xxh32(b""), 0x02CC_5D05);
        assert_eq!(xxh32(b"a"), 0x550D_7456);
        assert_eq!(xxh32(b"abc"), 0x32D1_53FF);
        assert_eq!(
            xxh32(b"Nobody inspects the spammish repetition"),
            0xE229_3B2F
        );
    }

    #[test]
    fn reads_a_frame_written_by_the_lz4_tool() {
        // `printf 'hello hello hello hello hello' | lz4 -c -BX`, lz4 1.10.0:
        // block and content checksums on.
        let frame = [
            0x04, 0x22, 0x4d, 0x18, 0x74, 0x40, 0xbd, 0x0f, 0x00, 0x00, 0x00, 0x6e, 0x68, 0x65,
            0x6c, 0x6c, 0x6f, 0x20, 0x06, 0x00, 0x50, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x98, 0xf6,
            0xdc, 0x6d, 0x00, 0x00, 0x00, 0x00, 0x79, 0xb1, 0xff, 0xf9,
        ];
        let mut d = Vec::new();
        let n = decompress_frame(&frame, &mut d, 1 << 20).unwrap();
        assert_eq!(n, frame.len());
        assert_eq!(d, b"hello hello hello hello hello");
    }

    #[test]
    fn rejects_damage() {
        let mut c = Vec::new();
        compress_frame(&b"hello hello hello hello hello hello".repeat(20), &mut c);
        for i in 0..c.len() {
            let mut bad = c.clone();
            bad[i] ^= 0x55;
            let mut d = Vec::new();
            // Damage must be caught or change nothing the decoder can see; it
            // must never panic.
            let _ = decompress_frame(&bad, &mut d, 1 << 16);
        }
        assert!(decompress_frame(&c[..c.len() - 1], &mut Vec::new(), 1 << 16).is_err());
    }
}
