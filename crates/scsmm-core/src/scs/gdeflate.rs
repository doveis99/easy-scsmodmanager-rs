//! GDeflate decompressor.
//!
//! HashFS v2 stores packed textures (.tobj/.dds) with their pixel data in
//! GDeflate - a DEFLATE variant whose bitstream is interleaved across 32
//! streams (used by DirectStorage). A plain inflate cannot read it.
//!
//! Ported from the Python original, itself a port of sk-zk/GisDeflate
//! (managed C#, from libdeflate's inflate). Only decompression exists;
//! that is all the extractor needs.

use crate::{format_err, Result};

const NUM_STREAMS: usize = 32;
const LOW_WATERMARK: u32 = 32;
/// Each tile decompresses to 64 KiB (the last one may be shorter).
const TILE_SIZE: usize = 64 * 1024;
/// HashFS v2 sizes are 28-bit, so nothing legitimate is larger than this.
const MAX_OUTPUT: usize = 1 << 29;

const PRECODE_SYMS: usize = 19;
const LITLEN_SYMS: usize = 288;
const OFFSET_SYMS: usize = 32;
/// Longest codeword in any DEFLATE Huffman code.
const MAX_CW: usize = 15;
const LENS_OVERRUN: usize = 137;

const PRECODE_TABLEBITS: usize = 7;
const LITLEN_TABLEBITS: usize = 10;
const OFFSET_TABLEBITS: usize = 8;
const MAX_PRE_CW: usize = 7;
const MAX_LITLEN_CW: usize = 15;
const MAX_OFFSET_CW: usize = 15;

// decode-table entry layout (matches libdeflate)
const RESULT_SHIFT: u32 = 8;
const SUBTABLE_PTR: u32 = 0x8000_0000;
const LENGTH_MASK: u32 = 0xFF;
const LITERAL: u32 = 0x4000_0000;
const LENGTH_BASE_SHIFT: u32 = 8;
const EXTRA_LENGTH_MASK: u32 = 0xFF;
const EXTRA_OFFSET_SHIFT: u32 = 16;
const OFFSET_BASE_MASK: u32 = (1 << EXTRA_OFFSET_SHIFT) - 1;

const PRECODE_PERM: [usize; PRECODE_SYMS] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

// decode-table sizes (zlib's "enough" worst case for the TABLEBITS above)
const PRECODE_ENOUGH: usize = 128;
const LITLEN_ENOUGH: usize = 1334;
const OFFSET_ENOUGH: usize = 402;

const KDEFLATE_ID: u8 = 4;

fn bad(msg: &str) -> crate::Error {
    format_err(format!("gdeflate: {msg}"))
}

// ---- static decode-result tables: each Huffman symbol's table payload

fn precode_results() -> [u32; PRECODE_SYMS] {
    std::array::from_fn(|i| (i as u32) << RESULT_SHIFT)
}

fn litlen_results() -> [u32; LITLEN_SYMS] {
    // (length base, extra bits) for symbols 256..=287. The last three are
    // GDeflate's long-length symbols: base 3 plus 16 extra bits.
    const LENGTHS: [(u32, u32); 32] = [
        (0, 0), (3, 0), (4, 0), (5, 0), (6, 0), (7, 0), (8, 0), (9, 0), (10, 0),
        (11, 1), (13, 1), (15, 1), (17, 1), (19, 2), (23, 2), (27, 2), (31, 2),
        (35, 3), (43, 3), (51, 3), (59, 3), (67, 4), (83, 4), (99, 4), (115, 4),
        (131, 5), (163, 5), (195, 5), (227, 5), (3, 16), (3, 16), (3, 16),
    ];
    std::array::from_fn(|i| {
        if i < 256 {
            ((i as u32) << 8) | LITERAL
        } else {
            let (base, extra) = LENGTHS[i - 256];
            ((base << 8) | extra) << 8
        }
    })
}

