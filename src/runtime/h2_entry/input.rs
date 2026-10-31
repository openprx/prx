//! The shared FIFO carries origin as an opaque value, never message metadata.
use super::{TerminalInputProducer, TerminalInputProof};
use crate::channels::traits::ChannelMessage;
use tokio::sync::mpsc;

/// Bounded polling on the original foreground descriptor. No read_line task
/// survives cancellation or borrows a reloadable input file.
#[cfg(unix)]
pub(crate) fn poll_original_terminal() -> anyhow::Result<Option<Vec<u8>>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    // The existing safe event reader borrows stdin when it is a TTY. Launch
    // already verifies that condition. Poll and read stay on the sole reader
    // thread, so no other consumer can steal the event after readiness.
    if !event::poll(std::time::Duration::from_millis(100))? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Vec::new()),
            KeyCode::Char(character) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                let mut bytes = [0_u8; 4];
                Some(character.encode_utf8(&mut bytes).as_bytes().to_vec())
            }
            KeyCode::Enter => Some(vec![b'\n']),
            KeyCode::Tab => Some(vec![b'\t']),
            KeyCode::Backspace => Some(vec![8]),
            _ => None,
        },
        Event::Paste(text) => {
            anyhow::ensure!(
                text.len() <= 256 * 1024,
                "original terminal paste exceeds its input bound"
            );
            Some(text.into_bytes())
        }
        _ => None,
    })
}
#[cfg(not(unix))]
pub(crate) fn poll_original_terminal() -> anyhow::Result<Option<Vec<u8>>> {
    anyhow::bail!("controlled launch requires the supported Unix foreground terminal profile")
}
/// Decode the existing terminal backspace event without splitting UTF-8.
pub(crate) fn erase_terminal_character(line: &mut Vec<u8>) {
    while let Some(byte) = line.pop() {
        if byte & 0b1100_0000 != 0b1000_0000 {
            break;
        }
    }
}

#[derive(Debug)]
pub(crate) struct ChatInput {
    pub(crate) message: ChannelMessage,
    pub(crate) terminal: Option<TerminalInputProof>,
}
impl From<ChannelMessage> for ChatInput {
    fn from(message: ChannelMessage) -> Self {
        Self {
            message,
            terminal: None,
        }
    }
}
#[derive(Clone)]
pub(crate) struct ChatInputSender(Sender);
#[derive(Clone)]
enum Sender {
    Owned(mpsc::Sender<ChatInput>),
    Legacy(mpsc::Sender<ChannelMessage>),
}
impl From<mpsc::Sender<ChannelMessage>> for ChatInputSender {
    fn from(sender: mpsc::Sender<ChannelMessage>) -> Self {
        Self(Sender::Legacy(sender))
    }
}
impl ChatInputSender {
    pub(crate) fn channel(capacity: usize) -> (Self, mpsc::Receiver<ChatInput>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self(Sender::Owned(tx)), rx)
    }
    pub(crate) async fn send(&self, message: ChannelMessage) -> Result<(), ()> {
        match &self.0 {
            Sender::Owned(sender) => sender.send(ChatInput::from(message)).await.map_err(|_| ()),
            Sender::Legacy(sender) => sender.send(message).await.map_err(|_| ()),
        }
    }
    pub(crate) fn blocking_send(&self, message: ChannelMessage) -> Result<(), ()> {
        match &self.0 {
            Sender::Owned(sender) => sender.blocking_send(ChatInput::from(message)).map_err(|_| ()),
            Sender::Legacy(sender) => sender.blocking_send(message).map_err(|_| ()),
        }
    }
    /// Only an OS reader owns `producer`; ordinary clones can send no proof.
    pub(crate) fn submit_terminal(
        &self,
        producer: &mut TerminalInputProducer,
        message: ChannelMessage,
    ) -> anyhow::Result<()> {
        let proof = producer.submitted(&message.id, &message.content)?;
        let Sender::Owned(sender) = &self.0 else {
            anyhow::bail!("terminal provenance requires the owned input FIFO")
        };
        let mut input = ChatInput {
            message,
            terminal: Some(proof),
        };
        loop {
            producer.profile().ensure_live()?;
            match sender.try_send(input) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => anyhow::bail!("terminal input owner ended"),
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    input = returned;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }
}

/// The concrete terminal listener selects one transport once. Generic channel
/// listeners cannot obtain a producer from sender/channel strings.
pub(crate) struct TerminalInputSink {
    sender: ChatInputSender,
    producer: Option<TerminalInputProducer>,
}
impl TerminalInputSink {
    pub(crate) fn ordinary(sender: impl Into<ChatInputSender>) -> Self {
        Self {
            sender: sender.into(),
            producer: None,
        }
    }
    pub(crate) fn controlled(sender: ChatInputSender, producer: TerminalInputProducer) -> Self {
        Self {
            sender,
            producer: Some(producer),
        }
    }
    pub(crate) fn blocking_send(&mut self, message: ChannelMessage) -> anyhow::Result<()> {
        match &mut self.producer {
            Some(producer) => self.sender.submit_terminal(producer, message),
            None => self
                .sender
                .blocking_send(message)
                .map_err(|()| anyhow::anyhow!("terminal input owner ended")),
        }
    }
}
