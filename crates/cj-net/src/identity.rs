//! Device identity (self-signed certificate), fingerprints and the pairing code.

use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

/// Fixed server name (SAN and SNI) used by every CursorJump certificate.
pub const SERVER_NAME: &str = "cursorjump.local";

const CERT_FILE: &str = "identity.der";
const KEY_FILE: &str = "identity.key";
const VALIDITY_YEARS: i32 = 10;

/// SHA-256 of a certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// Fingerprint of the given certificate DER.
    pub fn of_cert(cert_der: &[u8]) -> Self {
        Fingerprint(Sha256::digest(cert_der).into())
    }

    /// Lowercase hex, 64 characters.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parses exactly 64 hex characters (either case).
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Fingerprint(out))
    }
}

/// This device's certificate and PKCS#8 private key, both DER.
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    /// Loads `identity.der` / `identity.key` from `dir` if both are present and
    /// valid, otherwise generates a fresh identity and overwrites them.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);
        if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            let id = Identity { cert_der, key_der };
            if id.is_valid() {
                return Ok(id);
            }
        }
        let id = Identity::generate()?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(&cert_path, &id.cert_der)
            .with_context(|| format!("writing {}", cert_path.display()))?;
        std::fs::write(&key_path, &id.key_der)
            .with_context(|| format!("writing {}", key_path.display()))?;
        Ok(id)
    }

    /// SHA-256 fingerprint of the certificate.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of_cert(&self.cert_der)
    }

    pub(crate) fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }

    fn generate() -> Result<Identity> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::new(vec![SERVER_NAME.to_string()])?;
        let now_year = 1970
            + (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                / 31_556_952) as i32;
        params.not_before = rcgen::date_time_ymd(now_year - 1, 1, 1);
        params.not_after = rcgen::date_time_ymd(now_year + VALIDITY_YEARS, 1, 1);
        let cert = params.self_signed(&key)?;
        Ok(Identity {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
        })
    }

    /// Cheap sanity check: the key must be loadable and the cert must look like DER.
    fn is_valid(&self) -> bool {
        self.cert_der.first() == Some(&0x30)
            && rustls::crypto::ring::sign::any_supported_type(&self.private_key()).is_ok()
    }
}

/// Order-independent 6-digit pairing code ("NNN NNN") for two fingerprints.
pub fn pairing_code(a: &Fingerprint, b: &Fingerprint) -> String {
    let (lo, hi) = if a.0 <= b.0 { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update(lo.0);
    h.update(hi.0);
    let digest = h.finalize();
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert!(dir.path().join("identity.der").exists());
        assert!(dir.path().join("identity.key").exists());
    }

    #[test]
    fn fingerprint_hex_roundtrip() {
        let f = Fingerprint([0xAB; 32]);
        let h = f.to_hex();
        assert_eq!(h.len(), 64);
        assert_eq!(Fingerprint::from_hex(&h), Some(f));
        assert_eq!(Fingerprint::from_hex(&h.to_uppercase()), Some(f));
        assert_eq!(Fingerprint::from_hex("abc"), None);
        assert_eq!(Fingerprint::from_hex(&"zz".repeat(32)), None);
    }

    #[test]
    fn pairing_code_symmetric_and_formatted() {
        let a = Fingerprint([1; 32]);
        let b = Fingerprint([2; 32]);
        let c = pairing_code(&a, &b);
        assert_eq!(c, pairing_code(&b, &a));
        let bytes = c.as_bytes();
        assert_eq!(bytes.len(), 7);
        assert_eq!(bytes[3], b' ');
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(i, ch)| i == 3 || ch.is_ascii_digit())
        );
    }

    #[test]
    fn different_peer_different_code() {
        let a = Fingerprint([1; 32]);
        let b = Fingerprint([2; 32]);
        let c = Fingerprint([3; 32]);
        assert_ne!(pairing_code(&a, &b), pairing_code(&a, &c));
    }
}