fn offset_results() -> [u32; OFFSET_SYMS] {
    // (offset base, extra bits)
    const OFFSETS: [(u32, u32); 32] = [
        (1, 0), (2, 0), (3, 0), (4, 0), (5, 1), (7, 1), (9, 2), (13, 2),
        (17, 3), (25, 3), (33, 4), (49, 4), (65, 5), (97, 5), (129, 6), (193, 6),
        (257, 7), (385, 7), (513, 8), (769, 8), (1025, 9), (1537, 9), (2049, 10),
        (3073, 10), (4097, 11), (6145, 11), (8193, 12), (12289, 12), (16385, 13),
        (24577, 13), (32769, 14), (49153, 14),
    ];
    std::array::from_fn(|i| {
        let (base, extra) = OFFSETS[i];
        ((extra << EXTRA_OFFSET_SHIFT) | base) << RESULT_SHIFT
    })
}

/// Highest set bit of a non-zero value.
fn top_bit(x: u32) -> u32 {
    1 << (31 - x.leading_zeros())
}

/// Build a fast lookup table for a canonical Huffman code from its codeword
/// lengths. Ported from libdeflate; assumes bit-reversed codewords.
///
/// Every length in `lens[..num_syms]` must be `<= max_cw`; the table sizes
/// above are then large enough by construction.
fn build_decode_table(
    table: &mut [u32],
    lens: &[u8],
    num_syms: usize,
    results: &[u32],
    table_bits: usize,
    max_cw: usize,
    sorted_syms: &mut [u16],
) -> Result<()> {
    let mut len_counts = [0u32; MAX_CW + 2];
    let mut offsets = [0u32; MAX_CW + 2];
    for &len in &lens[..num_syms] {
        if len as usize > max_cw {
            return Err(bad("codeword length out of range"));
        }
        len_counts[len as usize] += 1;
    }
    offsets[1] = len_counts[0];
    let mut codespace: u32 = 0;
    for len in 1..max_cw {
        offsets[len + 1] = offsets[len] + len_counts[len];
        codespace = (codespace << 1) + len_counts[len];
    }
    codespace = (codespace << 1) + len_counts[max_cw];
    for (sym, &len) in lens[..num_syms].iter().enumerate() {
        sorted_syms[offsets[len as usize] as usize] = sym as u16;
        offsets[len as usize] += 1;
    }
    let mut ssi = offsets[0] as usize; // skip the unused (length 0) symbols

    if codespace > (1 << max_cw) {
        return Err(bad("overfull huffman code"));
    }
    if codespace < (1 << max_cw) {
        return Err(bad("incomplete huffman code"));
    }

    // a complete code has at least one codeword, so this loop ends
    let mut len = 1usize;
    while len_counts[len] == 0 {
        len += 1;
    }
    let mut codeword: u32 = 0;
    let mut count = len_counts[len];
    let mut cur_end = 1usize << len;

    // codewords that fit the main table
    while len <= table_bits {
        loop {
            table[codeword as usize] = results[sorted_syms[ssi] as usize] | len as u32;
            ssi += 1;
            if codeword as usize == cur_end - 1 {
                // last codeword: replicate the table up to its full size
                while len < table_bits {
                    table.copy_within(0..cur_end, cur_end);
                    cur_end <<= 1;
                    len += 1;
                }
                return Ok(());
            }
            let bit = top_bit(codeword ^ (cur_end as u32 - 1));
            codeword &= bit - 1;
            codeword |= bit;
            count -= 1;
            if count == 0 {
                break;
            }
        }
        loop {
            len += 1;
            if len <= table_bits {
                table.copy_within(0..cur_end, cur_end);
                cur_end <<= 1;
            }
            count = len_counts[len];
            if count != 0 {
                break;
            }
        }
    }

    // codewords longer than table_bits go into subtables
    let prefix_mask = (1u32 << table_bits) - 1;
    cur_end = 1 << table_bits;
    let mut subtable_prefix = u32::MAX;
    let mut subtable_start = 0usize;
    loop {
        if codeword & prefix_mask != subtable_prefix {
            subtable_prefix = codeword & prefix_mask;
            subtable_start = cur_end;
            let mut subtable_bits = len - table_bits;
            let mut space = count;
            while space < (1 << subtable_bits) {
                subtable_bits += 1;
                space = (space << 1) + len_counts[table_bits + subtable_bits];
            }
            cur_end = subtable_start + (1 << subtable_bits);
            table[subtable_prefix as usize] =
                SUBTABLE_PTR | ((subtable_start as u32) << RESULT_SHIFT) | subtable_bits as u32;
        }
        let entry = results[sorted_syms[ssi] as usize] | (len - table_bits) as u32;
        ssi += 1;
        let stride = 1usize << (len - table_bits);
        let mut i = subtable_start + (codeword >> table_bits) as usize;
        while i < cur_end {
            table[i] = entry;
            i += stride;
        }
        if codeword == (1u32 << len) - 1 {
            return Ok(());
        }
        let bit = top_bit(codeword ^ ((1u32 << len) - 1));
        codeword &= bit - 1;
        codeword |= bit;
        count -= 1;
        while count == 0 {
            len += 1;
            count = len_counts[len];
        }
    }
}

