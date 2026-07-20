//! VAPID (Voluntary Application Server Identification, RFC 8292) keypair
//! management for authenticated Web Push. A single P-256 keypair identifies
//! this server to push services (FCM, Mozilla autopush, etc.); each send is
//! authorized with a short-lived ES256 JWT plus the raw public key.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::elliptic_curve::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use p256::elliptic_curve::rand_core::OsRng;
use serde::{Deserialize, Serialize};

/// How long a VAPID JWT stays valid. RFC 8292 recommends staying well under
/// 24h; push services reject anything further out.
const JWT_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// jsonwebtoken 10 selects its crypto backend through a process-wide
/// [`CryptoProvider`](jsonwebtoken::crypto::CryptoProvider). When the workspace
/// feature graph unifies BOTH `rust_crypto` and `aws_lc_rs` onto jsonwebtoken,
/// the automatic "pick from features" path is ambiguous and `get_default()`
/// panics on the first sign. Install the RustCrypto provider exactly once
/// before we sign. `install_default` returns `Err` if a provider is already
/// installed (by us or another component) — either way a provider is present
/// afterwards, which is all signing needs, so the result is ignored. Both
/// built-in providers (`rust_crypto`, `aws_lc_rs`) implement ES256 over a
/// standard PKCS#8 EC key, so whichever ends up installed signs our VAPID JWT
/// correctly; the only observable difference is the backend crate. For a fully
/// deterministic backend, call this once at server startup before any other
/// component can install a provider (wired in the server bootstrap).
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.install_default();
    });
}

/// A P-256 keypair used to sign VAPID authorization headers.
pub struct VapidKeys {
    signing_key: SigningKey,
}

/// On-disk representation of `$GROK_HOME/push/vapid.json` — the private key
/// only, stored as PKCS#8 DER so it round-trips exactly through
/// `jsonwebtoken`'s `EncodingKey::from_ec_der` (the rust_crypto backend
/// expects PKCS#8, not raw SEC1).
#[derive(Serialize, Deserialize)]
struct VapidFile {
    pkcs8_der_b64: String,
}

#[derive(Serialize, Deserialize)]
struct VapidClaims {
    aud: String,
    exp: u64,
    sub: String,
}

impl VapidKeys {
    /// Load the keypair from `$GROK_HOME/push/vapid.json`, generating and
    /// persisting (mode 0600) a fresh one on first use.
    pub fn load_or_generate(grok_home: &Path) -> Result<Self> {
        let path = Self::store_path(grok_home);
        if let Ok(bytes) = fs::read(&path) {
            let file: VapidFile =
                serde_json::from_slice(&bytes).context("parsing vapid.json")?;
            let der = URL_SAFE_NO_PAD
                .decode(file.pkcs8_der_b64)
                .context("decoding stored VAPID key")?;
            let signing_key =
                SigningKey::from_pkcs8_der(&der).context("parsing stored VAPID key")?;
            return Ok(Self { signing_key });
        }

        let keys = Self::generate();
        keys.persist(&path)?;
        Ok(keys)
    }

    /// A fresh keypair that is never written to disk. Test-only: production
    /// code must go through `load_or_generate` so the identity is stable
    /// across restarts (push subscriptions are bound to the public key).
    pub fn generate_for_test() -> Self {
        Self::generate()
    }

    fn generate() -> Self {
        Self { signing_key: SigningKey::random(&mut OsRng) }
    }

    fn store_path(grok_home: &Path) -> PathBuf {
        grok_home.join("push").join("vapid.json")
    }

    fn persist(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("creating push dir")?;
        }
        let der = self
            .signing_key
            .to_pkcs8_der()
            .context("encoding VAPID key as pkcs8")?;
        let file = VapidFile { pkcs8_der_b64: URL_SAFE_NO_PAD.encode(der.as_bytes()) };
        fs::write(path, serde_json::to_vec_pretty(&file)?).context("writing vapid.json")?;
        Self::restrict_permissions(path)?;
        Ok(())
    }

    #[cfg(unix)]
    fn restrict_permissions(path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms).context("restricting vapid.json permissions")
    }

    #[cfg(not(unix))]
    fn restrict_permissions(_path: &Path) -> Result<()> {
        // No POSIX mode bits off-Unix; the file still lives under $GROK_HOME.
        Ok(())
    }

    /// URL-safe base64 (no padding) of the uncompressed SEC1 public key
    /// point — this is what the PWA passes as `applicationServerKey`.
    pub fn public_key_b64(&self) -> String {
        let verifying_key: &VerifyingKey = self.signing_key.verifying_key();
        let point = verifying_key.to_encoded_point(false);
        URL_SAFE_NO_PAD.encode(point.as_bytes())
    }

    /// Build the `Authorization: vapid t=<jwt>, k=<pub>` header value for a
    /// push request. `audience` is the endpoint's origin (scheme+host, no
    /// path); `subject` is a `mailto:` or `https:` contact URI as required
    /// by RFC 8292.
    pub fn sign_auth_header(&self, audience: &str, subject: &str) -> Result<String> {
        ensure_crypto_provider();
        let der = self
            .signing_key
            .to_pkcs8_der()
            .context("encoding VAPID key as pkcs8")?;
        let encoding_key = EncodingKey::from_ec_der(der.as_bytes());

        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before epoch")?
            .checked_add(JWT_TTL)
            .context("exp overflow")?
            .as_secs();
        let claims =
            VapidClaims { aud: audience.to_string(), exp, sub: subject.to_string() };

        let jwt = encode(&Header::new(Algorithm::ES256), &claims, &encoding_key)
            .context("signing VAPID JWT")?;
        Ok(format!("vapid t={jwt}, k={}", self.public_key_b64()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base64_url_nopad_decode(s: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(s).unwrap()
    }

    #[test]
    fn vapid_signs_es256_jwt_for_audience() {
        let keys = VapidKeys::generate_for_test();
        let hdr = keys
            .sign_auth_header("https://fcm.googleapis.com", "mailto:dev@example.com")
            .unwrap();
        assert!(hdr.starts_with("vapid t="));
        assert!(hdr.contains(", k="));
        assert_eq!(base64_url_nopad_decode(&keys.public_key_b64()).len(), 65);

        // Verify the JWT header and claims round-trip correctly.
        let jwt = hdr
            .strip_prefix("vapid t=")
            .unwrap()
            .split(", k=")
            .next()
            .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header_json = base64_url_nopad_decode(parts[0]);
        let header: serde_json::Value = serde_json::from_slice(&header_json).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "JWT");
        let claims_json = base64_url_nopad_decode(parts[1]);
        let claims: serde_json::Value = serde_json::from_slice(&claims_json).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:dev@example.com");
    }

    #[test]
    fn load_or_generate_persists_and_reloads_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let grok_home = dir.path();

        let first = VapidKeys::load_or_generate(grok_home).unwrap();
        let second = VapidKeys::load_or_generate(grok_home).unwrap();

        // Same on-disk key reloaded, not a freshly generated one.
        assert_eq!(first.public_key_b64(), second.public_key_b64());

        let path = grok_home.join("push").join("vapid.json");
        assert!(path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
