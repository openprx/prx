#![cfg(test)]
//! Owned HTTPS loopback endpoint for actual platform adapter sends.
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct LocalTransport {
    pub(super) endpoint: String,
    pub(super) client: reqwest::Client,
    records: Arc<parking_lot::Mutex<Vec<(String, serde_json::Value)>>>,
    stop: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}
impl LocalTransport {
    pub(super) async fn start() -> anyhow::Result<Self> {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
            )?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("https://127.0.0.1:{}/", listener.local_addr()?.port());
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .add_root_certificate(reqwest::Certificate::from_der(cert.cert.der().as_ref())?)
            .timeout(std::time::Duration::from_secs(5))
            .build()?;
        let records = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let captured = Arc::clone(&records);
        let stop = tokio_util::sync::CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    () = cancelled.cancelled() => return Ok(()),
                    result = listener.accept() => result?,
                };
                anyhow::ensure!(accepted.1.ip().is_loopback(), "nonlocal fixture connection");
                let work = async {
                    let mut tls = acceptor.accept(accepted.0).await?;
                    let mut bytes = Vec::new();
                    let (header_end, size) = loop {
                        let byte = tls.read_u8().await?;
                        bytes.push(byte);
                        anyhow::ensure!(bytes.len() <= 16384, "bounded local HTTP header");
                        if bytes.ends_with(b"\r\n\r\n") {
                            let text = std::str::from_utf8(&bytes)?;
                            let size = text
                                .lines()
                                .find_map(|line| {
                                    line.split_once(':')
                                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                        .map(|(_, value)| value.trim().parse::<usize>())
                                })
                                .transpose()?
                                .ok_or_else(|| anyhow::anyhow!("local POST content length missing"))?;
                            break (bytes.len(), size);
                        }
                    };
                    anyhow::ensure!(size <= 65536, "bounded local HTTP body");
                    let header = std::str::from_utf8(&bytes)?;
                    let request = header
                        .lines()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("request line missing"))?
                        .to_owned();
                    anyhow::ensure!(request.starts_with("POST "), "local adapter must POST");
                    bytes.resize(header_end + size, 0);
                    let body_bytes = bytes
                        .get_mut(header_end..)
                        .ok_or_else(|| anyhow::anyhow!("local HTTP body offset exceeds buffer"))?;
                    tls.read_exact(body_bytes).await?;
                    let body = serde_json::from_slice(body_bytes)?;
                    {
                        let mut records = captured.lock();
                        anyhow::ensure!(records.len() < 64, "bounded local request count");
                        records.push((request, body));
                    }
                    tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await?;
                    tls.shutdown().await?;
                    Ok::<_, anyhow::Error>(())
                };
                tokio::select! {
                    biased;
                    () = cancelled.cancelled() => return Ok(()),
                    result = tokio::time::timeout(std::time::Duration::from_secs(5), work) => result??,
                }
            }
        });
        Ok(Self {
            endpoint,
            client,
            records,
            stop,
            task: Some(task),
        })
    }
    pub(super) fn records(&self) -> Vec<(String, serde_json::Value)> {
        self.records.lock().clone()
    }
    pub(super) async fn finish(mut self) -> anyhow::Result<()> {
        self.stop.cancel();
        let task = self
            .task
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("local transport already joined"))?;
        match tokio::time::timeout(std::time::Duration::from_secs(5), task).await {
            Ok(result) => {
                self.task.take();
                result?
            }
            Err(error) => {
                let task = self.task.as_mut().expect("owned transport task remains present");
                task.abort();
                let _ = task.await;
                self.task.take();
                Err(error.into())
            }
        }
    }
}
impl Drop for LocalTransport {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