struct Tables {
    precode_results: [u32; PRECODE_SYMS],
    litlen_results: [u32; LITLEN_SYMS],
    offset_results: [u32; OFFSET_SYMS],
}

/// Decodes one tile into `out[out_next..out_end]`.
struct Inflater<'a> {
    page: &'a [u8],
    out: &'a mut [u8],
    tables: &'a Tables,
    in_next: usize,
    out_next: usize,
    out_end: usize,
    bitbuf: [u64; NUM_STREAMS],
    bitsleft: [u32; NUM_STREAMS],
    copy_len: [usize; NUM_STREAMS],
    copy_out: [usize; NUM_STREAMS],
    idx: usize,
    /// Bit 0 says whether the current stream owes a deferred match copy;
    /// the word rotates as the streams take turns.
    is_copy: u32,
    litlen: Vec<u32>,
    offset: Vec<u32>,
    precode: [u32; PRECODE_ENOUGH],
    lens: [u8; LITLEN_SYMS + OFFSET_SYMS + LENS_OVERRUN],
    precode_lens: [u8; PRECODE_SYMS],
    sorted_syms: [u16; LITLEN_SYMS],
}

impl<'a> Inflater<'a> {
    fn new(page: &'a [u8], out: &'a mut [u8], out_idx: usize, out_avail: usize, tables: &'a Tables) -> Self {
        Self {
            page,
            out,
            tables,
            in_next: 0,
            out_next: out_idx,
            out_end: out_idx + out_avail,
            bitbuf: [0; NUM_STREAMS],
            bitsleft: [0; NUM_STREAMS],
            copy_len: [0; NUM_STREAMS],
            copy_out: [0; NUM_STREAMS],
            idx: 0,
            is_copy: 0,
            litlen: vec![0; LITLEN_ENOUGH],
            offset: vec![0; OFFSET_ENOUGH],
            precode: [0; PRECODE_ENOUGH],
            lens: [0; LITLEN_SYMS + OFFSET_SYMS + LENS_OVERRUN],
            precode_lens: [0; PRECODE_SYMS],
            sorted_syms: [0; LITLEN_SYMS],
        }
    }

    /// Next 32 input bits; reading past the end yields zero bits, which a
    /// well-formed stream never consumes.
    fn le32(&self) -> u64 {
        let mut word = [0u8; 4];
        if let Some(rest) = self.page.get(self.in_next..) {
            let n = rest.len().min(4);
            word[..n].copy_from_slice(&rest[..n]);
        }
        u32::from_le_bytes(word) as u64
    }

    fn ensure(&mut self, n: u32) {
        if self.bitsleft[self.idx] < n {
            self.bitbuf[self.idx] |= self.le32() << self.bitsleft[self.idx];
            self.in_next += 4;
            self.bitsleft[self.idx] += 32;
        }
    }

