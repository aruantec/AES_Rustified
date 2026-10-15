//! Xenia-canary profile provisioning — mirrors Xenia's
//! `ProfileManager::CreateProfile`/`EncryptAccountFile` so a profile
//! can be created without ever opening Xenia's ImGui dialogs (which
//! can't be reached through the hidden capture window).
//!
//! A profile is a plain directory package —
//! `content/<XUID>/FFFE07D1/00010000/<XUID>/Account` — where `Account`
//! is the `X_XAMACCOUNTINFO` struct (0x17C, big-endian) plus an
//! 8-byte confounder, RC4-encrypted under an HMAC-SHA1-derived key,
//! prefixed with the HMAC digest of the plaintext region.

use std::path::{Path, PathBuf};

/// XeKeys key index 0x19 (xam account file), retail flavor.
const XEKEY_19: [u8; 0x10] = [
    0xE1, 0xBC, 0x15, 0x9C, 0x73, 0xB1, 0xEA, 0xE9, 0xAB, 0x31, 0x70, 0xF3, 0xAD, 0x47, 0xEB,
    0xF3,
];

/// sizeof(X_XAMACCOUNTINFO) — see xenia/kernel/xam/xam.h.
const ACCOUNT_INFO_LEN: usize = 0x17C;
/// Account file: digest (0x10) + confounder (8) + account info.
const ACCOUNT_FILE_LEN: usize = 0x10 + 8 + ACCOUNT_INFO_LEN;

/// `<storage root>/content` — Xenia's default when `content_root` is
/// unset (storage_root/content, portable or exe-dir rooted).
pub fn content_dir(root: &Path) -> PathBuf {
    root.join("content")
}

/// Profile directory name must match `[0-9A-F]{16}` and contain a
/// `FFFE07D1/00010000/<xuid>` package (Xenia's FindProfiles check).
pub fn find_xuids(root: &Path) -> Vec<u64> {
    let content = content_dir(root);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&content) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.len() != 16
            || !name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        {
            continue;
        }
        let pkg = e
            .path()
            .join("FFFE07D1")
            .join("00010000")
            .join(name);
        if !pkg.exists() {
            continue;
        }
        if let Ok(xuid) = u64::from_str_radix(name, 16) {
            if xuid != 0 {
                out.push(xuid);
            }
        }
    }
    out.sort_unstable();
    out
}

/// Xenia's IsGamertagValid — `^[A-Za-z][A-Za-z0-9]*( [A-Za-z0-9]+)*$`,
/// 1..=15 chars.
pub fn is_valid_gamertag(tag: &str) -> bool {
    let tag = tag.trim_end_matches('\0');
    if tag.is_empty() || tag.len() > 15 || !tag.is_ascii() {
        return false;
    }
    let mut it = tag.split(' ');
    let first = match it.next() {
        Some(w) => w,
        None => return false,
    };
    // First word must start with a letter, then alnum; following
    // words must be non-empty alnum (no leading/trailing/double
    // spaces — the regex allows no empty word).
    let first_ok = first
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && first.chars().all(|c| c.is_ascii_alphanumeric());
    first_ok
        && it.all(|w| {
            !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric())
        })
}

/// Xenia's GenerateXuid: 0xE03 << 52 | 31 random bits. No rand crate
/// — mix time + pid + a heap pointer for entropy.
fn generate_xuid() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let entropy = t
        ^ (std::process::id() as u64) << 32
        ^ (std::ptr::from_ref(&t) as usize as u64).wrapping_mul(0x9E37_79B9);
    (0xE03u64 << 52) | (entropy & 0x7FFF_FFFF)
}

/// RC4 keystream (key size is whatever is passed — Xenia uses 0x10
/// of the 0x14-byte HMAC output).
fn rc4(key: &[u8], data: &mut [u8]) {
    let mut s = [0u8; 256];
    for (i, v) in s.iter_mut().enumerate() {
        *v = i as u8;
    }
    let mut j = 0usize;
    for i in 0..256 {
        j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
        s.swap(i, j);
    }
    let (mut x, mut y) = (0usize, 0usize);
    for b in data.iter_mut() {
        x = (x + 1) % 256;
        y = (y + s[x] as usize) % 256;
        s.swap(x, y);
        *b ^= s[(s[x] as usize + s[y] as usize) % 256];
    }
}

