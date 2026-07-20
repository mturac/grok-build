//! RFC 8291 Web Push message encryption ("aes128gcm" content-encoding, built
//! on the single-record RFC 8188 HTTP Encrypted Content-Encoding scheme).
//!
//! A push message is encrypted to the subscriber's ECDH public key
//! (`p256dh`) using a fresh ephemeral keypair per message, combined with the
//! subscription's `auth` secret via HKDF-SHA256 to derive the AES-128-GCM
//! content-encryption key and nonce. The resulting body is self-describing
//! (RFC 8188 header carries the salt and the sender's ephemeral public key),
//! so the push service and user agent need no out-of-band key state beyond
//! what the subscription already provided.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, Key, KeyInit, Nonce};
use anyhow::{Context, Result, bail};
use hkdf::Hkdf;
use p256::PublicKey;
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::rand_core::OsRng;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use sha2::Sha256;

/// RFC 8188 record size we advertise in the header. The whole message is a
/// single record, so this only needs to be >= the padded plaintext length;
/// 4096 comfortably covers any push payload (push services cap the encrypted
/// body around 4KB anyway).
const RECORD_SIZE: u32 = 4096;

/// Length of an uncompressed SEC1 P-256 point (0x04 || X(32) || Y(32)).
const P256_POINT_LEN: usize = 65;
/// Web Push subscription `auth` secret length (RFC 8291).
const AUTH_SECRET_LEN: usize = 16;
/// AES-128-GCM authentication tag length appended to the ciphertext.
const AES_GCM_TAG_LEN: usize = 16;

/// Length of the RFC 8188 aes128gcm header: salt(16) || rs(4) || idlen(1) || keyid(65).
const HEADER_LEN: usize = 16 + 4 + 1 + P256_POINT_LEN;

/// The RFC 8188 last-and-only record delimiter appended before encryption.
const RECORD_DELIMITER: u8 = 0x02;

/// Derive the per-message HKDF pseudo-random key, CEK, and nonce shared by
/// both encrypt and decrypt: `IKM = HKDF-SHA256(salt=auth, ikm=ecdh_secret,
/// info=key_info)`, then `CEK`/`NONCE` are expanded from `IKM` under `salt`.
fn derive_cek_and_nonce(
    ecdh_secret: &[u8],
    auth: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12])> {
    let mut key_info = Vec::with_capacity(14 + ua_public.len() + as_public.len());
    key_info.extend_from_slice(b"WebPush: info\x00");
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public);

    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), ecdh_secret)
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("HKDF expand failed deriving IKM"))?;

    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    hk.expand(b"Content-Encoding: aes128gcm\x00", &mut cek)
        .map_err(|_| anyhow::anyhow!("HKDF expand failed deriving CEK"))?;
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: nonce\x00", &mut nonce)
        .map_err(|_| anyhow::anyhow!("HKDF expand failed deriving NONCE"))?;

    Ok((cek, nonce))
}

