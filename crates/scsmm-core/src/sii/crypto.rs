//! Decrypts ScsC SII files (profile.sii etc).
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

/// Decode a profile.sii payload to text, decrypting when needed.
/// `strict` refuses invalid UTF-8 instead of replacing it - the writer uses
/// that so it never round-trips a lossy copy of the user's profile.
pub fn sii_bytes_to_text(data: &[u8], strict: bool) -> Result<String> {
    let plain = if is_scsc(data) {
        decrypt_scsc(data)?
    } else {
        data.to_vec()
    };
    if strict {
        String::from_utf8(plain).map_err(|_| format_err("profile is not valid UTF-8"))
    } else {
        Ok(String::from_utf8_lossy(&plain).into_owned())
    }
}

#[cfg(test)]
mod tests {
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
    fn rejects_non_scsc() {
        assert!(decrypt_scsc(b"SiiNunit").is_err());
        assert!(decrypt_scsc(b"ScsC").is_err());
    }
}
