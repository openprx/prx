//! Durable remote publication and receiver-pin refresh before every attempt.
mod recipient;
use super::tls::Trust;
use crate::messaging::{DeliveryState, RecipientPin, SessionAddr, mailbox::MailboxError, runtime::MessagingRuntime};
use std::{sync::Arc, time::Duration};

pub(crate) struct RemoteTransport {
    trust: Trust,
    clients: super::clients::Clients,
}
impl RemoteTransport {
    pub(crate) fn new(config: &crate::config::Config) -> Self {
        let trust = Trust::new(config);
        Self {
            clients: super::clients::Clients::new(trust.clone()),
            trust,
        }
    }
    fn peer(&self, target: &SessionAddr) -> anyhow::Result<crate::config::A2aPeer> {
        let config = self.trust.current()?;
        config
            .peers
            .values()
            .find(|peer| peer.spiffe_id == target.instance)
            .cloned()
            .ok_or_else(|| {
                crate::messaging::mailbox::unauthorized(crate::messaging::mailbox::UnauthorizedReason::PeerAbsent)
                    .into()
            })
    }
    async fn attempt(
        &self,
        runtime: &MessagingRuntime,
        scope: &crate::messaging::mailbox::MailboxScope,
        source: &SessionAddr,
        message: &mut crate::messaging::PrxMessage,
    ) -> anyhow::Result<(u16, Option<u64>)> {
        self.trust.refresh_async().await?;
        let peer = self.peer(&message.to)?;
        anyhow::ensure!(peer.owner == source.owner, MailboxError::PeerNotAllowed);
        let clients = self.clients.clone();
        let selected = peer.clone();
        let (client, cancellation) = crate::runtime::blocking::spawn_blocking(move || clients.get(&selected)).await??;
        let request = async {
            let response = client
                .post(format!("https://{}/a2a/v1/pins", peer.address))
                .json(&serde_json::json!({"to":message.to}))
                .send()
                .await?;
            let status = response.status().as_u16();
            let retry = retry_after(&response);
            if status != 200 {
                return Ok((status, retry));
            }
            #[derive(serde::Deserialize)]
            struct Pin {
                to: SessionAddr,
                pin: RecipientPin,
                expires_at: i64,
            }
            let pin: Pin = serde_json::from_slice(&bounded_response(response).await?)?;
            anyhow::ensure!(pin.to == message.to, MailboxError::OwnerMismatch);
            let store = runtime.store.clone();
            let scope = scope.clone();
            let source = source.clone();
            let mut refreshed = message.clone();
            *message = crate::runtime::blocking::spawn_blocking(move || {
                store.refresh_outbox_pin(&scope, &source, &mut refreshed, pin.pin, pin.expires_at)?;
                Ok::<_, MailboxError>(refreshed)
            })
            .await??;
            self.trust.refresh_async().await?;
            // Re-read grants between the two requests; no cached bearer authority.
            let current = self.peer(&message.to)?;
            anyhow::ensure!(
                current.spiffe_id == peer.spiffe_id
                    && current.fingerprints == peer.fingerprints
                    && current.address == peer.address,
                AttemptInterrupted
            );
            let response = client
                .post(format!("https://{}/a2a/v1/messages", peer.address))
                .json(message)
                .send()
                .await?;
            Ok((response.status().as_u16(), retry_after(&response)))
        };
        tokio::select! {
            ()=cancellation.cancelled()=>Err(AttemptInterrupted.into()),
            result=request=>result,
        }
    }
    pub(crate) fn start(self, runtime: Arc<MessagingRuntime>) -> anyhow::Result<()> {
        let lease = Arc::new(super::leader::open(&self.trust.config.workspace_dir)?);
        let transport = Arc::new(self);
        tokio::spawn(async move {
            let _monitor = transport.clients.monitor();
            loop {
                let (transport, runtime, lease) = (Arc::clone(&transport), Arc::clone(&runtime), Arc::clone(&lease));
                let result = tokio::spawn(super::dispatch::run(transport, runtime, lease)).await;
                match result {
                    Ok(Err(error)) => {
                        tracing::error!(error=%crate::agent::diagnostics::error_class(&error), "A2A outbox dispatcher exited; restarting")
                    }
                    _ => tracing::error!("A2A outbox dispatcher exited; restarting"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        Ok(())
    }
    pub(super) async fn deliver(
        &self,
        runtime: Arc<MessagingRuntime>,
        pending: super::dispatch::Pending,
    ) -> anyhow::Result<()> {
        let super::dispatch::Pending {
            scope,
            source,
            mut message,
            attempts,
        } = pending;
        let (status, retry) = match self.attempt(&runtime, &scope, &source, &mut message).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    reason = attempt_reason(&error),
                    attempt = attempts.saturating_add(1),
                    "A2A outbound attempt failed"
                );
                if super::dispatch::permanent(&error) {
                    (403, None)
                } else {
                    (0, None)
                }
            }
        };
        if status != 202 && status != 0 {
            tracing::warn!(
                reason = status_reason(status),
                status,
                attempt = attempts.saturating_add(1),
                "A2A outbound attempt refused"
            );
        }
        if let Ok(peer) = self.peer(&message.to) {
            self.clients
                .state(&peer, if status == 202 { "available" } else { "unavailable" });
        }
        let store = runtime.store.clone();
        crate::runtime::blocking::spawn_blocking(move || {
            if status == 202 {
                store.acknowledge(&scope, &message.to, message.id, DeliveryState::Accepted)?;
            } else {
                let permanent = (400..500).contains(&status) && !matches!(status, 408 | 429);
                let delay = backoff(attempts).max(retry.unwrap_or(0));
                let next = if permanent {
                    None
                } else {
                    Some(
                        chrono::Utc::now()
                            .timestamp()
                            .saturating_add(i64::try_from(delay).unwrap_or(300)),
                    )
                };
                let error = status_reason(status);
                store.retry_outbox(&scope, &source, &message, next, error)?;
            }
            Ok::<_, MailboxError>(())
        })
        .await??;
        Ok(())
    }
}
fn retry_after(response: &reqwest::Response) -> Option<u64> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    value.parse().ok().or_else(|| {
        chrono::DateTime::parse_from_rfc2822(value)
            .ok()
            .and_then(|date| u64::try_from(date.timestamp().saturating_sub(chrono::Utc::now().timestamp())).ok())
    })
}
async fn bounded_response(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= 8192,
            "peer pin response exceeds byte bound"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn backoff(attempts: u32) -> u64 {
    let ceiling = 1_u64.checked_shl(attempts.min(9)).unwrap_or(300).min(300);
    rand::random_range(0..=ceiling)
}

#[derive(Debug, thiserror::Error)]
#[error("peer configuration changed during delivery")]
struct AttemptInterrupted;
fn attempt_reason(error: &anyhow::Error) -> &'static str {
    if error.is::<AttemptInterrupted>() {
        return "peer_configuration_changed";
    }
    if let Some(error) = error.downcast_ref::<reqwest::Error>() {
        return if error.is_timeout() {
            "peer_timeout"
        } else if error.is_connect() {
            "peer_connect_failed"
        } else {
            "peer_request_failed"
        };
    }
    if let Some(error) = error.downcast_ref::<MailboxError>() {
        return match error {
            MailboxError::Unauthorized => "peer_not_allowed",
            MailboxError::StaleIncarnation => "stale_incarnation",
            _ => "mailbox_attempt_failed",
        };
    }
    if error.is::<serde_json::Error>() {
        return "peer_response_invalid";
    }
    "peer_attempt_failed"
}
const fn status_reason(status: u16) -> &'static str {
    match status {
        400 => "field_invalid",
        401 => "peer_unauthenticated",
        403 => "peer_not_allowed",
        408 => "peer_timeout",
        413 => "message_too_large",
        422 => "chain_limit_reached",
        429 => "inbox_full",
        _ => "peer_unavailable",
    }
}