    fn bits(&self, n: u32) -> u32 {
        (self.bitbuf[self.idx] & ((1u64 << n) - 1)) as u32
    }

    fn remove(&mut self, n: u32) -> Result<()> {
        if self.bitsleft[self.idx] < n {
            return Err(bad("bit stream underrun"));
        }
        self.bitbuf[self.idx] >>= n;
        self.bitsleft[self.idx] -= n;
        Ok(())
    }

    fn pop(&mut self, n: u32) -> Result<u32> {
        let v = self.bits(n);
        self.remove(n)?;
        Ok(v)
    }

    fn advance(&mut self) {
        self.ensure(LOW_WATERMARK);
        self.idx = (self.idx + 1) % NUM_STREAMS;
        self.is_copy = self.is_copy.rotate_right(1);
    }

    /// Table lookup with one level of subtable indirection; consumes the
    /// codeword's bits and returns the entry.
    fn decode(&mut self, litlen: bool, table_bits: u32) -> Result<u32> {
        let index = self.bits(table_bits) as usize;
        let mut entry = if litlen { self.litlen[index] } else { self.offset[index] };
        if entry & SUBTABLE_PTR != 0 {
            self.remove(table_bits)?;
            let sub = ((entry >> RESULT_SHIFT) & 0xFFFF) as usize + self.bits(entry & LENGTH_MASK) as usize;
            let table = if litlen { &self.litlen } else { &self.offset };
            entry = *table.get(sub).ok_or_else(|| bad("bad subtable index"))?;
        }
        self.remove(entry & LENGTH_MASK)?;
        Ok(entry)
    }

    /// Carry out the match this stream announced on its previous turn.
    fn do_copy(&mut self) -> Result<()> {
        let length = self.copy_len[self.idx];
        let dst = self.copy_out[self.idx];
        let entry = self.decode(false, OFFSET_TABLEBITS as u32)? >> RESULT_SHIFT;
        let offset = (entry & OFFSET_BASE_MASK) as usize + self.pop(entry >> EXTRA_OFFSET_SHIFT)? as usize;
        if offset == 0 || offset > dst {
            return Err(bad("match offset before the start of the output"));
        }
        // byte by byte: source and destination may overlap
        for i in dst..dst + length {
            self.out[i] = self.out[i - offset];
        }
        Ok(())
    }

    fn build_huffman(&mut self, num_litlen: usize, num_offset: usize) -> Result<()> {
        build_decode_table(
            &mut self.offset,
            &self.lens[num_litlen..],
            num_offset,
            &self.tables.offset_results,
            OFFSET_TABLEBITS,
            MAX_OFFSET_CW,
            &mut self.sorted_syms,
        )?;
        build_decode_table(
            &mut self.litlen,
            &self.lens,
            num_litlen,
            &self.tables.litlen_results,
            LITLEN_TABLEBITS,
            MAX_LITLEN_CW,
            &mut self.sorted_syms,
        )
    }

    fn main_loop(&mut self) -> Result<()> {
        self.idx = 0;
        loop {
            if self.is_copy & 1 == 0 {
                let entry = self.decode(true, LITLEN_TABLEBITS as u32)?;
                if entry & LITERAL != 0 {
                    if self.out_next == self.out_end {
                        return Err(bad("output overrun"));
                    }
                    self.out[self.out_next] = ((entry >> RESULT_SHIFT) & 0xFF) as u8;
                    self.out_next += 1;
                    self.advance();
                    continue;
                }
                let entry = entry >> RESULT_SHIFT;
                let length = ((entry >> LENGTH_BASE_SHIFT) + self.pop(entry & EXTRA_LENGTH_MASK)?) as usize;
                if length == 0 {
                    return Ok(()); // end of block
                }
                if length > self.out_end - self.out_next {
                    return Err(bad("output overrun"));
                }
                self.copy_len[self.idx] = length;
                self.copy_out[self.idx] = self.out_next;
                self.is_copy |= 1;
                self.out_next += length;
            } else {
                self.do_copy()?;
                self.is_copy &= !1;
            }
            self.advance();
        }
    }

