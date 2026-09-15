use std::sync::Arc;

use hawse_proto::key::PublicKey;
use hawse_proto::msg::ALPN;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as TlsError,
    PeerIncompatible, SignatureScheme,
};
use x509_parser::prelude::FromDer as _;

#[cfg(all(feature = "ring", feature = "aws-lc-rs"))]
compile_error!("enable exactly one of the `ring` and `aws-lc-rs` features");
#[cfg(not(any(feature = "ring", feature = "aws-lc-rs")))]
compile_error!("enable one of the `ring` and `aws-lc-rs` features");

pub fn provider() -> Arc<CryptoProvider> {
    #[cfg(feature = "ring")]
    {
        Arc::new(rustls::crypto::ring::default_provider())
    }
    #[cfg(feature = "aws-lc-rs")]
    {
        Arc::new(rustls::crypto::aws_lc_rs::default_provider())
    }
}

const ED25519_OID: &str = "1.3.101.112";

#[derive(Debug, thiserror::Error)]
pub enum PeerKeyError {
    #[error("peer certificate is not valid DER")]
    Der,
    #[error("peer key algorithm {0} is not Ed25519")]
    Algorithm(String),
    #[error("peer key is not 32 bytes")]
    Length,
}

pub fn peer_key(cert: &CertificateDer<'_>) -> Result<PublicKey, PeerKeyError> {
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert.as_ref())
        .map_err(|_| PeerKeyError::Der)?;
    let spki = parsed.public_key();
    let oid = spki.algorithm.algorithm.to_id_string();
    if oid != ED25519_OID {
        return Err(PeerKeyError::Algorithm(oid));
    }
    PublicKey::from_slice(&spki.subject_public_key.data).map_err(|_| PeerKeyError::Length)
}

fn bad_cert(_: PeerKeyError) -> TlsError {
    TlsError::InvalidCertificate(CertificateError::BadEncoding)
}

#[derive(Debug)]
pub struct PinnedServer {
    expected: PublicKey,
    algs: WebPkiSupportedAlgorithms,
}

impl PinnedServer {
    pub fn new(expected: PublicKey, provider: &CryptoProvider) -> Self {
        Self {
            expected,
            algs: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let key = peer_key(end_entity).map_err(bad_cert)?;
        if key == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(
            PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Possession is proven by the TLS 1.3 `CertificateVerify` message; authorization happens in the control stream.
#[derive(Debug)]
pub struct AnyEd25519Client {
    algs: WebPkiSupportedAlgorithms,
}

impl AnyEd25519Client {
    pub fn new(provider: &CryptoProvider) -> Self {
        Self {
            algs: provider.signature_verification_algorithms,
        }
    }
}

impl ClientCertVerifier for AnyEd25519Client {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        peer_key(end_entity)
            .map(|_| ClientCertVerified::assertion())
            .map_err(bad_cert)
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(
            PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

pub fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    provider: Arc<CryptoProvider>,
) -> Result<rustls::ServerConfig, TlsError> {
    let verifier = Arc::new(AnyEd25519Client::new(&provider));
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert], key)?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    cfg.max_early_data_size = 0;
    Ok(cfg)
}

pub fn client_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    server_key: PublicKey,
    provider: Arc<CryptoProvider>,
) -> Result<rustls::ClientConfig, TlsError> {
    let verifier = Arc::new(PinnedServer::new(server_key, &provider));
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert], key)?;
    cfg.alpn_protocols = vec![ALPN.to_vec()];
    cfg.enable_early_data = false;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use rustls::pki_types::ServerName;
    use rustls::{ClientConnection, ServerConnection};

    fn handshake(
        client: &mut ClientConnection,
        server: &mut ServerConnection,
    ) -> Result<(), rustls::Error> {
        for _ in 0..16 {
            let mut c2s = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut c2s).unwrap();
            }
            let mut cursor = &c2s[..];
            while !cursor.is_empty() {
                server.read_tls(&mut cursor).unwrap();
            }
            server.process_new_packets()?;
            let mut s2c = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut s2c).unwrap();
            }
            let mut cursor = &s2c[..];
            while !cursor.is_empty() {
                client.read_tls(&mut cursor).unwrap();
            }
            client.process_new_packets()?;
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
        }
        panic!("handshake did not converge");
    }

    fn pair() -> (Identity, Identity) {
        (Identity::generate().unwrap(), Identity::generate().unwrap())
    }

    fn server(id: &Identity) -> ServerConnection {
        let (cert, key) = id.certificate().unwrap();
        ServerConnection::new(Arc::new(server_config(cert, key, provider()).unwrap())).unwrap()
    }

    fn client(id: &Identity, pinned: PublicKey) -> ClientConnection {
        let (cert, key) = id.certificate().unwrap();
        let cfg = client_config(cert, key, pinned, provider()).unwrap();
        ClientConnection::new(Arc::new(cfg), ServerName::try_from("hawse").unwrap()).unwrap()
    }

    #[test]
    fn peer_key_reads_ed25519_spki() {
        let id = Identity::generate().unwrap();
        let (cert, _) = id.certificate().unwrap();
        assert_eq!(peer_key(&cert).unwrap(), id.public_key());
    }

    #[test]
    fn peer_key_rejects_other_algorithms() {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let cert = rcgen::CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .self_signed(&kp)
            .unwrap();
        assert!(matches!(
            peer_key(cert.der()),
            Err(PeerKeyError::Algorithm(_))
        ));
    }

    #[test]
    fn mutual_handshake_exposes_both_keys_and_alpn() {
        let (s, c) = pair();
        let mut server = server(&s);
        let mut client = client(&c, s.public_key());
        handshake(&mut client, &mut server).unwrap();
        assert_eq!(server.alpn_protocol(), Some(ALPN));
        let cert = &server.peer_certificates().unwrap()[0];
        assert_eq!(peer_key(cert).unwrap(), c.public_key());
        assert_eq!(
            server.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
    }

    #[test]
    fn client_refuses_unpinned_server() {
        let (s, c) = pair();
        let other = Identity::generate().unwrap();
        let mut server = server(&s);
        let mut client = client(&c, other.public_key());
        let err = handshake(&mut client, &mut server).unwrap_err();
        assert!(
            matches!(
                err,
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
            ),
            "{err:?}"
        );
    }

    #[test]
    fn server_requires_a_client_certificate() {
        let (s, _) = pair();
        let mut server = server(&s);
        let cfg = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServer::new(
                s.public_key(),
                &provider(),
            )))
            .with_no_client_auth();
        let mut cfg = cfg;
        cfg.alpn_protocols = vec![ALPN.to_vec()];
        let mut client =
            ClientConnection::new(Arc::new(cfg), ServerName::try_from("hawse").unwrap()).unwrap();
        let err = handshake(&mut client, &mut server).unwrap_err();
        assert!(
            matches!(err, rustls::Error::NoCertificatesPresented),
            "{err:?}"
        );
    }

    #[test]
    fn early_data_is_disabled() {
        let (s, c) = pair();
        let (cert, key) = s.certificate().unwrap();
        assert_eq!(
            server_config(cert, key, provider())
                .unwrap()
                .max_early_data_size,
            0
        );
        let (cert, key) = c.certificate().unwrap();
        assert!(
            !client_config(cert, key, s.public_key(), provider())
                .unwrap()
                .enable_early_data
        );
    }
}
