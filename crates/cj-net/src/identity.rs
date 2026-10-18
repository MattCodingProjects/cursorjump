//! Device identity (self-signed certificate), fingerprints and the pairing code.

use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use ring::rand::SecureRandom;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
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
        write_atomic(&cert_path, &id.cert_der)?;
        write_atomic(&key_path, &id.key_der)?;
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

    /// Same check the TLS configs run: the cert parses, the key loads and they match.
    fn is_valid(&self) -> bool {
        rustls::sign::CertifiedKey::from_der(
            vec![CertificateDer::from(self.cert_der.clone())],
            self.private_key(),
            &rustls::crypto::ring::default_provider(),
        )
        .is_ok()
    }
}

/// Writes `data` to a temp file next to `path`, then renames it into place.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Fresh 32-byte random nonce from the OS RNG, for one pairing session.
pub fn random_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut n)
        .expect("OS random number generator failed");
    n
}

/// Commitment to a pairing nonce: SHA-256("cj-commit-v1" || nonce).
pub fn pair_commit(nonce: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"cj-commit-v1");
    h.update(nonce);
    h.finalize().into()
}

/// 6-digit pairing code ("NNN NNN") bound to both fingerprints and both
/// per-session nonces. Roles are explicit (connector vs. listener).
pub fn pairing_code(
    connector: &Fingerprint,
    listener: &Fingerprint,
    connector_nonce: &[u8; 32],
    listener_nonce: &[u8; 32],
) -> String {
    let mut h = Sha256::new();
    h.update(b"cj-pair-v1");
    h.update(connector.0);
    h.update(listener.0);
    h.update(connector_nonce);
    h.update(listener_nonce);
    let d = h.finalize();
    let n = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{Trust, server_config};

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
    fn corrupt_identity_regenerates() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("identity.der"),
            [0x30, 0xde, 0xad, 0xbe, 0xef],
        )
        .unwrap();
        let b = Identity::load_or_create(dir.path()).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert!(server_config(&b, Trust::PairingMode).is_ok());
        let c = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(b.fingerprint(), c.fingerprint());
    }

    #[test]
    fn mismatched_key_regenerates() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(d1.path()).unwrap();
        let other = Identity::load_or_create(d2.path()).unwrap();
        std::fs::write(d1.path().join("identity.key"), &other.key_der).unwrap();
        let b = Identity::load_or_create(d1.path()).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_ne!(b.key_der, other.key_der);
        assert!(server_config(&b, Trust::PairingMode).is_ok());
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
    fn pairing_code_formatted() {
        let c = pairing_code(
            &Fingerprint([1; 32]),
            &Fingerprint([2; 32]),
            &[3; 32],
            &[4; 32],
        );
        let b = c.as_bytes();
        assert_eq!(b.len(), 7);
        assert_eq!(b[3], b' ');
        assert!(
            b.iter()
                .enumerate()
                .all(|(i, ch)| i == 3 || ch.is_ascii_digit())
        );
    }

    #[test]
    fn pairing_code_changes_with_any_input() {
        let (f1, f2, f3) = (
            Fingerprint([1; 32]),
            Fingerprint([2; 32]),
            Fingerprint([9; 32]),
        );
        let (n1, n2, n3) = ([3u8; 32], [4u8; 32], [7u8; 32]);
        let base = pairing_code(&f1, &f2, &n1, &n2);
        assert_ne!(base, pairing_code(&f3, &f2, &n1, &n2));
        assert_ne!(base, pairing_code(&f1, &f3, &n1, &n2));
        assert_ne!(base, pairing_code(&f1, &f2, &n3, &n2));
        assert_ne!(base, pairing_code(&f1, &f2, &n1, &n3));
        // Roles are explicit: swapping them changes the code.
        assert_ne!(base, pairing_code(&f2, &f1, &n2, &n1));
    }

    #[test]
    fn commit_binds_nonce() {
        assert_eq!(pair_commit(&[1; 32]), pair_commit(&[1; 32]));
        assert_ne!(pair_commit(&[1; 32]), pair_commit(&[2; 32]));
    }

    #[test]
    fn nonces_are_random() {
        assert_ne!(random_nonce(), random_nonce());
    }
}
