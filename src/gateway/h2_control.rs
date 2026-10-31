//! Direct OS launch only. No reload file or network endpoint mints authority.
use super::*;
use crate::runtime::h2_entry::{EntryKind, EntryLimits, TerminalInputProducer};

#[allow(clippy::too_many_arguments)]
pub async fn run_gateway_controlled(
    host: &str,
    port: u16,
    config: Config,
    shutdown: CancellationToken,
    principal: String,
    limits_json: String,
    authority_seconds: u64,
) -> Result<()> {
    anyhow::ensure!(
        limits_json.len() <= 256 * 1024,
        "controlled launch intent exceeds its bound"
    );
    let limits: EntryLimits =
        serde_json::from_str(&limits_json).map_err(|_| anyhow::anyhow!("invalid explicit experiment ceilings"))?;
    let expires = chrono::Utc::now()
        .checked_add_signed(
            chrono::Duration::try_seconds(i64::try_from(authority_seconds)?)
                .ok_or_else(|| anyhow::anyhow!("controlled authority duration exceeds its supported range"))?,
        )
        .context("controlled authority expiry overflow")?;
    super::api::verify_startup_identity(&config, &principal).await?;
    let producer = TerminalInputProducer::open(&config, EntryKind::Gateway, Some(principal), expires, limits)?;
    producer.confirm_local_launch(&shutdown).await?;
    let profile = producer.profile();
    let result = super::run_gateway_inner(host, port, config, None, shutdown, Some(producer)).await;
    let cleanup = profile.close_reader().await;
    cleanup?;
    result
}

pub(super) async fn start_local_control(
    producer: TerminalInputProducer,
    state: AppState,
    shutdown: CancellationToken,
) -> Result<()> {
    let authority = producer.into_local_control()?;
    let profile = Arc::clone(authority.profile());
    let runtime = state.pin_turn_runtime();
    let service = Arc::clone(runtime.decision_slot.publish(&runtime, &state).await?);
    let handle = tokio::runtime::Handle::current();
    println!(
        "Controlled local review: show SESSION PENDING; approve SESSION PENDING DIGEST; deny SESSION PENDING DIGEST; revoke SESSION PENDING; exit"
    );
    let thread = std::thread::Builder::new()
        .name("prx-h2-local-review".into())
        .spawn(move || {
            let _lifetime = crate::runtime::h2_entry::ControlledReaderLifetime::new(
                Some(Arc::clone(authority.profile())),
                shutdown.clone(),
            );
            let result = (|| -> Result<()> {
                let mut line = Vec::new();
                while !shutdown.is_cancelled() && authority.profile().ensure_live().is_ok() {
                    let Some(bytes) = crate::runtime::h2_entry::poll_original_terminal()? else {
                        continue;
                    };
                    if bytes.is_empty() {
                        break;
                    }
                    for byte in bytes {
                        if matches!(byte, 8 | 127) {
                            crate::runtime::h2_entry::erase_terminal_character(&mut line);
                            continue;
                        }
                        if byte == b'\n' {
                            let text = std::str::from_utf8(&line).context("local review input is not UTF-8")?;
                            let fields: Vec<_> = text.split_whitespace().collect();
                            let outcome = handle.block_on(async {
                                use crate::causal_tree::ids::DecisionId;
                                match fields.as_slice() {
                                    ["show", session, id] => {
                                        let view = service
                                            .inspect_pending_decision_approval(
                                                authority.principal(),
                                                session,
                                                DecisionId::parse(id)?,
                                            )
                                            .await?;
                                        Ok(serde_json::to_string(&view)?)
                                    }
                                    [verb @ ("approve" | "deny"), session, id, digest] => {
                                        let receipt = service
                                            .review_h2_local(
                                                &authority,
                                                session,
                                                DecisionId::parse(id)?,
                                                digest,
                                                *verb == "approve",
                                            )
                                            .await?;
                                        Ok(serde_json::to_string(&receipt)?)
                                    }
                                    ["revoke", session, id] => {
                                        authority.profile().ensure_live()?;
                                        let receipt = service
                                            .revoke_pending_decision_approval(
                                                authority.principal(),
                                                session,
                                                DecisionId::parse(id)?,
                                            )
                                            .await?;
                                        Ok(serde_json::to_string(&receipt)?)
                                    }
                                    ["exit"] => {
                                        authority.profile().revoke();
                                        shutdown.cancel();
                                        Ok("local authority revoked".into())
                                    }
                                    _ => anyhow::bail!("invalid bounded local-review command"),
                                }
                            });
                            match outcome {
                                Ok(output) => println!("{output}"),
                                Err(error) => eprintln!("local review refused: {error}"),
                            }
                            line.clear();
                        } else if byte != b'\r' {
                            anyhow::ensure!(line.len() < 256 * 1024, "local review line exceeds its bound");
                            line.push(byte);
                        }
                    }
                }
                Ok(())
            })();
            authority.profile().revoke();
            shutdown.cancel();
            if let Err(error) = result {
                tracing::error!(%error,"local review reader ended without confirmed input cleanup");
            }
        })?;
    profile.own_reader(thread)
}
