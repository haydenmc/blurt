//! TLS for the control connection: server certificate trust and client identity.
//!
//! Server certificates are trusted if they validate against the system's CA
//! store (public servers with e.g. Let's Encrypt certificates), or if their
//! SHA-256 fingerprint matches one the user pinned earlier (trust on first
//! use, since most Mumble servers are self-signed). Otherwise the handshake
//! fails with the fingerprint, so the app can ask the user and retry.
//!
//! Mumble identifies registered users by their client certificate, so a
//! client keeps one long-lived self-signed certificate per identity.

use std::sync::{Arc, Mutex, OnceLock};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};

/// SHA-256 of a certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(der: &[u8]) -> Self {
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der);
        Self(digest.as_ref().try_into().expect("SHA-256 is 32 bytes"))
    }
}

impl std::fmt::Display for Fingerprint {
    /// Colon-separated hex, as certificate viewers show it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// A client certificate and its private key (PKCS#8), both DER.
#[derive(Clone)]
pub struct ClientIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("ClientIdentity")
            .field("fingerprint", &Fingerprint::of(&self.cert_der))
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    /// Generates a new self-signed identity. Keep it: registered users are
    /// recognized by it, and a new one is a different user to the server.
    pub fn generate(common_name: &str) -> Result<Self, rcgen::Error> {
        let key = rcgen::KeyPair::generate()?;
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        // Long-lived, like the official client's: the identity is the cert.
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(9999, 12, 31);
        let cert = params.self_signed(&key)?;
        Ok(Self {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
        })
    }
}

/// Accepts CA-valid certificates, or a pinned fingerprint. Records the
/// fingerprint it saw either way, so a failure can report it.
#[derive(Debug)]
struct TofuVerifier {
    provider: Arc<CryptoProvider>,
    /// `None` if the system has no usable CA certificates.
    webpki: Option<Arc<WebPkiServerVerifier>>,
    pinned: Option<Fingerprint>,
    seen: Arc<Mutex<Option<Fingerprint>>>,
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = Fingerprint::of(end_entity);
        *self.seen.lock().unwrap() = Some(fingerprint);

        if self.pinned == Some(fingerprint) {
            return Ok(ServerCertVerified::assertion());
        }
        match &self.webpki {
            Some(webpki) => webpki.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            ),
            None => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The system's CA certificates, loaded once per process.
fn system_roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            for e in &loaded.errors {
                log::warn!("Problem loading system CA certificates: {e}");
            }
            let mut roots = RootCertStore::empty();
            let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
            log::debug!("Loaded {added} system CA certificates ({ignored} unparsable)");
            Arc::new(roots)
        })
        .clone()
}

/// A connector for one connection attempt, and where it records the server's
/// certificate fingerprint.
pub(crate) struct Connector {
    pub connector: tokio_rustls::TlsConnector,
    pub seen: Arc<Mutex<Option<Fingerprint>>>,
}

pub(crate) fn make_connector(
    pinned: Option<Fingerprint>,
    identity: Option<&ClientIdentity>,
) -> Result<Connector, rustls::Error> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let roots = system_roots();
    let webpki = if roots.is_empty() {
        None
    } else {
        Some(
            WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
                .build()
                .map_err(|e| rustls::Error::General(e.to_string()))?,
        )
    };
    let seen = Arc::new(Mutex::new(None));
    let verifier = TofuVerifier {
        provider: provider.clone(),
        webpki,
        pinned,
        seen: seen.clone(),
    };

    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier));
    let config = match identity {
        Some(identity) => builder.with_client_auth_cert(
            vec![CertificateDer::from(identity.cert_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone())),
        )?,
        None => builder.with_no_client_auth(),
    };

    Ok(Connector {
        connector: tokio_rustls::TlsConnector::from(Arc::new(config)),
        seen,
    })
}

/// Whether an I/O error from the TLS handshake is a rejected server certificate.
pub(crate) fn certificate_error(error: &std::io::Error) -> Option<&rustls::CertificateError> {
    match error.get_ref()?.downcast_ref::<rustls::Error>()? {
        rustls::Error::InvalidCertificate(e) => Some(e),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_displays_as_colon_hex() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xAB;
        bytes[31] = 0x01;
        let shown = Fingerprint(bytes).to_string();
        assert!(shown.starts_with("AB:00:"));
        assert!(shown.ends_with(":00:01"));
        assert_eq!(shown.len(), 32 * 3 - 1);
    }

    #[test]
    fn generated_identities_are_distinct_and_usable() {
        let a = ClientIdentity::generate("alice").unwrap();
        let b = ClientIdentity::generate("alice").unwrap();
        assert_ne!(Fingerprint::of(&a.cert_der), Fingerprint::of(&b.cert_der));
        assert!(make_connector(None, Some(&a)).is_ok());
    }
}
