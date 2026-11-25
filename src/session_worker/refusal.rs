//! Typed reasons a session worker refuses to start.
//!
//! A worker that does not accept what its parent sealed for it ends with one
//! of a small, closed set of codes instead of free text. The code is written
//! to the worker's own log, carried in the result the worker prints, and
//! logged by the parent next to the run, so an operator and the model both
//! see why a child did not run. A code never carries payload text or secrets.

use std::fmt;

/// Why a worker (or the parent on its behalf) refused a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRefusalReason {
    /// The manifest could not be decoded, authenticated or validated.
    ManifestInvalid,
    /// Parent and worker do not speak the same sealed protocol version.
    ProtocolVersionMismatch,
    /// The configuration files on disk differ from the digest the parent sealed.
    ConfigGenerationMismatch,
    /// The configuration the worker loaded does not reproduce the parent's
    /// sealed effective configuration.
    ConfigChanged,
    /// The configuration directory the worker was told to use is not the sealed one.
    ConfigSourceMismatch,
    /// A failure that carries no refusal reason (it is not a refusal).
    Unclassified,
}

impl WorkerRefusalReason {
    const REFUSALS: [Self; 5] = [
        Self::ManifestInvalid,
        Self::ProtocolVersionMismatch,
        Self::ConfigGenerationMismatch,
        Self::ConfigChanged,
        Self::ConfigSourceMismatch,
    ];

    /// The stable short code, safe for logs and for the model.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ManifestInvalid => "manifest_invalid",
            Self::ProtocolVersionMismatch => "protocol_version_mismatch",
            Self::ConfigGenerationMismatch => "config_generation_mismatch",
            Self::ConfigChanged => "config_changed",
            Self::ConfigSourceMismatch => "config_source_mismatch",
            Self::Unclassified => "unclassified",
        }
    }
}

impl fmt::Display for WorkerRefusalReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

/// A refusal carrying its typed reason and a fixed, payload-free message.
#[derive(Debug)]
pub struct WorkerRefusal {
    reason: WorkerRefusalReason,
    message: String,
}

impl fmt::Display for WorkerRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            formatter.write_str(self.reason.code())
        } else {
            write!(formatter, "{}: {}", self.reason.code(), self.message)
        }
    }
}

impl std::error::Error for WorkerRefusal {}

/// A new refusal with a message.
pub fn refusal(reason: WorkerRefusalReason, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(WorkerRefusal {
        reason,
        message: message.into(),
    })
}

/// Give `error` the typed `reason` unless it already carries one.
pub fn tag(reason: WorkerRefusalReason) -> impl FnOnce(anyhow::Error) -> anyhow::Error {
    move |error| {
        if reason_of(&error).is_some() {
            error
        } else {
            error.context(WorkerRefusal {
                reason,
                message: String::new(),
            })
        }
    }
}

/// The typed reason of `error` at any depth of its chain, or `Unclassified`.
#[must_use]
pub fn classify(error: &anyhow::Error) -> WorkerRefusalReason {
    if let Some(refusal) = error.downcast_ref::<WorkerRefusal>() {
        return refusal.reason;
    }
    if error
        .downcast_ref::<crate::config::generation::ProcessConfigurationChanged>()
        .is_some()
    {
        return WorkerRefusalReason::ConfigChanged;
    }
    if error
        .downcast_ref::<super::protocol::WorkerManifestUnsupported>()
        .is_some()
    {
        return WorkerRefusalReason::ManifestInvalid;
    }
    WorkerRefusalReason::Unclassified
}

/// The typed reason of `error`, if it carries one.
#[must_use]
pub fn reason_of(error: &anyhow::Error) -> Option<WorkerRefusalReason> {
    Some(classify(error)).filter(|reason| *reason != WorkerRefusalReason::Unclassified)
}

/// The text a worker reports for `error`: the full chain, led by its code.
#[must_use]
pub fn describe(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    match reason_of(error) {
        Some(reason) if !text.starts_with(reason.code()) => format!("{}: {text}", reason.code()),
        _ => text,
    }
}

/// The reason a worker's reported failure text leads with, if any. Accepts the
/// `worker_unverified_failure:` and `sub-agent reported:` lead-ins the parent
/// adds when it records the text.
#[must_use]
pub fn reason_of_report(text: &str) -> Option<WorkerRefusalReason> {
    let mut rest = text.trim_start();
    for lead in ["sub-agent reported:", "worker_unverified_failure:"] {
        rest = rest.strip_prefix(lead).map_or(rest, str::trim_start);
    }
    WorkerRefusalReason::REFUSALS.into_iter().find(|reason| {
        rest.strip_prefix(reason.code())
            .is_some_and(|tail| tail.starts_with(':'))
    })
}
