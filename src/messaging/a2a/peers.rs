//! Pinned peer configuration and SPIFFE instance names.
//! Trust-domain syntax follows https://spiffe.io/docs/latest/spiffe-specs/spiffe-id/#21-trust-domain.
use crate::config::schema::{A2aConfig, A2aPeer};
use anyhow::{Result, ensure};
use std::collections::HashSet;

pub fn instance(spiffe: &str) -> Result<uuid::Uuid> {
    ensure!(spiffe.len() <= 2048, "SPIFFE ID exceeds 2048 bytes");
    let rest = spiffe
        .strip_prefix("spiffe://")
        .ok_or_else(|| anyhow::anyhow!("invalid SPIFFE scheme"))?;
    let (domain, path) = rest
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("SPIFFE path missing"))?;
    ensure!(
        !domain.is_empty()
            && domain.len() <= 255
            && domain
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')),
        "invalid SPIFFE trust domain"
    );
    let id = path
        .strip_prefix("prx/")
        .ok_or_else(|| anyhow::anyhow!("invalid SPIFFE instance path"))?;
    let uuid = uuid::Uuid::parse_str(id)?;
    ensure!(id == uuid.to_string(), "SPIFFE instance must be a canonical UUID");
    Ok(uuid)
}
pub fn validate(config: &A2aConfig) -> Result<()> {
    let mut identities = HashSet::new();
    let mut fingerprints = HashSet::new();
    let mut instances = HashSet::new();
    instance(&format!("spiffe://{}/prx/{}", config.trust_domain, uuid::Uuid::nil()))?;
    for peer in config.peers.values() {
        ensure!(
            instances.insert(instance(&peer.spiffe_id)?),
            "duplicate peer instance UUID"
        );
        ensure!(identities.insert(&peer.spiffe_id), "duplicate peer SPIFFE ID");
        ensure!(!peer.owner.trim().is_empty(), "peer owner is required");
        ensure!(
            !peer.sessions.is_empty() && !peer.agents.is_empty(),
            "peer recipient allowlists cannot be empty"
        );
        ensure!(
            (1..=2).contains(&peer.fingerprints.len()),
            "peer requires one or two fingerprints"
        );
        for fingerprint in &peer.fingerprints {
            ensure!(
                fingerprint.len() == 64
                    && fingerprint
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "fingerprint must be 64 lowercase hex characters"
            );
            ensure!(fingerprints.insert(fingerprint), "duplicate peer fingerprint");
        }
        let url = reqwest::Url::parse(&format!("https://{}", peer.address))?;
        ensure!(
            url.host_str().is_some()
                && url.port_or_known_default().is_some()
                && peer
                    .address
                    .rsplit_once(':')
                    .is_some_and(|(_, port)| port.parse::<u16>().is_ok_and(|port| port > 0))
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "peer address must be host:port"
        );
    }
    Ok(())
}
pub(crate) fn allows(peer: &A2aPeer, target: &crate::messaging::SessionAddr, local: &str) -> bool {
    target.instance == local
        && target.owner == peer.owner
        && peer
            .sessions
            .iter()
            .any(|value| value == "*" || value == &target.session)
        && peer.agents.iter().any(|value| value == "*" || value == &target.agent)
}

pub(crate) fn validate_local(config: &A2aConfig, local: &super::identity::Identity) -> Result<()> {
    validate(config)?;
    for peer in config.peers.values() {
        ensure!(
            instance(&peer.spiffe_id)? != local.instance_id
                && peer.spiffe_id != local.spiffe_id
                && !peer.fingerprints.contains(&local.fingerprint),
            "local identity cannot be a peer"
        );
    }
    Ok(())
}
