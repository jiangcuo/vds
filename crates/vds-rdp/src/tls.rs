//! TLS server identity: certificate chain, private key and the public key used by CredSSP.

use std::sync::Arc;

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;

use crate::error::{Error, Result};

/// A TLS identity for the RDP listener.
#[derive(Clone)]
pub struct TlsIdentity {
    pub(crate) acceptor: TlsAcceptor,
    /// Subject public key of the leaf certificate (CredSSP binds the session to it).
    pub(crate) public_key: Vec<u8>,
}

impl core::fmt::Debug for TlsIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TlsIdentity").finish_non_exhaustive()
    }
}

impl TlsIdentity {
    /// Builds an identity from a PEM certificate chain and a PEM private key
    /// (PKCS#8, PKCS#1 or SEC1).
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self> {
        let certs = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Tls(format!("certificate: {e}")))?;
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| Error::Tls(format!("private key: {e}")))?;
        Self::from_der(certs, key)
    }

    /// Generates a self-signed identity (RSA 2048 when available, ECDSA P-256 otherwise).
    ///
    /// Clients will warn about the certificate; use `tls-creds` in production.
    pub fn self_signed(hostname: &str) -> Result<Self> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
            .or_else(|_| rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256))
            .map_err(|e| Error::Tls(format!("key generation: {e}")))?;
        let params = rcgen::CertificateParams::new(vec![hostname.to_owned()])
            .map_err(|e| Error::Tls(format!("certificate parameters: {e}")))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| Error::Tls(format!("self-signing: {e}")))?;
        let key_der = PrivateKeyDer::try_from(key.serialize_der()).map_err(|e| Error::Tls(e.to_owned()))?;
        Self::from_der(vec![cert.der().clone()], key_der)
    }

    fn from_der(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> Result<Self> {
        let leaf = certs
            .first()
            .ok_or_else(|| Error::Tls(String::from("no certificate in chain")))?;
        let public_key = {
            use x509_cert::der::Decode as _;
            let cert = x509_cert::Certificate::from_der(leaf).map_err(|e| Error::Tls(format!("parsing: {e}")))?;
            cert.tbs_certificate
                .subject_public_key_info
                .subject_public_key
                .as_bytes()
                .ok_or_else(|| Error::Tls(String::from("unaligned subject public key")))?
                .to_owned()
        };
        // One explicit provider: rcgen and tokio-rustls both use aws-lc-rs, so there is no
        // process-wide default to rely on.
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Tls(format!("protocol versions: {e}")))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| Error::Tls(format!("certificate/key mismatch: {e}")))?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            public_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TlsIdentity;

    #[test]
    fn self_signed_identity_has_public_key() {
        let id = TlsIdentity::self_signed("localhost").unwrap();
        assert!(!id.public_key.is_empty());
    }

    #[test]
    fn rejects_garbage_pem() {
        assert!(TlsIdentity::from_pem(b"nope", b"nope").is_err());
    }
}