/// Minimal SHA-1 (single-shot) — enough for HMAC-SHA1.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(64 + data.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(data);
    let ih = sha1(&inner);
    let mut outer = Vec::with_capacity(84);
    outer.extend(k.iter().map(|b| b ^ 0x5C));
    outer.extend_from_slice(&ih);
    sha1(&outer)
}

/// Build the encrypted `Account` file — the exact transform in
/// Xenia's EncryptAccountFile (retail xekey 0x19, 0xFD confounder).
fn encrypt_account(gamertag: &str) -> Vec<u8> {
    let mut out = vec![0u8; ACCOUNT_FILE_LEN];
    // X_XAMACCOUNTINFO at +0x18 — all fields zero except the
    // gamertag: char16[0x10] stored big-endian, NUL-terminated.
    let acct = &mut out[0x18..0x18 + ACCOUNT_INFO_LEN];
    for (i, c) in gamertag.encode_utf16().take(0x10 - 1).enumerate() {
        acct[8 + i * 2] = (c >> 8) as u8;
        acct[8 + i * 2 + 1] = c as u8;
    }
    out[0x10..0x18].fill(0xFD); // confounder
    let hash = hmac_sha1(&XEKEY_19, &out[0x10..]);
    out[..0x10].copy_from_slice(&hash[..0x10]);
    let rc4_key = hmac_sha1(&XEKEY_19, &out[..0x10]);
    rc4(&rc4_key[..0x10], &mut out[0x10..]);
    out
}

/// Create a Xenia profile under `<root>/content` and return its
/// XUID. Mirrors `CreateProfile(gamertag, …)` — the package is a
/// directory, so this is just mkdir + Account write.
pub fn create(root: &Path, gamertag: &str) -> Result<u64, String> {
    if !is_valid_gamertag(gamertag) {
        return Err("invalid gamertag".into());
    }
    let xuid = generate_xuid();
    let hex = format!("{xuid:016X}");
    let pkg = content_dir(root)
        .join(&hex)
        .join("FFFE07D1")
        .join("00010000")
        .join(&hex);
    std::fs::create_dir_all(&pkg)
        .map_err(|e| format!("create profile dir: {e}"))?;
    std::fs::write(pkg.join("Account"), encrypt_account(gamertag))
        .map_err(|e| format!("write Account: {e}"))?;
    Ok(xuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamertag_rules() {
        assert!(is_valid_gamertag("Player"));
        assert!(is_valid_gamertag("Player One 2"));
        assert!(!is_valid_gamertag(""));
        assert!(!is_valid_gamertag("2Player"));
        assert!(!is_valid_gamertag("Player  Two")); // double space → empty word
        assert!(!is_valid_gamertag(" Player"));
        assert!(!is_valid_gamertag("Player "));
        assert!(!is_valid_gamertag("ThisNameIsTooLong"));
        assert!(!is_valid_gamertag("Bad-Name"));
    }

    #[test]
    fn sha1_known_vector() {
        // SHA1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
        let d = sha1(b"abc");
        let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn account_file_roundtrips() {
        // Encrypt then run Xenia's DecryptAccountFile logic — verify
        // the digest check passes and the gamertag survives.
        let buf = encrypt_account("Player One");
        assert_eq!(buf.len(), ACCOUNT_FILE_LEN);
        let rc4_key = hmac_sha1(&XEKEY_19, &buf[..0x10]);
        let mut dec = buf[0x10..].to_vec();
        rc4(&rc4_key[..0x10], &mut dec);
        let hash = hmac_sha1(&XEKEY_19, &dec);
        assert_eq!(&buf[..0x10], &hash[..0x10], "digest must verify");
        assert_eq!(&dec[..8], &[0xFD; 8], "confounder");
        // gamertag at acct offset 8; acct starts at dec[8] → dec[16..]
        let tag_utf16: Vec<u16> = "Player One".encode_utf16().collect();
        for (i, c) in tag_utf16.iter().enumerate() {
            let got = u16::from_be_bytes([dec[16 + i * 2], dec[16 + i * 2 + 1]]);
            assert_eq!(got, *c);
        }
        // NUL terminator right after "Player One" (10 chars = 20 B)
        assert_eq!(u16::from_be_bytes([dec[8 + 8 + 20], dec[8 + 8 + 21]]), 0);
    }
}
