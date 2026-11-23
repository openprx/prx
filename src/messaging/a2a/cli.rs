//! Explicit identity initialization and pinned peer grant management.
use anyhow::{Result, ensure};
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub enum A2aCommand {
    /// Create the persisted instance identity, or rotate its Ed25519 key.
    Init {
        #[arg(long)]
        rotate: bool,
        /// Stage a new identity so peers can pre-pin it before activation.
        #[arg(long, conflicts_with = "rotate")]
        prepare_rotate: bool,
    },
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
}
#[derive(Debug, Subcommand)]
pub enum PeerCommand {
    Add {
        name: String,
        #[arg(long)]
        spiffe: String,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        address: String,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long = "session", default_value = "*")]
        sessions: Vec<String>,
        #[arg(long = "agent", default_value = "*")]
        agents: Vec<String>,
    },
    List,
    Remove {
        name: String,
        #[arg(long)]
        fingerprint: Option<String>,
    },
}
pub async fn run(command: A2aCommand, mut config: crate::config::Config) -> Result<String> {
    let original = config.clone();
    match command {
        A2aCommand::Init { rotate, prepare_rotate } => {
            let identity = if prepare_rotate {
                super::identity::prepare_rotation(&config)?
            } else {
                super::identity::init(&config, rotate)?
            };
            Ok(format!(
                "{}\nfingerprint {}\nexpires {}",
                identity.spiffe_id, identity.fingerprint, identity.expires_at
            ))
        }
        A2aCommand::Peer {
            command: PeerCommand::List,
        } => {
            let local = super::identity::load(&config)?;
            let mut rows = vec![format!("local {} expires {}", local.spiffe_id, local.expires_at)];
            let mut peers: Vec<_> = config.a2a.peers.iter().collect();
            peers.sort_by_key(|(name, _)| *name);
            for (name, peer) in peers {
                let expires = super::tls::peer_expiry(&config, peer).await.map_or_else(
                    |_| "unavailable (peer unreachable or refused)".into(),
                    |expiry| expiry.to_string(),
                );
                rows.push(format!(
                    "{name} {} {} owner={} expires={expires} fingerprints={}",
                    peer.spiffe_id,
                    peer.address,
                    peer.owner,
                    peer.fingerprints
                        .iter()
                        .map(|value| value.get(..16).unwrap_or(value))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            Ok(rows.join("\n"))
        }
        A2aCommand::Peer {
            command:
                PeerCommand::Add {
                    name,
                    spiffe,
                    fingerprint,
                    address,
                    owner,
                    sessions,
                    agents,
                },
        } => {
            let local = super::identity::load(&config)?;
            super::peers::instance(&spiffe)?;
            ensure!(
                spiffe != local.spiffe_id && fingerprint != local.fingerprint,
                "local identity cannot be a peer"
            );
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "invalid peer name"
            );
            let owner = owner.unwrap_or_else(|| {
                crate::runtime::envelope::RuntimeEnvelope::chat_canonical(
                    config.workspace_dir.to_string_lossy().into_owned(),
                    "peer-management",
                    crate::memory::MemoryVisibility::Workspace,
                )
                .resolved_owner_id()
            });
            let mut fingerprints = vec![fingerprint.clone()];
            if let Some(old) = config.a2a.peers.get(&name) {
                ensure!(
                    old.spiffe_id == spiffe,
                    "remove existing peer before changing its identity"
                );
                fingerprints = old.fingerprints.clone();
                if !fingerprints.contains(&fingerprint) {
                    fingerprints.push(fingerprint);
                }
            }
            config.a2a.peers.insert(
                name,
                crate::config::A2aPeer {
                    spiffe_id: spiffe,
                    fingerprints,
                    address,
                    owner,
                    sessions,
                    agents,
                },
            );
            config.a2a.validate()?;
            persist(original, config).await?;
            Ok("Peer grant saved; both instances must pin each other.".into())
        }
        A2aCommand::Peer {
            command: PeerCommand::Remove { name, fingerprint },
        } => {
            if let Some(fingerprint) = fingerprint {
                let peer = config
                    .a2a
                    .peers
                    .get_mut(&name)
                    .ok_or_else(|| anyhow::anyhow!("peer not found"))?;
                peer.fingerprints.retain(|value| value != &fingerprint);
                if peer.fingerprints.is_empty() {
                    config.a2a.peers.remove(&name);
                }
            } else {
                ensure!(config.a2a.peers.remove(&name).is_some(), "peer not found");
            }
            config.a2a.validate()?;
            persist(original, config).await?;
            Ok("Peer grant removed; live connections will close.".into())
        }
    }
}

async fn persist(before: crate::config::Config, after: crate::config::Config) -> Result<()> {
    use crate::config::manage::{ConfigActor, ManagementService};
    let service = ManagementService::deferred(crate::config::new_shared(before));
    let plan = service.plan_candidate(after, None, ConfigActor::Cli).await?;
    let revision = plan.revision().map(str::to_owned);
    service.apply(plan, ConfigActor::Cli, revision.as_deref()).await?;
    Ok(())
}
