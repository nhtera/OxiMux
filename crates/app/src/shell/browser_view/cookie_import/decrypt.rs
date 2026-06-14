//! Chromium cookie-value decryption (macOS).
//!
//! Chromium stores cookie values AES-128-CBC-encrypted under a key derived from
//! a per-browser secret in the macOS Keychain (`<Browser> Safe Storage`). The
//! derivation is fixed and public: `PBKDF2-HMAC-SHA1(secret, "saltysalt", 1003)`
//! → 16-byte key; IV is 16 spaces. Encrypted values carry a `v10`/`v11` prefix;
//! Chromium 127+ additionally prepends a 32-byte per-host HMAC to the plaintext,
//! which we strip when the leading bytes look binary.
//!
//! Reading the Keychain item triggers a one-time macOS access prompt — this is
//! always user-initiated (the import menu pick) and is the same prompt the
//! reference cockpits surface. We never log the secret or any cookie value.

use aes::Aes128;
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use hmac::Hmac;
use sha1::Sha1;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

const PBKDF2_ITERATIONS: u32 = 1003;
const PBKDF2_SALT: &[u8] = b"saltysalt";
const AES_IV: [u8; 16] = [b' '; 16];

/// Fetch the browser's Keychain secret and derive its 16-byte AES key, or
/// `None` if the Keychain denied access / the item is missing. Shelling out to
/// `/usr/bin/security` (rather than the Security framework) matches what the
/// reference tools do and keeps the access prompt's wording browser-specific.
pub fn derive_key(service: &str, account: &str) -> Option<[u8; 16]> {
    let secret = keychain_secret(service, account)?;
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2::<Hmac<Sha1>>(&secret, PBKDF2_SALT, PBKDF2_ITERATIONS, &mut key).ok()?;
    Some(key)
}

/// `security find-generic-password -s <service> -a <account> -w` → the raw
/// secret bytes (one line, trailing newline trimmed).
fn keychain_secret(service: &str, account: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut bytes = out.stdout;
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    (!bytes.is_empty()).then_some(bytes)
}

/// Decrypt one `encrypted_value` blob to its UTF-8 cookie value. Returns `None`
/// when the blob lacks the `v10`/`v11` prefix (caller should use the row's
/// plaintext `value` column instead) or decryption fails.
pub fn decrypt_value(encrypted: &[u8], key: &[u8; 16]) -> Option<String> {
    if encrypted.len() <= 3 {
        return None;
    }
    let prefix = &encrypted[0..3];
    if prefix != b"v10" && prefix != b"v11" {
        return None;
    }
    let ciphertext = &encrypted[3..];
    let mut buf = ciphertext.to_vec();
    let plain = Aes128CbcDec::new(key.into(), &AES_IV.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .ok()?;
    Some(decode_plaintext(plain))
}

/// Decode decrypted bytes, stripping the Chromium-127+ 32-byte per-host HMAC
/// prefix when the leading block looks binary (≥8 non-printable bytes).
fn decode_plaintext(plain: &[u8]) -> String {
    let body = if plain.len() > 32 {
        let non_printable = plain[..32].iter().filter(|&&b| !(0x20..=0x7e).contains(&b)).count();
        if non_printable >= 8 { &plain[32..] } else { plain }
    } else {
        plain
    };
    String::from_utf8_lossy(body).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncryptMut, block_padding::Pkcs7 as Pkcs7Enc};

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    /// Round-trip: encrypt like Chromium (v10 prefix, space IV), decrypt back.
    #[test]
    fn decrypts_v10_round_trip() {
        let key = [7u8; 16];
        let value = b"session=abc123; logged_in";
        let mut buf = vec![0u8; value.len() + 16];
        buf[..value.len()].copy_from_slice(value);
        let ct = Aes128CbcEnc::new(&key.into(), &AES_IV.into())
            .encrypt_padded_mut::<Pkcs7Enc>(&mut buf, value.len())
            .unwrap()
            .to_vec();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&ct);

        let out = decrypt_value(&blob, &key).unwrap();
        assert_eq!(out, "session=abc123; logged_in");
    }

    #[test]
    fn strips_127_hmac_prefix() {
        // 32 binary bytes (the HMAC) followed by the real value.
        let key = [3u8; 16];
        let mut plaintext = vec![0u8; 32];
        plaintext.extend_from_slice(b"real_value");
        let mut buf = vec![0u8; plaintext.len() + 16];
        buf[..plaintext.len()].copy_from_slice(&plaintext);
        let ct = Aes128CbcEnc::new(&key.into(), &AES_IV.into())
            .encrypt_padded_mut::<Pkcs7Enc>(&mut buf, plaintext.len())
            .unwrap()
            .to_vec();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&ct);

        assert_eq!(decrypt_value(&blob, &key).unwrap(), "real_value");
    }

    #[test]
    fn returns_none_for_unprefixed_blob() {
        assert!(decrypt_value(b"plaintextcookie", &[0u8; 16]).is_none());
        assert!(decrypt_value(b"", &[0u8; 16]).is_none());
    }
}
