#![cfg(test)]
use crate::agent::context_budget::token_timing::TokenTiming;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;

tokio::task_local! { static CURRENT: Arc<Mutex<Snapshot>>; }
#[derive(Default, serde::Serialize)]
pub(crate) struct Snapshot {
    rows: Vec<Row>,
    overflow: bool,
}
#[derive(serde::Serialize)]
struct Row {
    stages: [Option<Stage>; 16],
    overflow: bool,
    tokenizer: Option<TokenTiming>,
}
#[derive(Clone, Copy, serde::Serialize)]
struct Stage {
    stage: &'static str,
    elapsed_ns: u64,
    remaining_deadline_ns: u64,
}
pub(crate) struct Capture(Arc<Mutex<Snapshot>>);
impl Capture {
    pub(crate) fn has_stage(&self, stage: &str) -> bool {
        self.0
            .lock()
            .rows
            .iter()
            .any(|row| row.stages.iter().flatten().any(|record| record.stage == stage))
    }
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(Snapshot {
            rows: Vec::with_capacity(16),
            overflow: false,
        })))
    }
    pub(crate) fn scope<F: std::future::Future>(
        &self,
        future: std::pin::Pin<Box<F>>,
    ) -> impl std::future::Future<Output = F::Output> {
        CURRENT.scope(Arc::clone(&self.0), future)
    }
    pub(crate) fn write(&self, path: &std::path::Path) -> anyhow::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(&*self.0.lock())?)?;
        Ok(())
    }
}
pub(crate) struct Span {
    capture: Arc<Mutex<Snapshot>>,
    started: Instant,
    deadline: Instant,
    row: Row,
    used: usize,
}
impl Span {
    pub(crate) fn current(deadline: Instant) -> Option<Self> {
        CURRENT
            .try_with(|capture| Self {
                capture: Arc::clone(capture),
                started: Instant::now(),
                deadline,
                row: Row {
                    stages: [None; 16],
                    overflow: false,
                    tokenizer: None,
                },
                used: 0,
            })
            .ok()
    }
    pub(crate) fn mark(&mut self, stage: &'static str) {
        let now = Instant::now();
        if let Some(slot) = self.row.stages.get_mut(self.used) {
            *slot = Some(Stage {
                stage,
                elapsed_ns: u64::try_from(now.saturating_duration_since(self.started).as_nanos()).unwrap_or(u64::MAX),
                remaining_deadline_ns: u64::try_from(self.deadline.saturating_duration_since(now).as_nanos())
                    .unwrap_or(u64::MAX),
            });
            self.used += 1;
        } else {
            self.row.overflow = true;
        }
    }
    pub(crate) fn tokenizer(&mut self, timing: Option<TokenTiming>) {
        self.row.tokenizer = timing;
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        let mut capture = self.capture.lock();
        if capture.rows.len() < 16 {
            capture.rows.push(Row {
                stages: self.row.stages,
                overflow: self.row.overflow,
                tokenizer: self.row.tokenizer,
            });
        } else {
            capture.overflow = true;
        }
    }
}
