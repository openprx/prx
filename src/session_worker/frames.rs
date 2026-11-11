//! Line framing shared by the real parent process and dormant mailbox runtime.
//! Terminal bytes are preserved verbatim, including authenticated startup proof.
use super::protocol::{WorkerControlFrame, WorkerOutputFrame};
use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Explicitly supplied by the dormant mailbox runtime; no factory installs it.
pub struct WorkerControlQueue {
    pub receiver: mpsc::Receiver<WorkerControlFrame>,
    /// Drops after the receiver and all queued payloads.
    pub owner: std::sync::Arc<dyn Send + Sync>,
}

pub struct WorkerFrameChannels {
    pub controls: WorkerControlQueue,
    pub outputs: mpsc::Sender<WorkerOutputFrame>,
}

pub async fn write_control_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &WorkerControlFrame) -> Result<()> {
    let bytes = serde_json::to_vec(frame).context("serialize worker control frame")?;
    writer.write_all(&bytes).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// One owner serializes all control kinds; concurrent producers never interleave
/// JSON bytes. Queue capacity belongs to the caller's bounded runtime channel.
pub async fn write_control_frames<W>(mut writer: W, mut frames: mpsc::Receiver<WorkerControlFrame>) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = frames.recv().await {
        write_control_frame(&mut writer, &frame).await?;
    }
    Ok(())
}

/// Drain frames as soon as a line arrives; only return the terminal result to
/// the existing completion verifier. EOF is still required before relinquishing
/// descendant pipe ownership. No extra consumer reads this stdout stream.
pub async fn read_worker_frames<R>(
    reader: R,
    frames: Option<mpsc::Sender<WorkerOutputFrame>>,
    progress: Option<std::sync::Arc<crate::agent::idle::ProgressBeat>>,
    limit: u64,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let reader = ProgressReader {
        inner: reader,
        progress,
    };
    let mut reader = BufReader::new(reader);
    let mut terminal = None;
    loop {
        if terminal.is_some() {
            // Keep pipe ownership until EOF without retaining later output.
            tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
            break;
        }
        let mut line = Vec::new();
        let count = (&mut reader)
            .take(limit.saturating_add(1))
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 {
            break;
        }
        ensure!(
            u64::try_from(count)? <= limit,
            "session-worker stdout line exceeded byte limit"
        );
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&line).context("invalid worker stdout JSON line")?;
        if value.get("kind").is_some() {
            let frame: WorkerOutputFrame = serde_json::from_value(value).context("invalid worker output frame")?;
            if let Some(frames) = &frames {
                frames.send(frame).await.context("worker frame receiver closed")?;
            } else {
                anyhow::bail!("session-worker emitted a frame without a registered runtime receiver");
            }
        } else {
            // WorkerResult has no top-level run_id in the existing protocol.
            // Do not deserialize/re-encode here: the verifier owns its proof.
            let _: super::startup_failure::WorkerOutput =
                serde_json::from_value(value).context("invalid worker terminal result")?;
            terminal = Some(line);
        }
    }
    // An externally killed worker may close stdout without writing anything.
    // Preserve that empty result for the owner's exit/signal classifier.
    Ok(terminal.unwrap_or_default())
}

