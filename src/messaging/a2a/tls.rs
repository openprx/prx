//! Pinned Ed25519 verification plus real TLS CertificateVerify signatures.
use super::{identity, peers};
use crate::config::{A2aPeer, Config};
use anyhow::{Result, ensure};
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use std::sync::Arc;

pub(crate) use super::trust::Trust;
impl Trust {
    pub(crate) fn resolve(&self, cert: &CertificateDer<'_>, now: UnixTime) -> Result<A2aPeer> {
        let config = self.effective()?.a2a;
        let fingerprint = identity::fingerprint(cert.as_ref());
        let peer = config
            .peers
            .values()
            .find(|peer| peer.fingerprints.contains(&fingerprint))
            .ok_or_else(|| anyhow::anyhow!("peer unauthenticated"))?;
        check_certificate(cert, &peer.spiffe_id, now)?;
        Ok(peer.clone())
    }
}
const fn rejected() -> Error {
    Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)
}
pub(crate) fn check_certificate(cert: &CertificateDer<'_>, spiffe: &str, now: UnixTime) -> Result<()> {
    peers::instance(spiffe)?;
    let (rest, parsed) =
        x509_parser::parse_x509_certificate(cert.as_ref()).map_err(|_| anyhow::anyhow!("invalid peer certificate"))?;
    ensure!(rest.is_empty(), "trailing certificate DER");
    let seconds = i64::try_from(now.as_secs())?;
    ensure!(
        parsed.validity().not_before.timestamp() <= seconds && seconds <= parsed.validity().not_after.timestamp(),
        "peer certificate outside validity window"
    );
    ensure!(
        parsed.public_key().algorithm.algorithm.to_id_string() == "1.3.101.112",
        "peer key must be Ed25519"
    );
    ensure!(
        parsed.basic_constraints()?.is_none_or(|basic| !basic.value.ca),
        "peer certificate cannot be a CA"
    );
    let san = parsed
        .subject_alternative_name()?
        .ok_or_else(|| anyhow::anyhow!("SPIFFE URI SAN missing"))?;
    let uris: Vec<_> = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            x509_parser::extensions::GeneralName::URI(uri) => Some(*uri),
            _ => None,
        })
        .collect();
    ensure!(
        uris.as_slice() == [spiffe],
        "peer requires exactly the pinned SPIFFE URI SAN"
    );
    Ok(())
}
fn signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> std::result::Result<HandshakeSignatureValid, Error> {
    if dss.scheme != SignatureScheme::ED25519 {
        return Err(rejected());
    }
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &rustls::crypto::ring::default_provider().signature_verification_algorithms,
    )
}
impl ClientCertVerifier for Trust {
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
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, Error> {
        if !intermediates.is_empty() {
            return Err(rejected());
        }
        self.resolve(cert, now).map_err(|_| rejected())?;
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        Err(rejected())
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        signature(msg, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
#[derive(Debug)]
struct ServerPin {
    trust: Trust,
    spiffe: String,
}
impl ServerCertVerifier for ServerPin {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        if !intermediates.is_empty() {
            return Err(rejected());
        }
        let peer = self.trust.resolve(cert, now).map_err(|_| rejected())?;
        if peer.spiffe_id != self.spiffe {
            return Err(rejected());
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        Err(rejected())
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        signature(msg, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}
pub(crate) fn server(trust: Trust) -> Result<rustls::ServerConfig> {
    let identity = trust.identity()?;
    check_certificate(&identity.cert, &identity.spiffe_id, UnixTime::now())?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(trust))
        .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}
pub(crate) fn client(trust: Trust, peer: &A2aPeer) -> Result<reqwest::Client> {
    let identity = trust.identity()?;
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ServerPin {
            trust,
            spiffe: peer.spiffe_id.clone(),
        }))
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    config.resumption = rustls::client::Resumption::disabled();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .http1_only()
        .pool_max_idle_per_host(1)
        .tls_info(true)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

pub(crate) async fn peer_expiry(config: &Config, peer: &A2aPeer) -> Result<i64> {
    let trust = Trust::new(config);
    trust.refresh_async().await?;
    let address = peer.address.clone();
    let peer = peer.clone();
    let client = crate::runtime::blocking::spawn_blocking(move || client(trust, &peer)).await??;
    let response = client.head(format!("https://{address}/a2a/v1/pins")).send().await?;
    let certificate = response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(reqwest::tls::TlsInfo::peer_certificate)
        .ok_or_else(|| anyhow::anyhow!("peer certificate missing"))?;
    let (_, parsed) =
        x509_parser::parse_x509_certificate(certificate).map_err(|_| anyhow::anyhow!("invalid peer certificate"))?;
    Ok(parsed.validity().not_after.timestamp())
}
