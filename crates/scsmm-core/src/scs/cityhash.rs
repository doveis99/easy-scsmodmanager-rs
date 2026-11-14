//! CityHash64 - the hash HashFS uses to address entries by path.
//!
//! Ported from the Python original, which follows TruckLib.HashFs (sk-zk),
//! itself a port of Google's CityHash v1.0.

const K0: u64 = 0xC3A5_C85C_97CB_3127;
const K1: u64 = 0xB492_B66F_BE98_F273;
const K2: u64 = 0x9AE1_6A3B_2F90_404F;
const K3: u64 = 0xC949_D7C7_509E_6557;
const KMUL: u64 = 0x9DDF_EA08_EB38_2D69;

#[inline]
fn fetch64(s: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(s[pos..pos + 8].try_into().expect("8 bytes"))
}

#[inline]
fn fetch32(s: &[u8], pos: usize) -> u64 {
    u32::from_le_bytes(s[pos..pos + 4].try_into().expect("4 bytes")) as u64
}

#[inline]
fn rotate(val: u64, shift: u32) -> u64 {
    val.rotate_right(shift)
}

#[inline]
fn shift_mix(val: u64) -> u64 {
    val ^ (val >> 47)
}

#[inline]
fn hash_len16(u: u64, v: u64) -> u64 {
    let mut a = (u ^ v).wrapping_mul(KMUL);
    a ^= a >> 47;
    let mut b = (v ^ a).wrapping_mul(KMUL);
    b ^= b >> 47;
    b.wrapping_mul(KMUL)
}

fn hash_len0_to16(s: &[u8]) -> u64 {
    let len = s.len();
    let l = len as u64;
    if len > 8 {
        let a = fetch64(s, 0);
        let b = fetch64(s, len - 8);
        return hash_len16(a, rotate(b.wrapping_add(l), len as u32)) ^ b;
    }
    if len >= 4 {
        let a = fetch32(s, 0);
        return hash_len16(l.wrapping_add(a << 3), fetch32(s, len - 4));
    }
    if len > 0 {
        let a = s[0] as u64;
        let b = s[len >> 1] as u64;
        let c = s[len - 1] as u64;
        let y = a.wrapping_add(b << 8);
        let z = l.wrapping_add(c << 2);
        return shift_mix(y.wrapping_mul(K2) ^ z.wrapping_mul(K3)).wrapping_mul(K2);
    }
    K2
}

fn hash_len17_to32(s: &[u8]) -> u64 {
    let len = s.len();
    let a = fetch64(s, 0).wrapping_mul(K1);
    let b = fetch64(s, 8);
    let c = fetch64(s, len - 8).wrapping_mul(K2);
    let d = fetch64(s, len - 16).wrapping_mul(K0);
    hash_len16(
        rotate(a.wrapping_sub(b), 43)
            .wrapping_add(rotate(c, 30))
            .wrapping_add(d),
        a.wrapping_add(rotate(b ^ K3, 20))
            .wrapping_sub(c)
            .wrapping_add(len as u64),
    )
}

fn weak_hash_len32_with_seeds(s: &[u8], pos: usize, a: u64, b: u64) -> (u64, u64) {
    let w = fetch64(s, pos);
    let x = fetch64(s, pos + 8);
    let y = fetch64(s, pos + 16);
    let z = fetch64(s, pos + 24);
    let mut a = a.wrapping_add(w);
    let mut b = rotate(b.wrapping_add(a).wrapping_add(z), 21);
    let c = a;
    a = a.wrapping_add(x);
    a = a.wrapping_add(y);
    b = b.wrapping_add(rotate(a, 44));
    (a.wrapping_add(z), b.wrapping_add(c))
}

