//! Same-invocation test material. Every allocation is charged to its caller's window.
use super::*;
use std::io::Write as _;

#[derive(Default)]
pub(super) struct Material {
    events: Vec<Value>,
    finished: bool,
    partial_emitted: bool,
}

pub(super) struct Incomplete {
    owner: Option<Arc<Mutex<Recorder>>>,
}
impl Incomplete {
    pub(super) fn new(enabled: bool, owner: &Arc<Mutex<Recorder>>) -> Self {
        Self {
            owner: enabled.then(|| Arc::clone(owner)),
        }
    }
    pub(super) fn disarm(&mut self) {
        self.owner = None;
    }
}
impl Drop for Incomplete {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            owner.lock().capture.emit_partial("scope_interrupted");
        }
    }
}

/// Call this inside the existing measured future, never before its counter starts.
pub(crate) async fn capture<F: Future>(
    label: &'static str,
    future: F,
) -> Result<(F::Output, Observation), ObservationError> {
    capture_mode(label, MAX_EVENTS, future, true).await
}

fn event(value: Value) -> Option<usize> {
    let mut id = None;
    record(|recorder| {
        if recorder.capture.material.is_none() {
            return Ok(());
        }
        if recorder.events >= recorder.event_limit {
            return Err(ObservationError::Bound);
        }
        let (_, bytes) = digest(&value)?;
        recorder.reserve(bytes)?;
        let material = recorder.capture.material.as_mut().ok_or(ObservationError::Missing)?;
        id = Some(material.events.len());
        material.events.push(value);
        Ok(())
    });
    id
}

/// Records the actual borrowed guarded octets, never a re-encoding.
pub(crate) fn evaluation(request_id: &str, bytes: &[u8]) -> Boundary {
    let mut id = None;
    let _ = ACTIVE.try_with(|owner| {
        if owner.lock().capture.material.is_none() {
            return;
        }
        if bytes.len() > MAX_REQUEST_CAPTURE || request_id.len() > 1024 {
            record(|_| Err(ObservationError::Bound));
            return;
        }
        id = event(json!({"kind":"evaluation_begin","request_id":request_id,
            "guarded_bytes_hex":hex::encode(bytes)}));
    });
    Boundary { id, returned: false }
}

/// Called at the real send boundary after accounting admission, once per retry.
pub(crate) fn send(request_id: &str, bytes: &[u8]) -> Boundary {
    let mut id = None;
    let _ = ACTIVE.try_with(|owner| {
        if owner.lock().capture.material.is_none() {
            return;
        }
        if bytes.len() > MAX_REQUEST_CAPTURE || request_id.len() > 1024 {
            record(|_| Err(ObservationError::Bound));
            return;
        }
        id = event(json!({"kind":"send_begin","request_id":request_id,
            "guarded_bytes_hex":hex::encode(bytes)}));
    });
    Boundary { id, returned: false }
}

pub(crate) struct Boundary {
    id: Option<usize>,
    returned: bool,
}
impl Boundary {
    pub(crate) const fn returned(&mut self) {
        self.returned = true;
    }
}
impl Drop for Boundary {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let _ = event(json!({"kind":"boundary_end","begin":id,"returned":self.returned}));
        }
    }
}

impl Observation {
    /// Consumes and drops capture intermediates before returning newly encoded evidence.
    /// The caller must invoke this before its original measured future returns.
    pub(crate) fn finish_material(
        mut self,
        decisions: &[&EngineDecision],
        idle: bool,
    ) -> Result<Vec<u8>, ObservationError> {
        let result = self.material_bytes(decisions, idle);
        if result.is_ok() {
            if let Some(material) = &mut self.material {
                material.finished = true;
            }
        } else {
            self.emit_partial("validation_failed");
        }
        drop(self);
        result
    }

    fn emit_partial(&mut self, reason: &'static str) {
        let Some(material) = &mut self.material else {
            return;
        };
        if material.finished || material.partial_emitted {
            return;
        }
        material.partial_emitted = true;
        let partial = json!({"schema":"prx-original-material-v1","complete":false,
            "reason":reason,"requests":self.requests.iter().map(|r| &r.payload).collect::<Vec<_>>(),
            "scores":self.scores.iter().map(|s| &s.payload).collect::<Vec<_>>(),
            "transport":material.events});
        if let Ok(bytes) = serde_json::to_vec(&partial) {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(b"PRX_ORIGINAL_MATERIAL_PARTIAL ");
            let _ = stderr.write_all(&bytes);
            let _ = stderr.write_all(b"\n");
        }
    }