    /// Flush the copies still pending in the other streams.
    fn block_done(&mut self) -> Result<()> {
        for _ in 0..NUM_STREAMS {
            if self.is_copy & 1 != 0 {
                self.do_copy()?;
                self.is_copy &= !1;
            }
            self.advance();
        }
        Ok(())
    }

    fn read_dynamic_header(&mut self) -> Result<()> {
        let num_litlen = self.pop(5)? as usize + 257;
        let num_offset = self.pop(5)? as usize + 1;
        let num_explicit = self.pop(4)? as usize + 4;
        self.ensure(LOW_WATERMARK);
        self.precode_lens = [0; PRECODE_SYMS];
        for &sym in &PRECODE_PERM[..num_explicit] {
            self.precode_lens[sym] = self.pop(3)? as u8;
            self.advance();
        }
        let results = self.tables.precode_results;
        build_decode_table(
            &mut self.precode,
            &self.precode_lens,
            PRECODE_SYMS,
            &results,
            PRECODE_TABLEBITS,
            MAX_PRE_CW,
            &mut self.sorted_syms,
        )?;

        self.idx = 0;
        let total = num_litlen + num_offset;
        let mut i = 0usize;
        while i < total {
            let entry = self.precode[self.bits(MAX_PRE_CW as u32) as usize];
            self.remove(entry & LENGTH_MASK)?;
            let presym = entry >> RESULT_SHIFT;
            // every run below stays inside `lens` thanks to LENS_OVERRUN
            match presym {
                0..=15 => {
                    self.lens[i] = presym as u8;
                    i += 1;
                }
                16 => {
                    if i == 0 {
                        return Err(bad("repeat with no previous length"));
                    }
                    let rep = self.lens[i - 1];
                    let count = 3 + self.pop(2)? as usize;
                    self.lens[i..i + 6].fill(rep);
                    i += count;
                }
                17 => {
                    let count = 3 + self.pop(3)? as usize;
                    self.lens[i..i + 10].fill(0);
                    i += count;
                }
                _ => {
                    let count = 11 + self.pop(7)? as usize;
                    self.lens[i..i + count].fill(0);
                    i += count;
                }
            }
            self.advance();
        }
        self.build_huffman(num_litlen, num_offset)
    }

    fn run(&mut self) -> Result<()> {
        self.idx = 0;
        for _ in 0..NUM_STREAMS {
            self.advance();
        }
        loop {
            self.idx = 0;
            let is_final = self.pop(1)? != 0;
            let block_type = self.pop(2)?;
            self.ensure(LOW_WATERMARK);

            match block_type {
                2 => self.read_dynamic_header()?,
                0 => {
                    // stored: the bytes are dealt out across the streams too
                    let len = self.pop(16)? as usize;
                    if len > self.out_end - self.out_next {
                        return Err(bad("output overrun"));
                    }
                    for _ in 0..len {
                        self.out[self.out_next] = self.pop(8)? as u8;
                        self.out_next += 1;
                        self.advance();
                    }
                    self.block_done()?;
                    if is_final {
                        return Ok(());
                    }
                    continue;
                }
                1 => {
                    self.lens[..144].fill(8);
                    self.lens[144..256].fill(9);
                    self.lens[256..280].fill(7);
                    self.lens[280..288].fill(8);
                    self.lens[288..320].fill(5);
                    self.build_huffman(288, 32)?;
                }
                _ => return Err(bad("reserved block type")),
            }

            self.main_loop()?;
            self.block_done()?;
            if is_final {
                return Ok(());
            }
        }
    }
}

