//! Decodes the two wrapped SII layouts: ScsC (profile.sii etc) and 3nK
//! (definition files of mods whose author scrambled them).
//!
//! SCS stores the profile with AES-256-CBC plus zlib. The key is publicly
//! known and ships in every game install.
//!
//! ```text
//! 0x00  4   magic "ScsC"
//! 0x04  32  HMAC (ignored on read)
//! 0x24  16  AES IV
//! 0x34  4   declared plaintext size (u32 LE)
//! 0x38  N   ciphertext (AES-CBC, zero padded)
//! ```
//!
//! There is no encrypt counterpart on purpose: the game verifies the HMAC
//! of an encrypted profile and that algorithm is not public, so the writer
//! always emits plaintext (which the game reads without the check).
//!
//! 3nK is no encryption, only a byte-wise XOR with a running key:
//!
//! ```text
//! 0x00  4   magic "3nK\x01"
//! 0x04  1   unused
//! 0x05  1   seed
//! 0x06  N   payload; byte i is XORed with key(seed + i)
//! ```

use std::io::Read;

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};

use crate::{format_err, Result};

pub const SCSC_MAGIC: &[u8; 4] = b"ScsC";
const HEADER_SIZE: usize = 0x38;
const SCS_KEY: [u8; 32] = [
    0x2A, 0x5F, 0xCB, 0x17, 0x91, 0xD2, 0x2F, 0xB6, 0x02, 0x45, 0xB3, 0xD8, 0x36, 0x9E, 0xD0, 0xB2,
    0xC2, 0x73, 0x71, 0x56, 0x3F, 0xBF, 0x1F, 0x3C, 0x9E, 0xDF, 0x6B, 0x11, 0x82, 0x5A, 0x5D, 0x0A,
];

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

pub fn is_scsc(data: &[u8]) -> bool {
    data.starts_with(SCSC_MAGIC)
}

pub fn decrypt_scsc(data: &[u8]) -> Result<Vec<u8>> {
    if !is_scsc(data) {
        return Err(format_err("not a ScsC blob"));
    }
    if data.len() < HEADER_SIZE {
        return Err(format_err("truncated ScsC header"));
    }
    let iv: [u8; 16] = data[0x24..0x34].try_into().expect("16-byte slice");
    let mut buf = data[HEADER_SIZE..].to_vec();
    // CBC needs whole blocks; a trailing partial block cannot be ciphertext
    let whole = buf.len() - buf.len() % 16;
    buf.truncate(whole);
    Aes256CbcDec::new(&SCS_KEY.into(), &iv.into())
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .map_err(|_| format_err("ScsC decrypt failed"))?;

    // the zero padding after the zlib stream is simply left unread
    let mut plain = Vec::new();
    flate2::read::ZlibDecoder::new(&buf[..])
        .read_to_end(&mut plain)
        .map_err(|e| format_err(format!("ScsC inflate failed: {e}")))?;
    Ok(plain)
}

pub const THREE_NK_MAGIC: &[u8; 4] = b"3nK\x01";
const THREE_NK_HEADER: usize = 6;

pub fn is_3nk(data: &[u8]) -> bool {
    data.starts_with(THREE_NK_MAGIC)
}

/// Key byte of the 3nK scrambling for position `seed`.
fn key_3nk(seed: u8) -> u8 {
    (((seed << 2) ^ !seed) << 3) ^ seed
}

/// Undo the 3nK scrambling. The result is whatever was wrapped - plain
/// text in practice.
pub fn decode_3nk(data: &[u8]) -> Result<Vec<u8>> {
    if !is_3nk(data) {
        return Err(format_err("not a 3nK blob"));
    }
    if data.len() < THREE_NK_HEADER {
        return Err(format_err("truncated 3nK header"));
    }
    let seed = data[5];
    Ok(data[THREE_NK_HEADER..]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key_3nk(seed.wrapping_add(i as u8)))
        .collect())
}