    fn material_bytes(&self, decisions: &[&EngineDecision], idle: bool) -> Result<Vec<u8>, ObservationError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let material = self.material.as_ref().ok_or(ObservationError::Missing)?;
        let original = if idle {
            if !decisions.is_empty()
                || !self.requests.is_empty()
                || !self.scores.is_empty()
                || !material.events.is_empty()
            {
                return Err(ObservationError::Changed);
            }
            Value::Null
        } else {
            let mut original = self.checked_sidecar(decisions)?;
            let object = original.as_object_mut().ok_or(ObservationError::Changed)?;
            object.insert(
                "scope".into(),
                json!("same invocation; capture costs charged to original measurement when enabled"),
            );
            object.insert(
                "physical_wire".into(),
                json!("see actual transport events; builtin has no physical HTTP wire"),
            );
            original
        };
        // Each begin must end, including an interrupted backend future. Such an
        // end proves only cancellation, never receipt or successful transport.
        let mut open = BTreeSet::new();
        let mut evaluation: Option<&Value> = None;
        let mut evaluation_index = None;
        for (index, entry) in material.events.iter().enumerate() {
            match entry.get("kind").and_then(Value::as_str) {
                Some("evaluation_begin") => {
                    if evaluation.is_some() {
                        return Err(ObservationError::Duplicate);
                    }
                    let id = entry
                        .get("request_id")
                        .and_then(Value::as_str)
                        .ok_or(ObservationError::Missing)?;
                    if !self
                        .requests
                        .iter()
                        .any(|r| r.payload.pointer("/request/request_id").and_then(Value::as_str) == Some(id))
                    {
                        return Err(ObservationError::Missing);
                    }
                    evaluation = Some(entry);
                    evaluation_index = Some(index);
                    open.insert(index);
                }
                Some("send_begin") => {
                    let parent = evaluation.ok_or(ObservationError::Missing)?;
                    if parent.get("request_id") != entry.get("request_id")
                        || parent.get("guarded_bytes_hex") != entry.get("guarded_bytes_hex")
                    {
                        return Err(ObservationError::Changed);
                    }
                    open.insert(index);
                }
                Some("boundary_end") => {
                    let begin = entry
                        .get("begin")
                        .and_then(Value::as_u64)
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or(ObservationError::Changed)?;
                    if !open.remove(&begin) {
                        return Err(ObservationError::Changed);
                    }
                    if evaluation_index == Some(begin) {
                        if !open.is_empty() {
                            return Err(ObservationError::Interrupted);
                        }
                        evaluation = None;
                        evaluation_index = None;
                    }
                }
                _ => return Err(ObservationError::Changed),
            }
        }
        if !open.is_empty() {
            return Err(ObservationError::Interrupted);
        }
        let value = json!({"schema":"prx-original-material-v1","complete":true,
            "idle":idle,"original":original,"transport":material.events,
            "hop_inventory":decisions.iter().map(|d|json!({"decision":d.decision_id.to_string(),
                "hop":d.state.lineage.hop(),"request_compiled":d.request_compiled,"scored_count":d.scored_count})).collect::<Vec<_>>(),
            "physical_wire":if material.events.is_empty() {"not_applicable_builtin_or_idle"} else {"send_attempts_are_not_receiver_receipts"}});
        let bytes = serde_json::to_vec(&value).map_err(|_| ObservationError::Changed)?;
        if bytes.len() > MAX_BYTES {
            return Err(ObservationError::Bound);
        }
        drop(value);
        Ok(bytes)
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.emit_partial("observation_dropped_before_finish");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_material_has_no_fabricated_request_or_wire() {
        let (_, observed) = capture("idle", async {}).await.expect("bounded idle scope");
        let bytes = observed.finish_material(&[], true).expect("explicit idle evidence");
        let value: Value = serde_json::from_slice(&bytes).expect("material JSON");
        assert_eq!(value.get("original"), Some(&Value::Null));
        assert_eq!(value.get("transport"), Some(&json!([])));
    }

    #[tokio::test]
    async fn unbound_send_cannot_be_certified_as_idle() {
        let (_, observed) = capture("unbound", async {
            let mut send = send("unbound-request", b"actual-test-bytes");
            send.returned();
        })
        .await
        .expect("bounded scope");
        assert!(
            observed.finish_material(&[], true).is_err(),
            "unbound transport cannot become idle evidence"
        );
    }

    #[tokio::test]
    async fn oversized_actual_body_invalidates_capture() {
        let (_, observed) = capture("bound", async {
            let bytes = vec![0; MAX_REQUEST_CAPTURE + 1];
            let _evaluation = evaluation("bounded-request", &bytes);
        })
        .await
        .expect("bounded scope owner");
        assert!(
            matches!(observed.finish_material(&[], true), Err(ObservationError::Bound)),
            "oversized retained body must fail closed"
        );
    }

    #[tokio::test]
    async fn cancelled_scope_does_not_leak_into_next_capture() {
        let retained = Mutex::new(None);
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            capture("cancelled", async {
                ACTIVE.with(|owner| *retained.lock() = Some(Arc::clone(owner)));
                std::future::pending::<()>().await;
            }),
        )
        .await;
        assert!(cancelled.is_err(), "pending capture must be cancelled");
        let owner = retained
            .lock()
            .take()
            .expect("actual cancelled owner retained for inspection");
        {
            let recorder = owner.lock();
            let material = recorder.capture.material.as_ref().expect("material mode");
            assert!(
                material.partial_emitted,
                "cancelled scope must attempt partial evidence persistence"
            );
            assert!(!material.finished, "cancelled scope cannot certify completion");
        }
        drop(owner);
        let (_, observed) = capture("after-cancel", async {})
            .await
            .expect("scope restored after cancellation");
        assert!(
            observed.finish_material(&[], true).is_ok(),
            "new empty scope must be independent"
        );
    }
}
