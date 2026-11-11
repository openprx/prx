//! Dormant /msg parser. Deliberately absent from command dispatch/help.
use crate::messaging::{
    SessionAddr,
    mailbox::{MailboxError, MailboxResult},
    send::{MailboxSender, parse_target},
};
use serde_json::json;

/// Parse the target without ever deriving the authenticated sender from text.
pub fn parse(input: &str, caller: &SessionAddr) -> MailboxResult<(SessionAddr, String)> {
    let rest = input
        .strip_prefix("/msg ")
        .ok_or(MailboxError::InvalidMetadata)?
        .trim_start();
    let (peer, rest) = rest
        .split_once(char::is_whitespace)
        .ok_or(MailboxError::InvalidMetadata)?;
    let (session, text) = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .ok_or(MailboxError::InvalidMetadata)?;
    if text.trim().is_empty() {
        return Err(MailboxError::InvalidMetadata);
    }
    let (session, agent) = session.split_once('/').unwrap_or((session, "main"));
    let target = parse_target(
        &json!({"instance":if peer=="local" {&caller.instance} else {peer},"session":session,"agent":agent}),
        caller,
    )?;
    Ok((target, text.trim_start().to_owned()))
}
pub async fn send(
    input: &str,
    sender: &dyn MailboxSender,
) -> MailboxResult<crate::messaging::mailbox::MailboxPublished> {
    let (target, text) = parse(input, sender.source())?;
    sender.send(target, text, "text/plain".to_owned()).await
}
