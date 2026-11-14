//! Rebuild a .dds file from a HashFS v2 packed-texture entry.
//!
//! A v2 texture entry carries 12 bytes of tobj/dds metadata (dimensions,
//! DXGI format, mip/face counts, row/image alignment) plus the surface
//! pixel data, GDeflate-compressed. The game strips the DDS header and pads
//! each row and mip to an alignment; to get a usable .dds back the header
//! is rebuilt and that padding undone. Ported from the Python original
//! (which follows TruckLib.HashFs).

use super::gdeflate;
use crate::{format_err, Result};

const DDS_MAGIC: u32 = 0x2053_4444; // "DDS "
const DX10_FOURCC: u32 = 0x3031_5844; // "DX10" - the extended header is always emitted

// DDS_HEADER.dwFlags
const DDSD_CAPS: u32 = 0x1;
const DDSD_HEIGHT: u32 = 0x2;
const DDSD_WIDTH: u32 = 0x4;
const DDSD_PIXELFORMAT: u32 = 0x1000;
const DDSD_MIPMAPCOUNT: u32 = 0x20000;
// DDS_HEADER.dwCaps
const DDSCAPS_COMPLEX: u32 = 0x8;
const DDSCAPS_MIPMAP: u32 = 0x40_0000;
const DDSCAPS_TEXTURE: u32 = 0x1000;
// DDS_HEADER.dwCaps2
const DDSCAPS2_CUBEMAP: u32 = 0x200;
const CUBE_FACES: u32 = 0x400 | 0x800 | 0x1000 | 0x2000 | 0x4000 | 0x8000;
const DDPF_FOURCC: u32 = 0x4;

/// A texture larger than this is not something a mod ships.
const MAX_SURFACE: usize = 1 << 29;

/// The 12-byte tobj/dds metadata that precedes a v2 image entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TobjMeta {
    pub width: u32,
    pub height: u32,
    pub mip: u32,
    pub format: u32,
    pub is_cube: bool,
    pub face: u32,
    pub pitch_align: usize,
    pub image_align: usize,
}

impl TobjMeta {
    pub fn parse(raw: &[u8; 12]) -> Self {
        let img = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        Self {
            width: u16::from_le_bytes([raw[0], raw[1]]) as u32 + 1,
            height: u16::from_le_bytes([raw[2], raw[3]]) as u32 + 1,
            mip: (img & 0xF) + 1,
            format: (img >> 4) & 0xFF,
            is_cube: (img >> 12) & 0x3 != 0,
            face: ((img >> 14) & 0x3F) + 1,
            pitch_align: 1 << ((img >> 20) & 0xF),
            image_align: 1 << ((img >> 24) & 0xF),
        }
    }
}

/// Bits per pixel of the DXGI formats that are neither block-compressed
/// nor packed (the DirectXTex table).
fn bits_per_pixel(format: u32) -> Option<usize> {
    Some(match format {
        1..=4 => 128,
        5..=8 => 96,
        9..=22 | 102 | 108 | 109 => 64,
        23..=47 | 67..=69 | 87..=93 | 100 | 101 | 107 => 32,
        104 | 105 => 24,
        48..=59 | 85 | 86 | 114 | 115 => 16,
        103 | 106 | 110 => 12,
        60..=65 | 111..=113 => 8,
        66 => 1,
        _ => return None,
    })
}

/// `(row_pitch, slice_pitch)` in bytes for one mip level.
fn surface_info(w: usize, h: usize, format: u32) -> Result<(usize, usize)> {
    let block = match format {
        70..=72 | 79..=81 => Some(8),           // BC1, BC4
        73..=78 | 82..=84 | 94..=99 => Some(16), // BC2, BC3, BC5, BC6H, BC7
        _ => None,
    };
    if let Some(block) = block {
        let row = w.div_ceil(4).max(1) * block;
        return Ok((row, row * h.div_ceil(4).max(1)));
    }
    Ok(match format {
        68 | 69 | 107 => {
            let row = w.div_ceil(2) * 4;
            (row, row * h)
        }
        108 | 109 => {
            let row = w.div_ceil(2) * 8;
            (row, row * h)
        }
        110 => {
            let row = w.div_ceil(4) * 4;
            (row, row * h * 2)
        }
        103 | 106 => {
            let row = w.div_ceil(2) * 2;
            (row, row * h + (row * h).div_ceil(2))
        }
        104 | 105 => {
            let row = w.div_ceil(2) * 4;
            (row, row * h + (row * h).div_ceil(2))
        }
        _ => {
            let bpp = bits_per_pixel(format)
                .ok_or_else(|| format_err(format!("unsupported dxgi format {format}")))?;
            let row = (w * bpp).div_ceil(8);
            (row, row * h)
        }
    })
}

fn align_up(x: usize, n: usize) -> usize {
    x.div_ceil(n) * n
}

