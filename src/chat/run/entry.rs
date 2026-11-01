//! Local chat entry construction; remote/config paths cannot create a producer.
use crate::{
    config::Config,
    runtime::{
        bootstrap::{AppContext, BootstrapProfile, RuntimeBootstrap},
        h2_entry::{EntryKind, EntryLimits, TerminalInputProducer},
    },
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: Config,
    provider: Option<String>,
    model: Option<String>,
    temperature: f64,
    plain: bool,
    session: Option<String>,
    list: bool,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    super::run_internal(
        config,
        provider,
        model,
        temperature,
        plain,
        session,
        list,
        shutdown,
        None,
    )
    .await
}

/// Only the direct OS command supplies these bounded bootstrap intentions.
#[allow(clippy::too_many_arguments)]
pub async fn run_controlled(
    config: Config,
    provider: Option<String>,
    model: Option<String>,
    temperature: f64,
    plain: bool,
    session: Option<String>,
    shutdown: CancellationToken,
    limits_json: String,
    authority_seconds: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        provider
            .as_ref()
            .is_none_or(|value| config.default_provider.as_ref() == Some(value))
            && model
                .as_ref()
                .is_none_or(|value| config.default_model.as_ref() == Some(value))
            && temperature.to_bits() == config.default_temperature.to_bits(),
        "controlled chat requires its fixed configured provider, model and temperature"
    );
    anyhow::ensure!(
        limits_json.len() <= 256 * 1024,
        "controlled launch intent exceeds its input bound"
    );
    let limits: EntryLimits =
        serde_json::from_str(&limits_json).map_err(|_| anyhow::anyhow!("invalid controlled experiment ceilings"))?;
    let expires = chrono::Utc::now()
        .checked_add_signed(
            chrono::Duration::try_seconds(i64::try_from(authority_seconds)?)
                .ok_or_else(|| anyhow::anyhow!("controlled authority duration exceeds its supported range"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("controlled authority expiry overflow"))?;
    anyhow::ensure!(
        cfg!(feature = "terminal-tui") && !plain && std::env::var("PRX_TUI").as_deref() != Ok("0"),
        "controlled chat requires the cancellable foreground TUI reader"
    );
    let producer = TerminalInputProducer::open(&config, EntryKind::Terminal, None, expires, limits)?;
    producer.confirm_local_launch(&shutdown).await?;
    let profile = producer.profile();
    let result = super::run_internal(
        config,
        provider,
        model,
        temperature,
        plain,
        session,
        false,
        shutdown,
        Some(producer),
    )
    .await;
    let cleanup = profile.close_reader().await;
    cleanup?;
    result
}

pub(super) async fn build_context(
    config: Config,
    list: bool,
    producer: Option<TerminalInputProducer>,
) -> anyhow::Result<Arc<AppContext>> {
    // `list_sessions` is a CLI flag known before any subsystem is built, so we
    // pick the profile once: `MemoryOnly` early-exits after memory (no tools),
    // `Interactive` builds the full memory + tools set. Either way memory is
    // built exactly once — no duplicate construction. The bootstrap wires
    // observer → security(+audit) → memory → runtime → tools in the hard-ordered
    // sequence, replacing the former hand-wired block (behaviour-equivalent).
    let profile = if list {
        BootstrapProfile::MemoryOnly
    } else {
        BootstrapProfile::Interactive
    };
    match producer {
        Some(producer) => {
            anyhow::ensure!(!list, "controlled experiments cannot use a list-only entry");
            RuntimeBootstrap::build_controlled(config, profile, producer).await
        }
        None => RuntimeBootstrap::build(config, profile).await,
    }
}

pub(super) fn validate_input(controlled: bool, proof: bool, text: &str) -> anyhow::Result<()> {
    if !controlled {
        return Ok(());
    }
    if let Some(command) = text.trim().strip_prefix('/') {
        let verb = command.split_whitespace().next().unwrap_or_default();
        anyhow::ensure!(
            proof
                && matches!(
                    verb,
                    "help"
                        | "new"
                        | "save"
                        | "resume"
                        | "resume-decision"
                        | "approval"
                        | "revoke"
                        | "approve"
                        | "deny"
                        | "cancel"
                        | "queue"
                        | "cost"
                        | "exit"
                        | "quit"
                        | "clear"
                ),
            "command is unavailable in a controlled experiment session"
        );
    }
    Ok(())
}

#[cfg_attr(not(feature = "terminal-tui"), allow(dead_code))]
pub(super) fn take_producer(context: &AppContext) -> Option<TerminalInputProducer> {
    context.terminal_producer.lock().take()
}

pub(super) fn prepare_origin(
    origin: Option<crate::runtime::h2_entry::TerminalInputProof>,
    message: &crate::channels::traits::ChannelMessage,
    routed: &str,
    resume: bool,
) -> anyhow::Result<Option<crate::runtime::h2_entry::TerminalInputProof>> {
    origin
        .map(|proof| {
            if resume {
                proof.resume_request(&message.id, &message.content, routed)
            } else {
                anyhow::ensure!(
                    proof.matches_message(&message.id, routed),
                    "terminal input changed before source admission"
                );
                Ok(proof)
            }
        })
        .transpose()
}