/// Decompress a complete GDeflate stream.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 8 {
        return Err(bad("truncated stream"));
    }
    let (id, magic) = (data[0], data[1]);
    if magic != id ^ 0xFF {
        return Err(bad("malformed stream"));
    }
    if id != KDEFLATE_ID {
        return Err(bad("unknown compressor id"));
    }
    let num_tiles = u16::from_le_bytes([data[2], data[3]]) as usize;
    let bitfield = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let last_tile_size = ((bitfield >> 2) & 0x3FFFF) as usize;
    if last_tile_size > TILE_SIZE {
        return Err(bad("bad last tile size"));
    }
    let shortfall = if last_tile_size == 0 { 0 } else { TILE_SIZE - last_tile_size };
    let uncompressed = (num_tiles * TILE_SIZE)
        .checked_sub(shortfall)
        .filter(|n| *n <= MAX_OUTPUT)
        .ok_or_else(|| bad("implausible output size"))?;

    let table_end = 8 + num_tiles * 4;
    let offsets: Vec<usize> = data
        .get(8..table_end)
        .ok_or_else(|| bad("truncated tile table"))?
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as usize)
        .collect();
    let input = &data[table_end..];

    let tables = Tables {
        precode_results: precode_results(),
        litlen_results: litlen_results(),
        offset_results: offset_results(),
    };
    let mut out = vec![0u8; uncompressed];
    for tile in 0..num_tiles {
        // tile 0 starts at 0; offsets[0] doubles as the last tile's length
        let start = if tile > 0 { offsets[tile] } else { 0 };
        let len = if tile + 1 < num_tiles {
            offsets[tile + 1]
                .checked_sub(start)
                .ok_or_else(|| bad("tile offsets not ascending"))?
        } else {
            offsets[0]
        };
        let page = input
            .get(start..start.saturating_add(len))
            .ok_or_else(|| bad("tile runs past the end of the stream"))?;
        let out_idx = tile * TILE_SIZE;
        let avail = TILE_SIZE.min(uncompressed - out_idx);
        Inflater::new(page, &mut out, out_idx, avail, &tables).run()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // real packed textures from the game, shipped as fixtures by the
    // Python original together with their expected output
    const TEX0: &[u8] = include_bytes!("../../fixtures/scs_textures/tex0.gdeflate");
    const TEX0_RAW: &[u8] = include_bytes!("../../fixtures/scs_textures/tex0.raw");
    const TEX1: &[u8] = include_bytes!("../../fixtures/scs_textures/tex1.gdeflate");
    const TEX1_RAW: &[u8] = include_bytes!("../../fixtures/scs_textures/tex1.raw");

    #[test]
    fn decodes_the_reference_textures_byte_for_byte() {
        assert_eq!(decompress(TEX0).unwrap(), TEX0_RAW);
        assert_eq!(decompress(TEX1).unwrap(), TEX1_RAW);
    }

    #[test]
    fn rejects_bad_headers() {
        assert!(decompress(b"").is_err());
        assert!(decompress(&[4, 0xFB, 0, 0]).is_err()); // too short
        assert!(decompress(&[4, 0x00, 1, 0, 0, 0, 0, 0]).is_err()); // magic mismatch
        assert!(decompress(&[5, 0xFA, 1, 0, 0, 0, 0, 0]).is_err()); // unknown id
        assert!(decompress(&[4, 0xFB, 1, 0, 0, 0, 0, 0]).is_err()); // no tile table
    }

    #[test]
    fn damaged_streams_fail_without_panicking() {
        // truncate, and flip bytes throughout: every outcome must be a clean
        // Ok or Err, never a panic or an out-of-bounds write
        for cut in [9, 20, 64, TEX0.len() / 2, TEX0.len() - 1] {
            let _ = decompress(&TEX0[..cut]);
        }
        for i in (8..TEX0.len()).step_by(3) {
            let mut bad = TEX0.to_vec();
            bad[i] ^= 0x5A;
            let _ = decompress(&bad);
        }
        let mut huge = TEX0.to_vec();
        huge[2] = 0xFF;
        huge[3] = 0xFF; // 65535 tiles
        assert!(decompress(&huge).is_err());
    }
}