/// The stored surface pads each mip to `image_align` and each row to
/// `pitch_align`; a DDS wants it tightly packed, so copy row by row.
fn unpad_surface(meta: &TobjMeta, surface: &[u8]) -> Result<Vec<u8>> {
    let mut levels = Vec::with_capacity(meta.mip as usize);
    let (mut w, mut h) = (meta.width as usize, meta.height as usize);
    for _ in 0..meta.mip {
        levels.push(surface_info(w, h, meta.format)?);
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    let packed: usize = levels.iter().map(|(_, slice)| slice).sum::<usize>() * meta.face as usize;
    if packed > MAX_SURFACE {
        return Err(format_err("implausible texture size"));
    }

    let mut out = Vec::with_capacity(packed);
    let mut x = 0usize;
    for _ in 0..meta.face {
        for &(row_pitch, slice_pitch) in &levels {
            x = align_up(x, meta.image_align);
            for _ in (0..slice_pitch).step_by(row_pitch.max(1)) {
                x = align_up(x, meta.pitch_align);
                // a short surface yields a short (or empty) row, like the
                // reference implementation; nothing is invented
                let end = (x + row_pitch).min(surface.len());
                if x < end {
                    out.extend_from_slice(&surface[x..end]);
                }
                x += row_pitch;
            }
        }
    }
    Ok(out)
}

fn dds_header(meta: &TobjMeta) -> Vec<u8> {
    let flags = DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PIXELFORMAT | DDSD_MIPMAPCOUNT;
    let mut caps = DDSCAPS_TEXTURE;
    if meta.mip > 1 {
        caps |= DDSCAPS_MIPMAP | DDSCAPS_COMPLEX;
    }
    let mut caps2 = 0;
    if meta.is_cube {
        caps |= DDSCAPS_COMPLEX;
        caps2 = DDSCAPS2_CUBEMAP | CUBE_FACES;
    }

    let mut h: Vec<u8> = Vec::with_capacity(148);
    let mut put = |values: &[u32]| {
        for v in values {
            h.extend_from_slice(&v.to_le_bytes());
        }
    };
    put(&[DDS_MAGIC, 124, flags, meta.height, meta.width]);
    put(&[0, 0, meta.mip]); // pitch, depth, mip count
    put(&[0; 11]); // reserved
    // DDS_PIXELFORMAT: always the DX10 four-cc, masks unused
    put(&[32, DDPF_FOURCC, DX10_FOURCC, 0, 0, 0, 0, 0]);
    put(&[caps, caps2, 0, 0, 0]);
    // DDS_HEADER_DXT10: format, Texture2D, cube flag, array size, misc flags 2
    put(&[meta.format, 3, if meta.is_cube { 4 } else { 0 }, 1, 0]);
    h
}

/// Rebuild a complete .dds file from a packed v2 image entry.
pub fn reconstruct_dds(tobj_meta: &[u8; 12], payload: &[u8], compressed: bool) -> Result<Vec<u8>> {
    let meta = TobjMeta::parse(tobj_meta);
    let decompressed;
    let surface = if compressed {
        decompressed = gdeflate::decompress(payload)?;
        &decompressed[..]
    } else {
        payload
    };
    let mut dds = dds_header(&meta);
    dds.extend_from_slice(&unpad_surface(&meta, surface)?);
    Ok(dds)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEX0: &[u8] = include_bytes!("../../fixtures/scs_textures/tex0.gdeflate");
    const TEX0_META: &[u8; 12] = include_bytes!("../../fixtures/scs_textures/tex0.tobj12");
    const TEX0_DDS: &[u8] = include_bytes!("../../fixtures/scs_textures/tex0.dds");
    const TEX0_RAW: &[u8] = include_bytes!("../../fixtures/scs_textures/tex0.raw");
    const TEX1: &[u8] = include_bytes!("../../fixtures/scs_textures/tex1.gdeflate");
    const TEX1_META: &[u8; 12] = include_bytes!("../../fixtures/scs_textures/tex1.tobj12");
    const TEX1_DDS: &[u8] = include_bytes!("../../fixtures/scs_textures/tex1.dds");

    #[test]
    fn metadata_fields() {
        // 7f00 7f00 d7048009 27010000
        let m = TobjMeta::parse(TEX0_META);
        assert_eq!((m.width, m.height), (128, 128));
        assert_eq!(m.mip, 8);
        assert!(!m.is_cube);
        assert_eq!(m.face, 1);
        assert!(m.pitch_align.is_power_of_two() && m.image_align.is_power_of_two());
    }

    #[test]
    fn rebuilds_the_reference_dds_byte_for_byte() {
        assert_eq!(reconstruct_dds(TEX0_META, TEX0, true).unwrap(), TEX0_DDS);
        assert_eq!(reconstruct_dds(TEX1_META, TEX1, true).unwrap(), TEX1_DDS);
        // an entry stored without compression goes through the same unpadding
        assert_eq!(reconstruct_dds(TEX0_META, TEX0_RAW, false).unwrap(), TEX0_DDS);
    }

    #[test]
    fn header_layout() {
        let dds = reconstruct_dds(TEX0_META, TEX0, true).unwrap();
        assert_eq!(&dds[..4], b"DDS ");
        assert_eq!(u32::from_le_bytes(dds[4..8].try_into().unwrap()), 124);
        assert_eq!(&dds[84..88], b"DX10");
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        // an unsupported format, and garbage in place of the stream
        let mut meta = *TEX0_META;
        meta[4] = 0x07;
        meta[5] = 0x00; // format 0 (unknown)
        assert!(reconstruct_dds(&meta, TEX0, true).is_err());
        assert!(reconstruct_dds(TEX0_META, b"not gdeflate", true).is_err());
        // a short uncompressed surface gives a short file, never a crash
        assert!(reconstruct_dds(TEX0_META, &[0u8; 10], false).is_ok());
    }
}
