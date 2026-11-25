//! The system-prompt layout: the slots, the `Source` trait and the builder
//! that places sources in slot order.
//!
//! Every surface (terminal chat, Console and rf sessions, webhooks, named
//! agents, scheduled runs, task and process children, channels) assembles its
//! system prompt here. A source names a slot and cannot choose a position; the
//! builder sorts stable sources ahead of volatile ones and, inside each group,
//! by slot. A segment that renders nothing is recorded in the provenance and
//! left out of the text.
use crate::config::IdentityConfig;
use crate::skills::Skill;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

/// What a source may read when it renders.
pub struct PromptInputs<'a> {
    pub workspace_dir: &'a Path,
    pub model_name: &'a str,
    pub skills: &'a [Skill],
    pub identity_config: Option<&'a IdentityConfig>,
    pub bootstrap_max_chars: Option<usize>,
    pub native_tools: bool,
    /// The turn's effective level; it selects the safety paragraph.
    pub autonomy: crate::security::AutonomyLevel,
}

/// The place a segment takes in the system prompt. The declaration order is
/// the layout order, and it is the order the prompt has always had: the
/// fixed rule sections, then the skills index, the workspace line, the
/// identity files, the time zone and the runtime line, then what a delegated
/// role appends, then sections a caller adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Slot {
    Task,
    ToolEvidence,
    ContextHandoff,
    Safety,
    Orchestration,
    Skills,
    Workspace,
    Identity,
    DateTime,
    Runtime,
    DelegatedRole,
    Custom,
}

impl Slot {
    /// The name recorded in the section provenance.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::ToolEvidence => "tool_evidence",
            Self::ContextHandoff => "context_handoff",
            Self::Safety => "safety",
            Self::Orchestration => "orchestration",
            Self::Skills => "skills",
            Self::Workspace => "workspace",
            Self::Identity => "identity",
            Self::DateTime => "datetime",
            Self::Runtime => "runtime",
            Self::DelegatedRole => "delegated_role",
            Self::Custom => "custom",
        }
    }
}

/// One rendered piece of the system prompt.
pub(crate) struct Segment {
    pub(crate) text: String,
}

impl Segment {
    /// A segment of `text`, or `None` when the text is blank.
    pub(crate) fn new(text: String) -> Option<Self> {
        (!text.trim().is_empty()).then_some(Self { text })
    }
}

/// Produces one segment of the system prompt for a declared slot.
pub(crate) trait Source: Send + Sync {
    fn slot(&self) -> Slot;

    /// The name recorded in the provenance; a slot's own name unless the
    /// source is a caller-supplied section.
    fn name(&self) -> &str {
        self.slot().name()
    }

    /// Whether the text changes between turns of one session. Volatile
    /// sources are placed after every stable one, so a change cannot move the
    /// provider's cacheable prefix.
    fn is_volatile(&self) -> bool {
        false
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSectionFingerprint {
    pub name: String,
    /// The slot the source fills (see [`Slot::name`]).
    pub slot: &'static str,
    /// Whether the source is volatile: placed after every stable source.
    pub volatile: bool,
    pub included: bool,
    pub chars: usize,
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledSystemPrompt {
    pub content: String,
    pub chars: usize,
    pub sha256: String,
    pub sections: Vec<PromptSectionFingerprint>,
}

#[must_use]
pub fn prompt_sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// The builder: sources in layout order.
#[derive(Default)]
pub(crate) struct Layout {
    sources: Vec<Box<dyn Source>>,
}

impl Layout {
    /// The sources every runtime surface has: the rule sections, the skills
    /// index, the workspace line, the identity files, the time zone and the
    /// runtime line.
    pub(crate) fn standard() -> Self {
        use super::sources::{
            ContextHandoffSection, DateTimeSection, IdentitySection, OrchestrationSection, RuntimeSection,
            SafetySection, SkillsSection, TaskSection, WorkspaceSection,
        };
        Self::default()
            .place(Box::new(TaskSection))
            .place(Box::new(ContextHandoffSection))
            .place(Box::new(SafetySection))
            .place(Box::new(OrchestrationSection))
            .place(Box::new(SkillsSection))
            .place(Box::new(WorkspaceSection))
            .place(Box::new(IdentitySection))
            .place(Box::new(DateTimeSection))
            .place(Box::new(RuntimeSection))
    }

    /// Add `source` at the position its slot gives it. A source for a slot
    /// that is already filled replaces the earlier one (caller sections are
    /// the exception and keep their insertion order).
    #[must_use]
    pub(crate) fn place(mut self, source: Box<dyn Source>) -> Self {
        let slot = source.slot();
        if slot != Slot::Custom {
            self.sources.retain(|existing| existing.slot() != slot);
        }
        let key = (source.is_volatile(), slot);
        let at = self
            .sources
            .iter()
            .position(|existing| (existing.is_volatile(), existing.slot()) > key)
            .unwrap_or(self.sources.len());
        self.sources.insert(at, source);
        self
    }

    pub(crate) fn has(&self, slot: Slot) -> bool {
        self.sources.iter().any(|source| source.slot() == slot)
    }

    pub(crate) fn compile(&self, inputs: &PromptInputs<'_>) -> Result<CompiledSystemPrompt> {
        let mut output = String::new();
        let mut fingerprints = Vec::with_capacity(self.sources.len());
        for source in &self.sources {
            let segment = source.render(inputs)?;
            let part = segment.as_ref().map_or("", |segment| segment.text.trim_end());
            let included = !part.trim().is_empty();
            fingerprints.push(PromptSectionFingerprint {
                name: source.name().to_string(),
                slot: source.slot().name(),
                volatile: source.is_volatile(),
                included,
                chars: part.chars().count(),
                bytes: part.len(),
                sha256: prompt_sha256(part),
            });
            if !included {
                continue;
            }
            output.push_str(part);
            output.push_str("\n\n");
        }
        let compiled = CompiledSystemPrompt {
            chars: output.chars().count(),
            sha256: prompt_sha256(&output),
            content: output,
            sections: fingerprints,
        };
        // The manifest travels beside the text, never inside it: the request
        // record looks it up by the hash of the prompt it sends.
        super::request_provenance::register_prompt(&compiled);
        Ok(compiled)
    }
}
