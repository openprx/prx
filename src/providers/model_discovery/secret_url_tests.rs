use super::*;
use crate::secret_url_test_support::{DEADLINE, MARKER, dropping_server};

#[test]
fn secret_url_model_discovery_strips_source_errors() {
    for status in [Some(500), None] {
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let server_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("test: fixture runtime");
            runtime.block_on(async {
                tokio::time::timeout(DEADLINE, async {
                    let (address, server) = dropping_server(1, status).await;
                    ready_tx.send(address).expect("test: publish listener address");
                    tokio::time::timeout(DEADLINE, server)
                        .await
                        .expect("test: fixture deadline")
                        .expect("test: fixture task");
                })
                .await
                .expect("test: model fixture lifetime deadline");
            });
            done_tx.send(()).expect("test: publish fixture completion");
        });
        let address = ready_rx.recv_timeout(DEADLINE).expect("test: listener ready deadline");
        let endpoint = format!("http://{address}/v1/models?sig={MARKER}");
        let error = fetch_openai_compatible_models(&endpoint, None, true).expect_err("test: failing model endpoint");
        for rendered in [
            format!("{error:#}"),
            crate::security::redaction::redacted_error_report(&error),
        ] {
            assert!(
                !rendered.contains(MARKER),
                "test: model fetch leaked query credential: {rendered}"
            );
            assert!(
                rendered.contains("model fetch failed: GET"),
                "test: model fetch context was lost"
            );
        }
        assert!(
            !error.to_string().contains(MARKER),
            "test: endpoint label retains query value"
        );
        done_rx
            .recv_timeout(DEADLINE)
            .expect("test: fixture completion deadline");
        server_thread.join().expect("test: join completed fixture thread");
    }
}
