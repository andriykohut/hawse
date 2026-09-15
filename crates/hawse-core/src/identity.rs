use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hawse_proto::key::PublicKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Identity {
    key_pair: rcgen::KeyPair,
    public: PublicKey,
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("cannot read key file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot write key file {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("key file is not a PKCS#8 PEM private key")]
    Parse(#[source] rcgen::Error),
    #[error("key is {0}; only Ed25519 is supported")]
    WrongAlgorithm(String),
    #[error("key generation failed")]
    Generate(#[source] rcgen::Error),
    #[error("certificate generation failed")]
    Certificate(#[source] rcgen::Error),
}

impl Identity {
    pub fn generate() -> Result<Self, IdentityError> {
        let key_pair =
            rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(IdentityError::Generate)?;
        Self::from_key_pair(key_pair)
    }

    pub fn from_pem(pem: &str) -> Result<Self, IdentityError> {
        let key_pair = rcgen::KeyPair::from_pem(pem).map_err(IdentityError::Parse)?;
        Self::from_key_pair(key_pair)
    }

    fn from_key_pair(key_pair: rcgen::KeyPair) -> Result<Self, IdentityError> {
        if key_pair.algorithm() != &rcgen::PKCS_ED25519 {
            return Err(IdentityError::WrongAlgorithm(format!(
                "{:?}",
                key_pair.algorithm()
            )));
        }
        let public = PublicKey::from_slice(key_pair.public_key_raw())
            .map_err(|e| IdentityError::WrongAlgorithm(e.to_string()))?;
        Ok(Self { key_pair, public })
    }

    pub fn to_pem(&self) -> String {
        self.key_pair.serialize_pem()
    }

    /// The flag is `true` when no key existed and one was written (mode 0600).
    pub fn load_or_create(path: &Path) -> Result<(Self, bool), IdentityError> {
        match fs::read_to_string(path) {
            Ok(pem) => Ok((Self::from_pem(&pem)?, false)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                write_private(path, identity.to_pem().as_bytes()).map_err(|source| {
                    IdentityError::Write {
                        path: path.to_owned(),
                        source,
                    }
                })?;
                Ok((identity, true))
            }
            Err(source) => Err(IdentityError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    /// Self-signed; verifiers on both sides read only the public key inside it.
    pub fn certificate(
        &self,
    ) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), IdentityError> {
        let params = rcgen::CertificateParams::new(Vec::<String>::new())
            .map_err(IdentityError::Certificate)?;
        let cert = params
            .self_signed(&self.key_pair)
            .map_err(IdentityError::Certificate)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pair.serialize_der()));
        Ok((cert.der().clone(), key))
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    io::Write::write_all(&mut file, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::FromDer as _;

    #[test]
    fn generates_ed25519() {
        let id = Identity::generate().unwrap();
        assert!(id.public_key().to_string().starts_with("ed25519:"));
    }

    #[test]
    fn pem_round_trip_keeps_the_key() {
        let id = Identity::generate().unwrap();
        let pem = id.to_pem();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(
            Identity::from_pem(&pem).unwrap().public_key(),
            id.public_key()
        );
    }

    #[test]
    fn rejects_other_algorithms() {
        let ec = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        assert!(matches!(
            Identity::from_pem(&ec.serialize_pem()),
            Err(IdentityError::WrongAlgorithm(_))
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            Identity::from_pem("not a key"),
            Err(IdentityError::Parse(_))
        ));
    }

    #[test]
    fn load_or_create_creates_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("client.key");
        let (first, created) = Identity::load_or_create(&path).unwrap();
        assert!(created);
        let (second, created) = Identity::load_or_create(&path).unwrap();
        assert!(!created);
        assert_eq!(first.public_key(), second.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn certificate_carries_the_public_key() {
        let id = Identity::generate().unwrap();
        let (cert, _key) = id.certificate().unwrap();
        let (_, parsed) =
            x509_parser::certificate::X509Certificate::from_der(cert.as_ref()).unwrap();
        assert_eq!(
            parsed.public_key().subject_public_key.data.as_ref(),
            id.public_key().as_bytes()
        );
    }
}