/// Encrypt `plaintext` for a Web Push subscription using RFC 8291 aes128gcm.
/// `ua_public` = the subscription's p256dh key (65-byte uncompressed P-256 point).
/// `auth` = the subscription's 16-byte auth secret. Returns the full aes128gcm
/// body (ready to POST with `Content-Encoding: aes128gcm`).
pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth: &[u8]) -> Result<Vec<u8>> {
    // The uncompressed 65-byte point is used verbatim in `key_info`; a
    // compressed (33-byte) key would parse but derive a key the browser can't
    // reproduce, so require the exact form for interop.
    if ua_public.len() != P256_POINT_LEN {
        bail!(
            "subscription p256dh must be a 65-byte uncompressed P-256 point, got {}",
            ua_public.len()
        );
    }
    if auth.len() != AUTH_SECRET_LEN {
        bail!(
            "subscription auth secret must be {AUTH_SECRET_LEN} bytes, got {}",
            auth.len()
        );
    }
    // Single-record encoding: the whole payload + delimiter + GCM tag must fit
    // in the advertised record size.
    if plaintext.len() + 1 + AES_GCM_TAG_LEN > RECORD_SIZE as usize {
        bail!("push payload too large for a single aes128gcm record");
    }
    let ua_public_key =
        PublicKey::from_sec1_bytes(ua_public).context("parsing subscription p256dh key")?;

    let as_secret = EphemeralSecret::random(&mut OsRng);
    let as_public = as_secret.public_key().to_encoded_point(false);
    let as_public_bytes = as_public.as_bytes();
    if as_public_bytes.len() != P256_POINT_LEN {
        bail!(
            "unexpected ephemeral public key length: {}",
            as_public_bytes.len()
        );
    }

    let ecdh_secret = as_secret.diffie_hellman(&ua_public_key);

    let salt: [u8; 16] = rand::random();
    let (cek, nonce) = derive_cek_and_nonce(
        ecdh_secret.raw_secret_bytes().as_slice(),
        auth,
        ua_public,
        as_public_bytes,
        &salt,
    )?;

    let mut padded = Vec::with_capacity(plaintext.len() + 1);
    padded.extend_from_slice(plaintext);
    padded.push(RECORD_DELIMITER);

    let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), padded.as_ref())
        .map_err(|_| anyhow::anyhow!("AES-128-GCM encryption failed"))?;

    let mut body = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(P256_POINT_LEN as u8);
    body.extend_from_slice(as_public_bytes);
    body.extend_from_slice(&ciphertext);

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::SecretKey;
    use p256::ecdh::diffie_hellman;

    /// Reverses `encrypt`: parses the aes128gcm header to recover the salt
    /// and the sender's ephemeral public key, recomputes the same CEK/NONCE
    /// from the recipient's static secret, and AES-128-GCM decrypts.
    fn decrypt(body: &[u8], ua_secret: &SecretKey, auth: &[u8]) -> Result<Vec<u8>> {
        if body.len() < HEADER_LEN {
            bail!("body shorter than aes128gcm header");
        }
        let salt = &body[0..16];
        let idlen = body[20] as usize;
        assert_eq!(idlen, P256_POINT_LEN, "unexpected idlen in header");
        let as_public_bytes = &body[21..21 + idlen];
        let ciphertext = &body[21 + idlen..];

        let as_public = PublicKey::from_sec1_bytes(as_public_bytes)
            .context("parsing sender ephemeral public key")?;

        let ua_public_bytes = ua_secret.public_key().to_encoded_point(false);
        let ua_public_bytes = ua_public_bytes.as_bytes();

        let shared = diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());

        let (cek, nonce) = derive_cek_and_nonce(
            shared.raw_secret_bytes().as_slice(),
            auth,
            ua_public_bytes,
            as_public_bytes,
            salt,
        )?;

        let cipher = Aes128Gcm::new(Key::<Aes128Gcm>::from_slice(&cek));
        let padded = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext)
            .map_err(|_| anyhow::anyhow!("AES-128-GCM decryption failed"))?;

        let (last, rest) = padded.split_last().context("empty decrypted record")?;
        assert_eq!(*last, RECORD_DELIMITER, "missing last-record delimiter");
        Ok(rest.to_vec())
    }

    fn roundtrip(plaintext: &[u8]) {
        let ua_secret = SecretKey::random(&mut OsRng);
        let ua_public_bytes = ua_secret.public_key().to_encoded_point(false);
        let ua_public_bytes = ua_public_bytes.as_bytes();
        let auth: [u8; 16] = rand::random();

        let body = encrypt(plaintext, ua_public_bytes, &auth).expect("encrypt should succeed");

        assert_eq!(body.len(), HEADER_LEN + plaintext.len() + 1 + 16);
        assert_eq!(body[20], P256_POINT_LEN as u8, "idlen byte at offset 20");
        assert_eq!(
            body[21..21 + P256_POINT_LEN].len(),
            P256_POINT_LEN,
            "keyid at offset 21"
        );

        let recovered = decrypt(&body, &ua_secret, &auth).expect("decrypt should succeed");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        roundtrip(b"When I grow up, I want to be a watermelon");
    }

    #[test]
    fn encrypt_decrypt_roundtrip_short_plaintext() {
        roundtrip(b"{}");
    }
}
