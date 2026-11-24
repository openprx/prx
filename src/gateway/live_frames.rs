//! Ephemeral rf stream delivery. Producers never wait for a socket.
use crate::agent::{live_frames::LiveFrameSink, turn_contracts::ToolLoopEvent};
use crate::runtime::envelope::RuntimeEnvelope;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, mpsc};

const CONNECTION_FRAMES: usize = 256;
const PRODUCER_FRAMES: usize = 64;
const TEXT_BYTES: usize = 4096;
const RAW_BYTES: usize = 65536;

#[derive(Default)]
pub struct StreamHub {
    state: Mutex<HubState>,
}
#[derive(Default)]
struct HubState {
    connections: HashMap<String, Arc<StreamQueue>>,
    runs: HashMap<(String, String), bool>,
    producers: HashMap<(String, String), std::sync::Weak<Producer>>,
}
#[derive(Default)]
pub(crate) struct StreamQueue {
    state: Mutex<QueueState>,
    pub(crate) notify: Notify,
}
#[derive(Default)]
struct QueueState {
    frames: VecDeque<Value>,
    gaps: HashSet<String>,
    failed: bool,
}
impl QueueState {
    fn gap(&mut self, run: &str) {
        if self.gaps.len() == CONNECTION_FRAMES && !self.gaps.contains(run) {
            self.failed = true;
        } else {
            self.gaps.insert(run.to_owned());
        }
    }
}
impl StreamQueue {
    pub(crate) fn failed(&self) -> bool {
        self.state.lock().failed
    }
    pub(crate) fn pending(&self) -> bool {
        let state = self.state.lock();
        state.failed || !state.frames.is_empty() || !state.gaps.is_empty()
    }
    pub(crate) fn pop(&self) -> Option<Value> {
        let mut state = self.state.lock();
        if let Some(run) = state.gaps.iter().next().cloned() {
            state.gaps.remove(&run);
            return Some(json!({"type":"stream_gap","run_id":run}));
        }
        state.frames.pop_front()
    }
    fn gap(&self, run: &str) {
        self.state.lock().gap(run);
        self.notify.notify_one();
    }
    fn push(&self, frame: Value) {
        let mut state = self.state.lock();
        if state.frames.len() == CONNECTION_FRAMES {
            if let Some(old) = state.frames.pop_front() {
                if let Some(run) = old.get("run_id").and_then(Value::as_str) {
                    state.gap(run);
                }
            }
        }
        state.frames.push_back(frame);
        drop(state);
        self.notify.notify_one();
    }
}
impl StreamHub {
    pub(crate) fn connected(&self, owner: &str) -> bool {
        self.state
            .lock()
            .connections
            .get(owner)
            .is_some_and(|queue| !queue.failed())
    }
    pub(crate) fn connect(&self, owner: &str) -> Arc<StreamQueue> {
        let queue = Arc::new(StreamQueue::default());
        let mut state = self.state.lock();
        for ((run_owner, run), emitted) in &state.runs {
            if run_owner == owner && *emitted {
                queue.gap(run);
            }
        }
        state.connections.insert(owner.to_owned(), Arc::clone(&queue));
        queue
    }
    pub(crate) fn disconnect(&self, owner: &str, queue: &Arc<StreamQueue>) {
        let mut state = self.state.lock();
        if state.connections.get(owner).is_some_and(|old| Arc::ptr_eq(old, queue)) {
            state.connections.remove(owner);
        }
    }
    fn publish(&self, owner: &str, run: &str, value: Value) {
        let mut state = self.state.lock();
        if let Some(emitted) = state.runs.get_mut(&(owner.to_owned(), run.to_owned())) {
            *emitted = true;
        }
        if let Some(queue) = state.connections.get(owner) {
            queue.push(value);
        }
    }
    fn gap(&self, owner: &str, run: &str) {
        let mut state = self.state.lock();
        if let Some(emitted) = state.runs.get_mut(&(owner.to_owned(), run.to_owned())) {
            *emitted = true;
        }
        if let Some(queue) = state.connections.get(owner) {
            queue.gap(run);
        }
    }
    pub(crate) fn producer(
        self: &Arc<Self>,
        root: &Value,
        envelope: &RuntimeEnvelope,
        tools: &HashSet<String>,
    ) -> Arc<dyn LiveFrameSink> {
        let owner = envelope.resolved_owner_id();
        let run = envelope.run_id.clone().unwrap_or_default();
        let existing = self
            .state
            .lock()
            .producers
            .get(&(owner.clone(), run.clone()))
            .and_then(std::sync::Weak::upgrade);
        if let Some(producer) = existing {
            return producer;
        }
        let mut scope = root.clone();
        if let Some(object) = scope.as_object_mut() {
            object.insert("session".into(), json!(envelope.session_key));
            object.insert("run_id".into(), json!(run));
            if let Some(parent) = &envelope.parent_run_id {
                object.insert(
                    "parent_session".into(),
                    json!(envelope.parent_session_key.as_ref().unwrap_or(&envelope.session_key)),
                );
                object.insert("parent_run_id".into(), json!(parent));
                object.insert(
                    "root_session".into(),
                    root.get("session").cloned().unwrap_or(Value::Null),
                );
                object.insert("root_run_id".into(), root.get("run_id").cloned().unwrap_or(Value::Null));
            }
        }
        self.state.lock().runs.insert((owner.clone(), run.clone()), false);
        let (tx, rx) = mpsc::channel(PRODUCER_FRAMES);
        let gap = Arc::new(AtomicBool::new(false));
        tokio::spawn(coalesce(
            Arc::clone(self),
            owner,
            run,
            scope,
            tools.clone(),
            rx,
            Arc::clone(&gap),
        ));
        let producer = Arc::new(Producer {
            tx,
            gap,
            child: envelope.parent_run_id.is_some(),
        });
        self.state.lock().producers.insert(
            (
                envelope.resolved_owner_id(),
                envelope.run_id.clone().unwrap_or_default(),
            ),
            Arc::downgrade(&producer),
        );
        producer
    }
}
struct Producer {
    tx: mpsc::Sender<Value>,
    gap: Arc<AtomicBool>,
    child: bool,
}
impl Producer {
    fn send(&self, value: Value) {
        if self.tx.try_send(value).is_err() {
            self.gap.store(true, Ordering::Relaxed);
        }
    }
}
impl LiveFrameSink for Producer {
    fn thinking(&self) {
        self.send(json!({"kind":"typing","state":"thinking"}));
    }
    fn awaiting_approval(&self, tool: &str) {
        self.send(json!({"kind":"tool_status","tool":tool,"phase":"awaiting_approval"}));
    }
    fn child_denial(
        &self,
        tool: &str,
        child_run_id: &str,
        cause: crate::security::approval_ledger::run_call::RfDenialCause,
    ) {
        use crate::security::approval_ledger::run_call::RfDenialCause;
        use crate::tools::execution::ToolCallRefusalReason;
        let phase = match cause {
            RfDenialCause::Refusal(ToolCallRefusalReason::Failed | ToolCallRefusalReason::Indeterminate) => "failed",
            RfDenialCause::Cancelled => "cancelled",
            RfDenialCause::Approval(_) | RfDenialCause::Refusal(_) => "denied",
        };
        let cause = crate::security::approval_ledger::run_call::rf_denial_cause(cause);
        self.send(json!({"kind":"tool_status","tool":tool,"phase":phase,"cause":cause,"child_run_id":child_run_id}));
    }
    fn emit(&self, event: &ToolLoopEvent) {
        use crate::agent::tool_evidence::ToolCallOutcome;
        match event {
            ToolLoopEvent::TextDelta(text) if !self.child => {
                if text.len() > RAW_BYTES {
                    self.gap.store(true, Ordering::Relaxed);
                } else {
                    self.send(json!({"kind":"delta","text":text}));
                }
            }
            ToolLoopEvent::ToolStarted { name, .. } => {
                self.send(json!({"kind":"tool_status","tool":name,"phase":"started"}))
            }
            ToolLoopEvent::ToolFinished {
                name,
                outcome,
                refusal_cause,
                ..
            } => {
                let (phase, cause) = match outcome {
                    ToolCallOutcome::Ok => ("succeeded", None),
                    ToolCallOutcome::Denied { cause } => (
                        "denied",
                        cause.map(crate::security::approval_ledger::run_call::rf_denial_cause),
                    ),
                    ToolCallOutcome::Refused | ToolCallOutcome::PlanOnly => ("denied", None),
                    ToolCallOutcome::Cancelled => ("cancelled", Some("cancelled")),
                    ToolCallOutcome::AwaitingApproval => ("awaiting_approval", None),
                    ToolCallOutcome::Error | ToolCallOutcome::Unknown => ("failed", None),
                };
                let cause = refusal_cause
                    .map(crate::security::approval_ledger::run_call::rf_denial_cause)
                    .or(cause);
                let mut frame = json!({"kind":"tool_status","tool":name,"phase":phase});
                if let (Some(cause), Some(object)) = (cause, frame.as_object_mut()) {
                    object.insert("cause".into(), json!(cause));
                }
                self.send(frame);
            }
            // In particular ReasoningDelta is neither copied nor queued.
            _ => {}
        }
    }
}
fn prefix(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
}
async fn coalesce(
    hub: Arc<StreamHub>,
    owner: String,
    run: String,
    scope: Value,
    tools: HashSet<String>,
    mut rx: mpsc::Receiver<Value>,
    gap: Arc<AtomicBool>,
) {
    let mut buffer = crate::agent::stream_buffer::StreamBoundaryBuffer::new();
    let mut raw_bytes = 0usize;
    let mut structured = String::new();
    let mut holding = false;
    let mut suppress_text = false;
    let mut text = String::new();
    let mut frames = VecDeque::new();
    let mut idx = 0u64;
    let mut closed = false;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            value = rx.recv(), if !closed && frames.len() < PRODUCER_FRAMES && text.len() < RAW_BYTES => {
                match value {
                    Some(value) if value.get("kind").and_then(Value::as_str) == Some("delta") => {
                        if let Some(delta) = value.get("text").and_then(Value::as_str) {
                            if suppress_text || gap.load(Ordering::Relaxed) { suppress_text = true; continue; }
                            raw_bytes = raw_bytes.saturating_add(delta.len());
                            if raw_bytes > RAW_BYTES {
                                buffer = crate::agent::stream_buffer::StreamBoundaryBuffer::new(); structured.clear(); holding = true; suppress_text = true; raw_bytes = 0; gap.store(true, Ordering::Relaxed);
                            } else if holding || delta.contains(['<','{','[']) {
                                if !holding {
                                    if let Some(tail) = buffer.flush_all() { text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&tail, &tools)); }
                                    holding = true;
                                }
                                structured.push_str(delta);
                            } else if let Some(chunk) = buffer.push(delta) {
                                raw_bytes = 0;
                                text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&chunk, &tools));
                            }
                        }
                    }
                    Some(value) => {
                        suppress_text = false;
                        if holding { text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&structured, &tools)); structured.clear(); holding = false; raw_bytes = 0; }
                        if let Some(tail) = buffer.flush_all() { text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&tail, &tools)); raw_bytes = 0; }
                        while !text.is_empty() {
                            let end = prefix(&text, TEXT_BYTES);
                            let delta: String = text.drain(..end).collect();
                            frames.push_back(json!({"kind":"delta","text":delta}));
                        }
                        frames.push_back(value);
                    }
                    None => {
                        closed = true;
                        if holding { text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&structured, &tools)); structured.clear(); holding = false; }
                        if let Some(tail) = buffer.flush_all() { text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&tail, &tools)); }
                    }
                }
            }
            _ = tick.tick() => {
                if gap.swap(false, Ordering::Relaxed) { hub.gap(&owner, &run); buffer = crate::agent::stream_buffer::StreamBoundaryBuffer::new(); structured.clear(); holding = false; suppress_text = true; raw_bytes = 0; }
                // A short safe text tail is flushed on the pacing boundary too.
                if raw_bytes > 0 && !holding {
                    if let Some(tail) = buffer.flush_all() { text.push_str(&crate::agent::sanitize::sanitize_stream_chunk(&tail, &tools)); }
                    raw_bytes = 0;
                }
                let next = frames.pop_front().or_else(|| if !text.is_empty() {
                    let end = prefix(&text, TEXT_BYTES);
                    let delta: String = text.drain(..end).collect();
                    Some(json!({"kind":"delta","text":delta}))
                } else { None });
                if let Some(frame) = next {
                    if let Some(next_idx) = idx.checked_add(1) {
                        idx = next_idx;
                        let mut value = scope.clone();
                        if let (Some(object), Some(frame)) = (value.as_object_mut(), frame.as_object()) {
                            object.extend(frame.clone()); object.insert("type".into(), json!("stream")); object.insert("idx".into(), json!(idx));
                            hub.publish(&owner, &run, value);
                        }
                    } else { hub.gap(&owner, &run); break; }
                } else if closed { break; }
            }
        }
    }
    let mut state = hub.state.lock();
    state.runs.remove(&(owner.clone(), run.clone()));
    state.producers.remove(&(owner, run));
}
