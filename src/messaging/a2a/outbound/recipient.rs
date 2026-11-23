//! Resolve receiving owners through the authenticated pin route, never tool JSON.
use super::{RemoteTransport, bounded_response};
use crate::messaging::{
    RecipientPin, SessionAddr,
    mailbox::{MailboxAuthority, MailboxError, MailboxResult},
    runtime::MessagingRuntime,
    send::MailboxDestination,
};

#[derive(serde::Deserialize)]
struct PinResponse {
    to: SessionAddr,
    pin: RecipientPin,
    expires_at: i64,
}
impl RemoteTransport {
    pub(crate) async fn destination(
        &self,
        runtime: &MessagingRuntime,
        source: &SessionAddr,
        target: &SessionAddr,
    ) -> MailboxResult<MailboxDestination> {
        self.resolve_destination(runtime, source, target)
            .await
            .inspect_err(|error| {
                let reason = match error {
                    MailboxError::PeerUnknown => "peer_unknown",
                    MailboxError::PeerNotAllowed => "peer_not_allowed",
                    MailboxError::OwnerMismatch => "owner_mismatch",
                    MailboxError::RemoteNotEnabled => "remote_not_enabled",
                    MailboxError::PeerUnavailable => "peer_unavailable",
                    _ => "peer_resolution_failed",
                };
                tracing::warn!(reason, "A2A recipient resolution refused");
            })
    }
    async fn resolve_destination(
        &self,
        runtime: &MessagingRuntime,
        source: &SessionAddr,
        target: &SessionAddr,
    ) -> MailboxResult<MailboxDestination> {
        self.trust
            .refresh_async()
            .await
            .map_err(|_| MailboxError::RemoteNotEnabled)?;
        if !self
            .trust
            .effective()
            .map_err(|_| MailboxError::RemoteNotEnabled)?
            .a2a
            .messaging_enabled
        {
            return Err(MailboxError::RemoteNotEnabled);
        }
        let peer = self.peer(target).map_err(|_| MailboxError::PeerUnknown)?;
        if peer.owner != source.owner {
            return Err(MailboxError::PeerNotAllowed);
        }
        // The public address defaults to the local owner. The remote owner is
        // learned only from the receiver authenticated by our current peer pin.
        if target.owner != source.owner {
            return Err(MailboxError::OwnerMismatch);
        }
        let clients = self.clients.clone();
        let selected = peer.clone();
        let (client, cancellation) = crate::runtime::blocking::spawn_blocking(move || clients.get(&selected))
            .await
            .map_err(|_| MailboxError::Storage)?
            .map_err(|_| MailboxError::PeerNotAllowed)?;
        let request = client
            .post(format!("https://{}/a2a/v1/pins", peer.address))
            .json(&serde_json::json!({"to":{"instance":target.instance,"session":target.session,"agent":target.agent}}))
            .send();
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(MailboxError::PeerUnavailable),
            response = request => response,
        }
        .inspect_err(|error| {
            let (reason, os_code) = transport_reason(error);
            tracing::warn!(reason, os_code, "A2A pin request failed");
        });
        let pin = match response {
            Ok(response) => {
                if response.status().as_u16() != 200 {
                    return Err(MailboxError::PeerNotAllowed);
                }
                serde_json::from_slice::<PinResponse>(
                    &bounded_response(response)
                        .await
                        .map_err(|_| MailboxError::PeerUnavailable)?,
                )
                .map_err(|_| MailboxError::InvalidPin)?
            }
            Err(error) if offline(&error) => {
                let store = runtime.store.clone();
                let (source, target) = (source.clone(), target.clone());
                let workspace = runtime.workspace.clone();
                let to = crate::runtime::blocking::spawn_blocking(move || {
                    store.known_remote_target(&workspace, &source, &target)
                })
                .await
                .map_err(|_| MailboxError::Storage)??
                .ok_or(MailboxError::PeerUnavailable)?;
                PinResponse {
                    to,
                    pin: RecipientPin {
                        incarnation: uuid::Uuid::nil(),
                        generation: 1,
                        token: "pending-peer-pin".into(),
                    },
                    expires_at: chrono::Utc::now().timestamp() + 86400,
                }
            }
            Err(_) => return Err(MailboxError::PeerNotAllowed),
        };
        if pin.to.instance != target.instance || pin.to.session != target.session || pin.to.agent != target.agent {
            return Err(MailboxError::InvalidPin);
        }
        crate::messaging::mailbox::validate_address(&pin.to)?;
        self.trust
            .refresh_async()
            .await
            .map_err(|_| MailboxError::RemoteNotEnabled)?;
        let current = self.peer(target).map_err(|_| MailboxError::PeerUnknown)?;
        if current != peer {
            return Err(MailboxError::PeerNotAllowed);
        }
        Ok(MailboxDestination {
            authority: MailboxAuthority::outbound(runtime.workspace.clone(), source.clone(), pin.to)?,
            pin: pin.pin,
            pin_expires_at: pin.expires_at,
        })
    }
}
fn transport_reason(error: &reqwest::Error) -> (&'static str, Option<i32>) {
    use std::error::Error as _;
    let mut cause = error.source();
    while let Some(error) = cause {
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            let reason = match error.kind() {
                std::io::ErrorKind::ConnectionRefused => "peer_connection_refused",
                std::io::ErrorKind::TimedOut => "peer_timeout",
                std::io::ErrorKind::NetworkUnreachable => "peer_network_unreachable",
                std::io::ErrorKind::HostUnreachable => "peer_host_unreachable",
                std::io::ErrorKind::PermissionDenied => "peer_network_permission_denied",
                _ => "peer_io_failed",
            };
            return (reason, error.raw_os_error());
        }
        cause = error.source();
    }
    (
        if error.is_timeout() {
            "peer_timeout"
        } else {
            "peer_request_failed"
        },
        None,
    )
}
fn offline(error: &reqwest::Error) -> bool {
    use std::error::Error as _;
    let mut cause = error.source();
    while let Some(error) = cause {
        if let Some(error) = error.downcast_ref::<std::io::Error>() {
            return matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
            );
        }
        cause = error.source();
    }
    error.is_timeout()
}
