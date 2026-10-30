//! Transport boundary checks; no lesson policy or durable store is mocked in.
use super::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum Preparation {
    Allow,
    Refuse,
    Pending,
}

struct Signals {
    preparation: Preparation,
    deadline: Instant,
    admitted: AtomicUsize,
    settled: AtomicUsize,
    prepared: Mutex<Vec<Vec<u8>>>,
    entered: tokio::sync::Notify,
}

impl Signals {
    fn new(preparation: Preparation) -> Arc<Self> {
        Arc::new(Self {
            preparation,
            deadline: Instant::now() + Duration::from_secs(5),
            admitted: AtomicUsize::new(0),
            settled: AtomicUsize::new(0),
            prepared: Mutex::new(Vec::new()),
            entered: tokio::sync::Notify::new(),
        })
    }
}

struct TestAccounting(Arc<Signals>);
impl sealed::AccountingOwner for TestAccounting {}

#[async_trait]
impl AttemptAccounting for TestAccounting {
    fn deadline(&self) -> Instant {
        self.0.deadline
    }

    async fn begin(&self, _request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>> {
        self.0.admitted.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Receipt(Arc::clone(&self.0))))
    }
}

struct Receipt(Arc<Signals>);
impl sealed::Receipt for Receipt {}

#[async_trait]
impl AttemptReceipt for Receipt {
    fn deadline(&self) -> Instant {
        self.0.deadline
    }

    async fn prepare_handoff(&mut self, body: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(self.0.admitted.load(Ordering::SeqCst) == 1);
        self.0.prepared.lock().push(body.to_vec());
        self.0.entered.notify_one();
        match self.0.preparation {
            Preparation::Allow => Ok(()),
            Preparation::Refuse => anyhow::bail!("preparation refused"),
            Preparation::Pending => std::future::pending().await,
        }
    }

    async fn settle(self: Box<Self>, _usage: TokenUsage) -> anyhow::Result<()> {
        self.0.settled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct CountedBody<'a>(&'a AtomicUsize);
impl Serialize for CountedBody<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        serde_json::json!({"messages":[{"role":"user", "content":"quoted \"context\", tab\tand newline\n"}]})
            .serialize(serializer)
    }
}

#[tokio::test]
async fn handoff_preparation_borrows_the_single_serialized_native_body() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut wire = Vec::new();
        let body = loop {
            anyhow::ensure!(wire.len() <= 8192, "fixture request exceeds bound");
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await?;
            anyhow::ensure!(read > 0, "fixture request ended early");
            wire.extend_from_slice(
                chunk
                    .get(..read)
                    .ok_or_else(|| anyhow::anyhow!("fixture read exceeds buffer"))?,
            );
            if let Some(end) = wire.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = std::str::from_utf8(
                    wire.get(..end)
                        .ok_or_else(|| anyhow::anyhow!("fixture headers exceed buffer"))?,
                )?;
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .ok_or_else(|| anyhow::anyhow!("fixture content length missing"))?;
                if wire.len() >= end + 4 + length {
                    break wire
                        .get(end + 4..end + 4 + length)
                        .ok_or_else(|| anyhow::anyhow!("fixture body exceeds buffer"))?
                        .to_vec();
                }
            }
        };
        let response = br#"{"usage":{"prompt_tokens":8,"completion_tokens":2}}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .as_bytes(),
            )
            .await?;
        stream.write_all(response).await?;
        Ok::<_, anyhow::Error>(body)
    });
    let owner = Signals::new(Preparation::Allow);
    let serializations = AtomicUsize::new(0);
    let result = ACCOUNTING
        .scope(
            Arc::new(TestAccounting(Arc::clone(&owner))),
            send_json(
                reqwest::Client::new().post(&endpoint),
                "fixture-model",
                &endpoint,
                None,
                &CountedBody(&serializations),
                PhysicalKind::Chat,
            ),
        )
        .await;
    result?;
    let wire = server.await??;
    assert_eq!(serializations.load(Ordering::SeqCst), 1);
    assert_eq!(*owner.prepared.lock(), vec![wire]);
    assert_eq!(owner.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(owner.settled.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn handoff_refusal_is_typed_and_never_connects_after_admission() -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let owner = Signals::new(Preparation::Refuse);
    let result = ACCOUNTING
        .scope(
            Arc::new(TestAccounting(Arc::clone(&owner))),
            send_json(
                reqwest::Client::new().post(&endpoint),
                "fixture-model",
                &endpoint,
                None,
                &serde_json::json!({"messages":[]}),
                PhysicalKind::Chat,
            ),
        )
        .await;
    let error = result
        .err()
        .ok_or_else(|| anyhow::anyhow!("refusal unexpectedly sent"))?;
    assert!(is_local_refusal(&error));
    assert_eq!(owner.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(owner.settled.load(Ordering::SeqCst), 0);
    assert_eq!(owner.prepared.lock().len(), 1);
    assert_eq!(
        listener
            .accept()
            .err()
            .ok_or_else(|| anyhow::anyhow!("fixture unexpectedly connected"))?
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    Ok(())
}

#[tokio::test]
async fn handoff_deadline_refuses_an_unfinished_preparation_before_native_transport() -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let owner = Signals::new(Preparation::Pending);
    let result = ACCOUNTING
        .scope(
            Arc::new(TestAccounting(Arc::clone(&owner))),
            send_json(
                reqwest::Client::new().post(&endpoint),
                "fixture-model",
                &endpoint,
                None,
                &serde_json::json!({"messages":[]}),
                PhysicalKind::Chat,
            ),
        )
        .await;
    let error = result
        .err()
        .ok_or_else(|| anyhow::anyhow!("unfinished preparation unexpectedly sent"))?;
    assert!(is_local_refusal(&error));
    assert_eq!(owner.prepared.lock().len(), 1);
    assert_eq!(owner.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(owner.settled.load(Ordering::SeqCst), 0);
    assert_eq!(
        listener
            .accept()
            .err()
            .ok_or_else(|| anyhow::anyhow!("fixture unexpectedly connected"))?
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    Ok(())
}

#[tokio::test]
async fn dropping_pending_handoff_never_connects_or_zeroes_admitted_usage() -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let owner = Signals::new(Preparation::Pending);
    let work_owner = Arc::clone(&owner);
    let work_endpoint = endpoint.clone();
    let task = tokio::spawn(async move {
        ACCOUNTING
            .scope(
                Arc::new(TestAccounting(work_owner)),
                send_json(
                    reqwest::Client::new().post(&work_endpoint),
                    "fixture-model",
                    &work_endpoint,
                    None,
                    &serde_json::json!({"messages":[]}),
                    PhysicalKind::Chat,
                ),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), owner.entered.notified()).await?;
    task.abort();
    match task.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => anyhow::bail!("pending request was not cancelled"),
    }
    assert_eq!(owner.admitted.load(Ordering::SeqCst), 1);
    assert_eq!(owner.settled.load(Ordering::SeqCst), 0);
    assert_eq!(
        listener
            .accept()
            .err()
            .ok_or_else(|| anyhow::anyhow!("fixture unexpectedly connected"))?
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    Ok(())
}
