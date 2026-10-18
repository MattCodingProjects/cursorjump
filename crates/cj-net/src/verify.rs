//! Pinned-fingerprint TLS verifiers and the rustls configs built on them.

use std::sync::Arc;

use anyhow::Result;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsError,
    ServerConfig, SignatureScheme,
};

pub use crate::identity::SERVER_NAME;
use crate::identity::{Fingerprint, Identity};

/// ALPN protocol id for CursorJump v1.
pub const ALPN: &[u8] = b"cursorjump/1";

/// Whom to trust: anyone (first-contact pairing) or exactly one fingerprint.
#[derive(Clone, Debug)]
pub enum Trust {
    PairingMode,
    Pinned(Fingerprint),
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn check_cert(
    trust: &Trust,
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> Result<(), TlsError> {
    let reject = || TlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure);
    if !intermediates.is_empty() {
        return Err(reject());
    }
    match trust {
        Trust::PairingMode => Ok(()),
        Trust::Pinned(fp) if Fingerprint::of_cert(end_entity) == *fp => Ok(()),
        Trust::Pinned(_) => Err(reject()),
    }
}

fn tls12_unsupported() -> TlsError {
    TlsError::General("TLS 1.2 is not supported".into())
}

/// Client-side verifier for the server's certificate.
#[derive(Debug)]
pub struct CjServerCertVerifier {
    trust: Trust,
    algs: WebPkiSupportedAlgorithms,
}

impl CjServerCertVerifier {
    pub fn new(trust: Trust) -> Self {
        Self {
            trust,
            algs: provider().signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for CjServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        check_cert(&self.trust, end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(tls12_unsupported())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Server-side verifier for the client's (mandatory) certificate.
#[derive(Debug)]
pub struct CjClientCertVerifier {
    trust: Trust,
    algs: WebPkiSupportedAlgorithms,
}

impl CjClientCertVerifier {
    pub fn new(trust: Trust) -> Self {
        Self {
            trust,
            algs: provider().signature_verification_algorithms,
        }
    }
}

impl ClientCertVerifier for CjClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        check_cert(&self.trust, end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(tls12_unsupported())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// TLS 1.3-only server config that requires a client certificate matching `trust`.
pub fn server_config(id: &Identity, trust: Trust) -> Result<ServerConfig> {
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(CjClientCertVerifier::new(trust)))
        .with_single_cert(
            vec![CertificateDer::from(id.cert_der.clone())],
            id.private_key(),
        )?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

/// TLS 1.3-only client config presenting our certificate and checking the server against `trust`.
pub fn client_config(id: &Identity, trust: Trust) -> Result<ClientConfig> {
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CjServerCertVerifier::new(trust)))
        .with_client_auth_cert(
            vec![CertificateDer::from(id.cert_der.clone())],
            id.private_key(),
        )?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    fn verifier(trust: Trust) -> CjServerCertVerifier {
        CjServerCertVerifier::new(trust)
    }

    fn check(v: &CjServerCertVerifier, id: &Identity) -> Result<(), rustls::Error> {
        let cert = CertificateDer::from(id.cert_der.clone());
        let name = ServerName::try_from(SERVER_NAME).unwrap();
        v.verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .map(|_| ())
    }

    #[test]
    fn pinned_verifier_accepts_match_rejects_other() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(d1.path()).unwrap();
        let b = Identity::load_or_create(d2.path()).unwrap();
        let v = verifier(Trust::Pinned(a.fingerprint()));
        assert!(check(&v, &a).is_ok());
        assert!(check(&v, &b).is_err());
    }

    #[test]
    fn pairing_verifier_accepts_any() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(d1.path()).unwrap();
        let b = Identity::load_or_create(d2.path()).unwrap();
        let v = verifier(Trust::PairingMode);
        assert!(check(&v, &a).is_ok());
        assert!(check(&v, &b).is_ok());
    }

    #[test]
    fn configs_build_with_alpn() {
        let d = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(d.path()).unwrap();
        let s = server_config(&id, Trust::PairingMode).unwrap();
        let c = client_config(&id, Trust::Pinned(id.fingerprint())).unwrap();
        assert_eq!(s.alpn_protocols, vec![ALPN.to_vec()]);
        assert_eq!(c.alpn_protocols, vec![ALPN.to_vec()]);
    }
}