fn hash_len33_to64(s: &[u8]) -> u64 {
    let len = s.len();
    let mut z = fetch64(s, 24);
    let mut a = fetch64(s, 0).wrapping_add(
        (len as u64)
            .wrapping_add(fetch64(s, len - 16))
            .wrapping_mul(K0),
    );
    let mut b = rotate(a.wrapping_add(z), 52);
    let mut c = rotate(a, 37);
    a = a.wrapping_add(fetch64(s, 8));
    c = c.wrapping_add(rotate(a, 7));
    a = a.wrapping_add(fetch64(s, 16));
    let vf = a.wrapping_add(z);
    let vs = b.wrapping_add(rotate(a, 31)).wrapping_add(c);
    a = fetch64(s, 16).wrapping_add(fetch64(s, len - 32));
    z = fetch64(s, len - 8);
    b = rotate(a.wrapping_add(z), 52);
    c = rotate(a, 37);
    a = a.wrapping_add(fetch64(s, len - 24));
    c = c.wrapping_add(rotate(a, 7));
    a = a.wrapping_add(fetch64(s, len - 16));
    let wf = a.wrapping_add(z);
    let ws = b.wrapping_add(rotate(a, 31)).wrapping_add(c);
    let r = shift_mix(
        vf.wrapping_add(ws)
            .wrapping_mul(K2)
            .wrapping_add(wf.wrapping_add(vs).wrapping_mul(K0)),
    );
    shift_mix(r.wrapping_mul(K0).wrapping_add(vs)).wrapping_mul(K2)
}

/// CityHash64 of a byte string.
pub fn city_hash64(s: &[u8]) -> u64 {
    let len = s.len();
    if len <= 16 {
        return hash_len0_to16(s);
    }
    if len <= 32 {
        return hash_len17_to32(s);
    }
    if len <= 64 {
        return hash_len33_to64(s);
    }

    let l = len as u64;
    let mut x = fetch64(s, len - 40);
    let mut y = fetch64(s, len - 16).wrapping_add(fetch64(s, len - 56));
    let mut z = hash_len16(
        fetch64(s, len - 48).wrapping_add(l),
        fetch64(s, len - 24),
    );
    let mut v = weak_hash_len32_with_seeds(s, len - 64, l, z);
    let mut w = weak_hash_len32_with_seeds(s, len - 32, y.wrapping_add(K1), x);
    x = x.wrapping_mul(K1).wrapping_add(fetch64(s, 0));

    let mut pos = 0;
    let mut remaining = (len - 1) & !63;
    loop {
        x = rotate(
            x.wrapping_add(y)
                .wrapping_add(v.0)
                .wrapping_add(fetch64(s, pos + 8)),
            37,
        )
        .wrapping_mul(K1);
        y = rotate(
            y.wrapping_add(v.1).wrapping_add(fetch64(s, pos + 48)),
            42,
        )
        .wrapping_mul(K1);
        x ^= w.1;
        y = y.wrapping_add(v.0).wrapping_add(fetch64(s, pos + 40));
        z = rotate(z.wrapping_add(w.0), 33).wrapping_mul(K1);
        v = weak_hash_len32_with_seeds(s, pos, v.1.wrapping_mul(K1), x.wrapping_add(w.0));
        w = weak_hash_len32_with_seeds(
            s,
            pos + 32,
            z.wrapping_add(w.1),
            y.wrapping_add(fetch64(s, pos + 16)),
        );
        std::mem::swap(&mut z, &mut x);
        pos += 64;
        remaining -= 64;
        if remaining == 0 {
            break;
        }
    }

    hash_len16(
        hash_len16(v.0, w.0)
            .wrapping_add(shift_mix(y).wrapping_mul(K1))
            .wrapping_add(z),
        hash_len16(v.1, w.1).wrapping_add(x),
    )
}

/// Hash a HashFS entry path the way the game does: drop one leading slash,
/// prepend a non-zero salt as a decimal string, CityHash64 the UTF-8 bytes.
pub fn hash_path(path: &str, salt: u16) -> u64 {
    let path = path.strip_prefix('/').unwrap_or(path);
    if salt != 0 {
        city_hash64(format!("{salt}{path}").as_bytes())
    } else {
        city_hash64(path.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values generated with the original app's Python
    // implementation of the hash, one per length class.
    include!("cityhash_vectors.in");

    #[test]
    fn matches_reference_vectors() {
        for (input, expected) in VECTORS {
            assert_eq!(city_hash64(input.as_bytes()), *expected, "input {input:?}");
        }
    }

    #[test]
    fn empty_root_path_and_salt() {
        assert_eq!(hash_path("", 0), K2);
        assert_eq!(hash_path("/", 0), K2);
        assert_eq!(hash_path("/manifest.sii", 0), hash_path("manifest.sii", 0));
        assert_eq!(hash_path("def", 7), city_hash64(b"7def"));
    }
}
