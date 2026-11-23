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
    /// Enable mTLS messaging at this address; restart running daemons and chats.
    Bind { address: std::net::SocketAddr },
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
        /// Local owner allowed to send to this peer and receive from it; destination owner is learned from its pins route.
        #[arg(long)]
        owner: String,
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
            let selected = config.clone();
            let identity = crate::runtime::blocking::spawn_blocking(move || {
                if prepare_rotate {
                    super::identity::prepare_rotation(&selected)
                } else {
                    super::identity::init(&selected, rotate)
                }
            })
            .await??;
            Ok(format!(
                "{}\nfingerprint {}\nexpires {}\nowner {}\nRestart running daemons and chats after A2A configuration or identity changes.",
                identity.spiffe_id,
                identity.fingerprint,
                identity.expires_at,
                local_owner(&config)
            ))
        }
        A2aCommand::Bind { address } => {
            ensure!(address.port() != 0, "A2A bind requires a nonzero port");
            config.a2a.bind = address.to_string();
            config.a2a.messaging_enabled = true;
            config.a2a.validate()?;
            persist(original, config).await?;
            Ok("A2A messaging enabled; restart running daemons and chats to apply the listener configuration.".into())
        }
        A2aCommand::Peer {
            command: PeerCommand::List,
        } => {
            let selected = config.clone();
            let local = crate::runtime::blocking::spawn_blocking(move || super::identity::load(&selected)).await??;
            let mut rows = vec![format!(
                "local {} owner={} expires {}",
                local.spiffe_id,
                local_owner(&config),
                local.expires_at
            )];
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
            let selected = config.clone();
            let local = crate::runtime::blocking::spawn_blocking(move || super::identity::load(&selected)).await??;
            super::peers::instance(&spiffe)?;
            ensure!(
                super::peers::instance(&spiffe)? != local.instance_id
                    && spiffe != local.spiffe_id
                    && fingerprint != local.fingerprint,
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
            let bound_owner = owner.clone();
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
            Ok(format!(
                "Peer grant saved, bound owner={bound_owner}; both instances must pin each other. Restart running daemons and chats after setup."
            ))
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
            Ok(
                "Peer grant removed; live connections will close. Restart running daemons and chats after setup."
                    .into(),
            )
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

fn local_owner(config: &crate::config::Config) -> String {
    crate::runtime::envelope::RuntimeEnvelope::chat_canonical(
        config.workspace_dir.to_string_lossy().into_owned(),
        "peer-management",
        crate::memory::MemoryVisibility::Workspace,
    )
    .resolved_owner_id()
}