/// The bytes of a SII file with every known wrapper taken off: 3nK
/// scrambling and ScsC encryption, in whatever order they were applied.
/// Anything else comes back unchanged.
pub fn unwrap_sii(data: &[u8]) -> Result<Vec<u8>> {
    let mut plain = data.to_vec();
    // a wrapper inside a wrapper is possible, an endless chain is not
    for _ in 0..4 {
        plain = if is_3nk(&plain) {
            decode_3nk(&plain)?
        } else if is_scsc(&plain) {
            decrypt_scsc(&plain)?
        } else {
            break;
        };
    }
    Ok(plain)
}

/// Decode a SII payload to text, unwrapping it when needed.
/// `strict` refuses invalid UTF-8 instead of replacing it - the writer uses
/// that so it never round-trips a lossy copy of the user's profile.
pub fn sii_bytes_to_text(data: &[u8], strict: bool) -> Result<String> {
    let plain = unwrap_sii(data)?;
    if strict {
        String::from_utf8(plain).map_err(|_| format_err("profile is not valid UTF-8"))
    } else {
        Ok(String::from_utf8_lossy(&plain).into_owned())
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::{key_3nk, THREE_NK_MAGIC};

    /// Wrap `plain` the way a 3nK-scrambled definition file is stored.
    pub fn scramble_3nk(plain: &[u8], seed: u8) -> Vec<u8> {
        let mut out = THREE_NK_MAGIC.to_vec();
        out.extend_from_slice(&[0, seed]);
        out.extend(plain.iter().enumerate().map(|(i, b)| b ^ key_3nk(seed.wrapping_add(i as u8))));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::scramble_3nk;
    use super::*;
    use aes::cipher::BlockEncryptMut;
    use std::io::Write;

    fn encrypt_for_test(plain: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        z.write_all(plain).unwrap();
        let mut body = z.finish().unwrap();
        body.resize(body.len().div_ceil(16) * 16, 0);
        let iv = [7u8; 16];
        let len = body.len();
        cbc::Encryptor::<aes::Aes256>::new(&SCS_KEY.into(), &iv.into())
            .encrypt_padded_mut::<NoPadding>(&mut body, len)
            .unwrap();
        let mut out = SCSC_MAGIC.to_vec();
        out.extend_from_slice(&[0u8; 32]);
        out.extend_from_slice(&iv);
        out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn round_trip() {
        let plain = b"SiiNunit\n{\nuser_profile : _nameless.1 {\n active_mods: 0\n}\n}\n";
        let blob = encrypt_for_test(plain);
        assert!(is_scsc(&blob));
        assert_eq!(decrypt_scsc(&blob).unwrap(), plain);
        assert_eq!(sii_bytes_to_text(&blob, true).unwrap().as_bytes(), plain);
    }

    #[test]
    fn decodes_3nk() {
        // the first bytes of a real scrambled definition file
        let real = [0x33, 0x6e, 0x4b, 0x01, 0x00, 0x10, 0x3b, 0x28, 0x53, 0x5d, 0xb9, 0x8b, 0xf7, 0xc3];
        assert_eq!(decode_3nk(&real).unwrap(), b"SiiNunit");

        // the key wraps around after 256 bytes
        let plain: Vec<u8> = (0..700u32).map(|i| (i * 7) as u8).collect();
        for seed in [0u8, 0x10, 0xff] {
            let blob = scramble_3nk(&plain, seed);
            assert!(is_3nk(&blob));
            assert_eq!(decode_3nk(&blob).unwrap(), plain);
        }
        assert!(decode_3nk(b"SiiNunit").is_err());
        assert!(decode_3nk(b"3nK\x01").is_err());
    }

    #[test]
    fn unwraps_nested_and_leaves_plain_alone() {
        let plain = b"SiiNunit\n{\n}\n";
        assert_eq!(unwrap_sii(plain).unwrap(), plain);
        let scrambled = scramble_3nk(plain, 0x42);
        assert_eq!(sii_bytes_to_text(&scrambled, true).unwrap().as_bytes(), plain);
        let both = scramble_3nk(&encrypt_for_test(plain), 3);
        assert_eq!(unwrap_sii(&both).unwrap(), plain);
    }

    #[test]
    fn rejects_non_scsc() {
        assert!(decrypt_scsc(b"SiiNunit").is_err());
        assert!(decrypt_scsc(b"ScsC").is_err());
    }
}
