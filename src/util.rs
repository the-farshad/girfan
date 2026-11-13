//! Small helpers: randomness, hex, hashing, encryption, private files, and text formatting.

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system's random source failed");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The first N hex digits of the SHA-256 of TEXT (for stable ids).
pub fn digest(text: &str, n: usize) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(text.as_bytes()))[..n].to_string()
}

/// AES-256-GCM with a random nonce: nonce followed by the ciphertext.
pub fn seal(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let cipher = Aes256Gcm::new_from_slice(key).expect("a 32-byte key");
    let nonce: [u8; 12] = random();
    let sealed = cipher
        .encrypt(
            (&nonce[..]).try_into().expect("12 bytes"),
            Payload { msg: plain, aad },
        )
        .expect("encryption failed");
    [nonce.to_vec(), sealed].concat()
}

/// The plain text of what seal() made, or None when the key is wrong or the data damaged.
pub fn unseal(key: &[u8; 32], aad: &[u8], blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 12 {
        return None;
    }
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    cipher
        .decrypt(
            (&blob[..12]).try_into().ok()?,
            Payload {
                msg: &blob[12..],
                aad,
            },
        )
        .ok()
}

/// Writes DATA to PATH readable by you only, replacing the old file in one step.
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().expect("a file in a folder");
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

/// Equality that takes as long whatever the inputs (for the web page's access code).
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// $1,234.56, -$12.00, or with SIGN +$5.00; "-" for none.
pub fn money(x: Option<f64>, sign: bool) -> String {
    let Some(x) = x else { return "-".into() };
    let cents = (x.abs() * 100.0).round() as u64;
    let whole = (cents / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let s = format!("${grouped}.{:02}", cents % 100);
    if x < 0.0 && cents > 0 {
        format!("-{s}")
    } else if sign && cents > 0 {
        format!("+{s}")
    } else {
        s
    }
}

/// TEXT cut to W characters, with … when cut.
pub fn cut(text: &str, w: usize) -> String {
    if text.chars().count() <= w {
        text.to_string()
    } else {
        text.chars().take(w.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_and_hex() {
        assert_eq!(money(Some(1234567.891), false), "$1,234,567.89");
        assert_eq!(money(Some(-12.0), false), "-$12.00");
        assert_eq!(money(Some(5.0), true), "+$5.00");
        assert_eq!(money(Some(-0.001), true), "$0.00");
        assert_eq!(unhex(&hex(&[0, 255, 16])), Some(vec![0, 255, 16]));
    }

    #[test]
    fn sealing() {
        let key: [u8; 32] = random();
        let blob = seal(&key, b"aad", b"hello");
        assert_eq!(unseal(&key, b"aad", &blob).as_deref(), Some(&b"hello"[..]));
        assert!(unseal(&key, b"other", &blob).is_none());
        assert!(unseal(&random(), b"aad", &blob).is_none());
    }
}
