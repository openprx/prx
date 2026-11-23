//! Durable remote publication and receiver-pin refresh before every attempt.
use super::{peers, tls::Trust};
use crate::messaging::{
    DeliveryState, RecipientPin, SessionAddr,
    mailbox::{MailboxAuthority, MailboxError, MailboxResult},
    runtime::MessagingRuntime,
    send::MailboxDestination,
};
use std::{collections::HashSet, sync::Arc, time::Duration};

pub(crate) struct RemoteTransport {
    trust: Trust,
    clients: super::clients::Clients,
}
impl RemoteTransport {
    pub(crate) fn new(config: &crate::config::Config) -> Self {
        let trust = Trust {
            config: Arc::new(config.clone()),
        };
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
            .find(|peer| peers::instance(&peer.spiffe_id).is_ok_and(|id| id.to_string() == target.instance))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("peer_not_allowed"))
    }
    pub(crate) fn destination(
        &self,
        runtime: &MessagingRuntime,
        source: &SessionAddr,
        target: &SessionAddr,
    ) -> MailboxResult<MailboxDestination> {
        self.peer(target).map_err(|_| MailboxError::RemoteNotEnabled)?;
        Ok(MailboxDestination {
            authority: MailboxAuthority::outbound(runtime.workspace.clone(), source.clone(), target.clone())?,
            pin: RecipientPin {
                incarnation: uuid::Uuid::nil(),
                generation: 1,
                token: "pending-peer-pin".into(),
            },
            pin_expires_at: chrono::Utc::now().timestamp() + 86400,
        })
    }
    async fn attempt(
        &self,
        runtime: &MessagingRuntime,
        scope: &crate::messaging::mailbox::MailboxScope,
        source: &SessionAddr,
        message: &mut crate::messaging::PrxMessage,
    ) -> anyhow::Result<(u16, Option<u64>)> {
        let peer = self.peer(&message.to).map_err(|_| MailboxError::Unauthorized)?;
        let (client, cancellation) = self.clients.get(&peer)?;
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
                pin: RecipientPin,
                expires_at: i64,
            }
            let pin: Pin = serde_json::from_slice(&bounded_response(response).await?)?;
            runtime
                .store
                .refresh_outbox_pin(scope, source, message, pin.pin, pin.expires_at)?;
            // Re-read grants between the two requests; no cached bearer authority.
            let current = self.peer(&message.to)?;
            anyhow::ensure!(
                current.spiffe_id == peer.spiffe_id
                    && current.fingerprints == peer.fingerprints
                    && current.address == peer.address,
                "peer changed"
            );
            let response = client
                .post(format!("https://{}/a2a/v1/messages", peer.address))
                .json(message)
                .send()
                .await?;
            Ok((response.status().as_u16(), retry_after(&response)))
        };
        tokio::select! {
            ()=cancellation.cancelled()=>Err(MailboxError::Unauthorized.into()),
            result=request=>result,
        }
    }
    pub(crate) fn start(self, runtime: Arc<MessagingRuntime>) {
        tokio::spawn(async move {
            let _monitor = self.clients.monitor();
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                let Ok(sources) = runtime.store.outbox_sources() else {
                    continue;
                };
                let mut peers = HashSet::new();
                for (scope, source) in sources {
                    let Ok(pending) = runtime.store.pending_outbox(&scope, &source) else {
                        continue;
                    };
                    for outbound in pending {
                        let mut message = outbound.message().clone();
                        if message.to.instance == runtime.instance || !peers.insert(message.to.instance.clone()) {
                            continue;
                        }
                        let Ok((attempts, created)) = runtime.store.outbox_retry(&scope, &message) else {
                            continue;
                        };
                        let now = chrono::Utc::now().timestamp();
                        if now.saturating_sub(created) >= 86400 {
                            let _ = runtime.store.retry_outbox(&scope, &source, &message, None, "expired");
                            continue;
                        }
                        let result = self.attempt(&runtime, &scope, &source, &mut message).await;
                        let (status, retry) = match result {
                            Ok(value) => value,
                            Err(error)
                                if error.downcast_ref::<MailboxError>() == Some(&MailboxError::StaleIncarnation)
                                    || error.downcast_ref::<MailboxError>() == Some(&MailboxError::Unauthorized) =>
                            {
                                (403, None)
                            }
                            Err(_) => (0, None),
                        };
                        if let Ok(peer) = self.peer(&message.to) {
                            self.clients
                                .state(&peer, if status == 202 { "available" } else { "unavailable" });
                        }
                        if status == 202 {
                            let _ = runtime
                                .store
                                .acknowledge(&scope, &message.to, message.id, DeliveryState::Accepted);
                            continue;
                        }
                        let permanent =
                            matches!(status, 401 | 403 | 413 | 422) || (400..500).contains(&status) && status != 429;
                        let delay = backoff(attempts).max(retry.unwrap_or(0));
                        let next = if permanent {
                            None
                        } else {
                            Some(now.saturating_add(i64::try_from(delay).unwrap_or(300)))
                        };
                        let error = match status {
                            401 => "peer_unauthenticated",
                            403 => "peer_not_allowed",
                            413 => "message_too_large",
                            422 => "chain_limit_reached",
                            429 => "inbox_full",
                            _ => "peer_unavailable",
                        };
                        let _ = runtime.store.retry_outbox(&scope, &source, &message, next, error);
                    }
                }
            }
        });
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
