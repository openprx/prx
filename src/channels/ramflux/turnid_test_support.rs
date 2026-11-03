//! Controlled ownership and identity seams for socket acceptance tests.
#![allow(clippy::expect_used, clippy::panic)]
use super::*;

impl RamfluxChannel {
    pub(crate) async fn turnid_race(&self, a: serde_json::Value, b: serde_json::Value) -> [Result<(), String>; 2] {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let send = |frame: serde_json::Value| {
            let inner = Arc::clone(&self.inner);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                inbound::on_frame(&inner, &serde_json::to_vec(&frame).expect("test: frame"))
                    .await
                    .map_err(|error| error.to_string())
            })
        };
        let (a, b) = tokio::join!(send(a), send(b));
        [a.expect("test: first ingress"), b.expect("test: second ingress")]
    }

    pub(crate) fn turnid_resume_forward(&self) {
        let barrier = self.inner.forward_release.lock().take().expect("test: forward barrier");
        tokio::spawn(async move {
            barrier.wait().await;
        });
    }

    pub(crate) async fn turnid_wait_disconnect(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while self.inner.connection_open.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test: disconnected");
    }

    pub(crate) async fn turnid_wait_slot_released(&self, conversation: &str, message: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while self.inner.queue.owns(conversation, message) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test: slot released");
    }

    pub(crate) async fn turnid_wait_unreserved(&self, conversation: &str, host: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while self.inner.rpc.turn_ids.has_host(conversation, host) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test: host released");
    }

    pub(crate) fn turnid_queue_barrier(&self, suspended: bool) {
        if suspended {
            self.inner.queue.suspend_delete("conv-1", "barrier");
        } else {
            assert_eq!(self.inner.queue.resume_delete("conv-1", "barrier"), []);
        }
    }

    pub(crate) fn turnid_live(&self, conversation: &str, host: &str) -> Option<String> {
        self.inner.rpc.turn_ids.resolve(conversation, host)
    }

    pub(crate) fn turnid_pause_forward(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        *self.inner.forward_release.lock() = Some(Arc::clone(&barrier));
        *self.inner.forward_pause.lock() = Some((tx, barrier));
        rx
    }

    pub(crate) fn turnid_abort_forward(&self) {
        self.inner
            .forward_abort
            .lock()
            .take()
            .expect("test: forwarding task")
            .abort();
    }

    pub(crate) fn turnid_hold_owner(&self, conversation: &str, message: &str) -> Arc<turn_ids::Reservation> {
        self.inner
            .rpc
            .turn_ids
            .owner(conversation, message)
            .expect("test: owner")
    }

    pub(crate) fn turnid_retire_unbound(&self) {
        self.inner.rpc.turn_ids.release_unbound(None);
    }

    pub(crate) fn turnid_detach_store(&self) {
        self.inner.store.detach();
    }

    pub(crate) fn turnid_attach_store(&self, backend: crate::memory::erasure::SharedErasureStore) {
        self.inner.store.attach(backend);
    }

    pub(crate) async fn turnid_wait_forward_finished(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if self
                    .inner
                    .forward_abort
                    .lock()
                    .as_ref()
                    .is_some_and(tokio::task::AbortHandle::is_finished)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test: forwarding task finished");
    }

    pub(crate) fn turnid_volatile_empty(&self) -> bool {
        self.inner.rpc.volatile_empty()
    }
}

pub(crate) fn turnid_code_fixture() -> [String; 2] {
    let ids = Arc::new(turn_ids::TurnIds::default());
    let a = ids
        .reserve("conv", "a", Some("deadbeef:1"), |_| false)
        .expect("test: first");
    let b = ids
        .reserve("conv", "b", Some("deadbeef:2"), |_| false)
        .expect("test: second");
    ids.bind("conv", "a", "12345678-0000-4000-8000-000000000001");
    ids.bind("conv", "b", "87654321-0000-4000-8000-000000000002");
    let codes = [
        crate::channels::turn_failure::turn_error_id(&ids.resolve("conv", "deadbeef:1").expect("test: run")),
        crate::channels::turn_failure::turn_error_id(&ids.resolve("conv", "deadbeef:2").expect("test: run")),
    ];
    drop((a, b));
    codes
}

#[test]
fn shutdown_pump_does_not_reenter_admission() {
    let (done, completed) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let dir = tempfile::tempdir().expect("test: workspace");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test: runtime");
        let handle = runtime.handle().clone();
        drop(runtime);
        let _entered = handle.enter();
        for bind_failure in [true, false] {
            let channel = RamfluxChannel::new(
                RamfluxConfig {
                    on_new_message: crate::config::RamfluxOnNewMessage::Queue,
                    ..Default::default()
                },
                dir.path(),
            )
            .expect("test: channel");
            let prepare = |id: &str| {
                inbound::prepare_message(&channel.inner, &serde_json::json!({
                    "conversation_id":"conv", "message_id":id, "turn_id":id,
                    "sender":{"principal_id":"prx_user", "device_id":"prx_node", "kind":"human", "trust":"verified", "is_self":false},
                    "chat":{"conversation_id":"conv", "kind":"dm", "member_count":2,"mentioned":false},
                    "text":"request", "timestamp_ms":1
                })).expect("test: admission").expect("test: admitted")
            };
            let first = prepare("first");
            assert!(matches!(prepare("second"), turn_queue::Admission::Queued));
            if bind_failure {
                // Losing the pre-bind owner forces claim_admitted's failure path.
                drop(first);
                assert!(!channel.claim_admitted("conv", "first", "internal"));
            } else {
                let released = {
                    let _admission = channel.inner.admission.lock();
                    channel.inner.cancel_message("conv", "first")
                };
                channel.inner.pump_cancelled(released);
                drop(first);
            }
            assert!(!channel.inner.queue.owns("conv", "first"));
            assert!(!channel.inner.queue.owns("conv", "second"));
        }
        done.send(()).expect("test: shutdown completed");
    });
    completed
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("test: no admission deadlock on shutdown");
}