struct ProgressReader<R> {
    inner: R,
    progress: Option<std::sync::Arc<crate::agent::idle::ProgressBeat>>,
}
impl<R: AsyncRead + Unpin> AsyncRead for ProgressReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = std::pin::Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(&result, std::task::Poll::Ready(Ok(())))
            && buf.filled().len() > before
            && let Some(progress) = &this.progress
        {
            progress.record(crate::agent::idle::ProgressKind::ChannelOutput);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::DeliveryState;
    use uuid::Uuid;

    #[tokio::test]
    async fn stdout_frames_arrive_before_eof_and_terminal_proof_bytes_are_preserved() -> Result<()> {
        let (mut worker, parent) = tokio::io::duplex(4096);
        let (frames_tx, mut frames_rx) = mpsc::channel(4);
        let reader = tokio::spawn(read_worker_frames(parent, Some(frames_tx), None, 4096));
        let ack = WorkerOutputFrame::Ack {
            message_id: Uuid::from_u128(1),
            state: DeliveryState::Delivered,
        };
        worker.write_all(&serde_json::to_vec(&ack)?).await?;
        worker.write_all(b"\n").await?;
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), frames_rx.recv()).await?;
        assert_eq!(received, Some(ack));
        assert!(
            !reader.is_finished(),
            "frames must arrive while the worker still owns stdout"
        );
        let terminal = serde_json::json!({"success":false,"output":"","error":"initial source changed","tokens_used":null,"authenticated_startup_failure":{"phase":"initial-config-source","run_id":"prx-run","manifest_digest":vec![0;32],"seal":vec![1;32]}});
        // The wire is one JSON object per line; retain arbitrary legal spaces.
        let mut bytes = serde_json::to_vec(&terminal)?;
        bytes.extend_from_slice(b" \n");
        worker.write_all(&bytes).await?;
        worker.write_all(b"ignored after terminal\n").await?;
        worker
            .write_all(&serde_json::to_vec(&WorkerOutputFrame::BrokerCancel { call_id: 9 })?)
            .await?;
        worker.write_all(b"\n").await?;
        drop(worker);
        assert_eq!(reader.await??, bytes);
        Ok(())
    }

    #[tokio::test]
    async fn multiplexed_control_writer_preserves_each_frame() -> Result<()> {
        let (parent, worker) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel(4);
        let pump = tokio::spawn(write_control_frames(parent, rx));
        let first = WorkerControlFrame::Steer {
            message: "operator steering".into(),
        };
        let second = WorkerControlFrame::BrokerCancelAck { call_id: 1 };
        tx.send(first.clone()).await?;
        let other = tx.clone();
        tx.send(second.clone()).await?;
        drop(tx);
        drop(other);
        let mut lines = BufReader::new(worker).lines();
        let first_line = lines.next_line().await?.context("first frame missing")?;
        let second_line = lines.next_line().await?.context("second frame missing")?;
        assert_eq!(serde_json::from_str::<WorkerControlFrame>(&first_line)?, first);
        assert_eq!(serde_json::from_str::<WorkerControlFrame>(&second_line)?, second);
        assert!(lines.next_line().await?.is_none());
        pump.await??;
        Ok(())
    }

    #[tokio::test]
    async fn empty_stdout_remains_available_to_exit_signal_classifier() -> Result<()> {
        let bytes = read_worker_frames(tokio::io::empty(), None, None, 4096).await?;
        assert_eq!(bytes, Vec::<u8>::new());
        Ok(())
    }

    #[tokio::test]
    async fn live_frames_have_a_per_line_limit_instead_of_a_cumulative_budget() -> Result<()> {
        let (mut worker, parent) = tokio::io::duplex(4096);
        let (sender, mut receiver) = mpsc::channel(32);
        let reader = tokio::spawn(read_worker_frames(parent, Some(sender), None, 256));
        let frame = WorkerOutputFrame::Ack {
            message_id: Uuid::from_u128(1),
            state: DeliveryState::Delivered,
        };
        let mut wire = serde_json::to_vec(&frame)?;
        wire.push(b'\n');
        assert!(wire.len() < 256);
        for _ in 0..16 {
            worker.write_all(&wire).await?;
        }
        let terminal = b"{\"success\":true,\"output\":\"done\",\"error\":null,\"tokens_used\":null}\n";
        worker.write_all(terminal).await?;
        drop(worker);
        assert_eq!(reader.await??, terminal);
        let mut received = 0;
        while let Some(actual) = receiver.recv().await {
            assert_eq!(actual, frame);
            received += 1;
        }
        assert_eq!(received, 16);
        Ok(())
    }
}
