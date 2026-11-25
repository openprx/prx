//! The provenance of one provider request: which contributor put what into it.
//!
//! The record is the `provenance` object of the compiled-request
//! row. It holds hashes, counts, names and typed reasons only: no prompt
//! text, no message text, no file path, no memory key. Three inputs feed it:
//!
//! * the segment manifest the layout produces beside the compiled prompt
//!   ([`register_prompt`]), found again by the hash of the prompt that is sent;
//! * the reducers a round ran ([`note_reducers`], called by the ladder) and the
//!   attachment of the live message ([`note_live_attachment`], called by the
//!   surface that builds it), both kept per task until the request row reads
//!   them;
//! * the messages of the request itself, counted once.

use super::layout::{CompiledSystemPrompt, PromptSectionFingerprint};
use super::reduce::{Outcome, Tier};
use crate::context::transcript::AttachmentSources;
use crate::providers::ChatMessage;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, LazyLock};

/// Compiled prompts kept for the lookup (most recent last).
const MAX_PROMPTS: usize = 32;
/// Tasks whose notes are kept; the oldest is dropped past this.
const MAX_TASKS: usize = 128;
/// Reducer notes kept per task between two request rows.
const MAX_REDUCER_NOTES: usize = 32;
/// The text a cleared tool result starts with inside its message, and how far
/// into the message it is looked for (the message is a small JSON envelope
/// whose `content` member comes first).
const STUB_MARKER: &str = "[tool result cleared:";
const STUB_WINDOW: usize = 160;

struct RegisteredPrompt {
    /// The task that compiled the prompt: the same task that sends it.
    owner: TaskKey,
    sha256: String,
    chars: usize,
    content: Arc<str>,
    sections: Arc<[PromptSectionFingerprint]>,
}

static PROMPTS: LazyLock<Mutex<VecDeque<Arc<RegisteredPrompt>>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Remember the manifest of a compiled prompt. The text is kept only to
/// recognise a request whose system message is this prompt followed by
/// something a surface appended; it never leaves the process.
pub(crate) fn register_prompt(compiled: &CompiledSystemPrompt) {
    if compiled.content.is_empty() {
        return;
    }
    let owner = TaskKey::current();
    let mut prompts = PROMPTS.lock();
    if let Some(at) = prompts.iter().position(|entry| entry.sha256 == compiled.sha256) {
        prompts.remove(at);
    }
    if prompts.len() >= MAX_PROMPTS {
        prompts.pop_front();
    }
    prompts.push_back(Arc::new(RegisteredPrompt {
        owner,
        sha256: compiled.sha256.clone(),
        chars: compiled.chars,
        content: Arc::from(compiled.content.as_str()),
        sections: Arc::from(compiled.sections.as_slice()),
    }));
}

/// How a request's system message relates to a registered prompt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SystemMatch {
    /// The message is the compiled prompt.
    Exact,
    /// The message starts with the compiled prompt; the rest was appended.
    Prefix,
}

fn find_prompt(system: &str, system_sha256: Option<&str>) -> Option<(Arc<RegisteredPrompt>, SystemMatch)> {
    let prompts = PROMPTS.lock();
    if let Some(sha256) = system_sha256
        && let Some(entry) = prompts.iter().rev().find(|entry| entry.sha256 == sha256)
    {
        return Some((Arc::clone(entry), SystemMatch::Exact));
    }
    // An appended text: only a prompt this task compiled, and the longest of
    // them, so another session's shorter prompt is never taken for it.
    let owner = TaskKey::current();
    prompts
        .iter()
        .filter(|entry| entry.owner == owner && system.starts_with(&*entry.content))
        .max_by_key(|entry| entry.content.len())
        .map(|entry| (Arc::clone(entry), SystemMatch::Prefix))
}

/// The key a task's notes are filed under: the tokio task, or the thread when
/// the caller is not inside a spawned task.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TaskKey {
    Task(tokio::task::Id),
    Thread(std::thread::ThreadId),
}

impl TaskKey {
    fn current() -> Self {
        tokio::task::try_id().map_or_else(|| Self::Thread(std::thread::current().id()), Self::Task)
    }
}

/// What one reducer did in one ladder call.
#[derive(Clone, Copy)]
pub(crate) struct ReducerNote {
    reducer: &'static str,
    tier: Tier,
    outcome: Option<Outcome>,
}

impl ReducerNote {
    pub(crate) const fn ran(reducer: &'static str, tier: Tier, outcome: Outcome) -> Self {
        Self {
            reducer,
            tier,
            outcome: Some(outcome),
        }
    }

    /// A step the ladder considered and found not needed.
    pub(crate) const fn skipped(reducer: &'static str, tier: Tier) -> Self {
        Self {
            reducer,
            tier,
            outcome: None,
        }
    }

    fn to_json(self) -> Value {
        json!({
            "reducer": self.reducer,
            "tier": self.tier.name(),
            "applied": self.outcome.is_some(),
            "reason": self.outcome.map(|outcome| outcome.reason.name()),
            "items_removed": self.outcome.map(|outcome| outcome.removed.items),
            "tokens_removed": self.outcome.and_then(|outcome| outcome.removed.tokens),
            "bytes_removed": self.outcome.and_then(|outcome| outcome.removed.bytes),
        })
    }
}

/// What a surface put around the live user message, as counts. `request_chars`
/// is the character count of the message as the surface built it; the record
/// uses the note only when the message that is sent still has that size.
#[derive(Clone, Default)]
pub(crate) struct LiveAttachment {
    /// The run the message belongs to.
    pub(crate) run_id: Option<String>,
    pub(crate) request_chars: usize,
    pub(crate) prefix_chars: Option<usize>,
    pub(crate) tail_chars: Option<usize>,
    pub(crate) events: usize,
    pub(crate) memories: usize,
    pub(crate) documents: usize,
    /// Stored attachments replayed into the earlier messages, when the history
    /// loader counted them.
    pub(crate) attachments_replayed: Option<usize>,
}

impl LiveAttachment {
    pub(crate) fn new(request_chars: usize, sources: &AttachmentSources) -> Self {
        Self {
            request_chars,
            events: sources.events.len(),
            memories: sources.memories.len(),
            documents: sources.documents.len(),
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct TaskNotes {
    /// The run the notes below belong to.
    run_id: Option<String>,
    reducers: Vec<ReducerNote>,
    live: Option<LiveAttachment>,
}

#[derive(Default)]
struct Notes {
    tasks: HashMap<TaskKey, TaskNotes>,
    order: VecDeque<TaskKey>,
}

impl Notes {
    fn of(&mut self, key: TaskKey) -> &mut TaskNotes {
        if !self.tasks.contains_key(&key) {
            if self.order.len() >= MAX_TASKS
                && let Some(oldest) = self.order.pop_front()
            {
                self.tasks.remove(&oldest);
            }
            self.order.push_back(key);
        }
        self.tasks.entry(key).or_default()
    }
}

static NOTES: LazyLock<Mutex<Notes>> = LazyLock::new(|| Mutex::new(Notes::default()));

/// Record what one ladder call did, for the next request row of this task.
pub(crate) fn note_reducers(notes: Vec<ReducerNote>) {
    if notes.is_empty() {
        return;
    }
    let mut all = NOTES.lock();
    let task = all.of(TaskKey::current());
    task.reducers.extend(notes);
    let excess = task.reducers.len().saturating_sub(MAX_REDUCER_NOTES);
    if excess > 0 {
        task.reducers.drain(..excess);
    }
}

/// Start the notes of run `run_id` on this task. Notes of another run, or of
/// an unknown one, are dropped; the notes of the same run stay (a surface
/// notes its attachment and runs its first ladder before the engine opens).
pub(crate) fn open_turn(run_id: Option<&str>) {
    let mut all = NOTES.lock();
    let task = all.of(TaskKey::current());
    if run_id.is_none() || task.run_id.as_deref() != run_id {
        *task = TaskNotes {
            run_id: run_id.map(str::to_owned),
            ..TaskNotes::default()
        };
    }
}

/// Record the attachment of this task's live user message, which opens its run.
pub(crate) fn note_live_attachment(note: LiveAttachment) {
    open_turn(note.run_id.clone().as_deref());
    NOTES.lock().of(TaskKey::current()).live = Some(note);
}

/// Take what the round's ladders ran and the live note of `run_id`. Notes of
/// another run are never returned. The reducer notes are cleared; the live
/// note stays for the later rounds of the same run.
fn take_notes(run_id: Option<&str>) -> (Vec<ReducerNote>, Option<LiveAttachment>) {
    let mut all = NOTES.lock();
    let Some(task) = all.tasks.get_mut(&TaskKey::current()) else {
        return (Vec::new(), None);
    };
    if run_id.is_none() || task.run_id.as_deref() != run_id {
        return (Vec::new(), None);
    }
    (std::mem::take(&mut task.reducers), task.live.clone())
}

/// The facts of one request that the engine already holds.
pub(crate) struct RequestFacts<'a> {
    /// The messages as they are sent.
    pub(crate) messages: &'a [ChatMessage],
    /// The run the request belongs to; notes of another run are not used.
    pub(crate) run_id: Option<&'a str>,
    /// Hash and character count of the system message that is sent.
    pub(crate) system_sha256: Option<&'a str>,
    pub(crate) system_chars: usize,
    /// The tool snapshot hash and the counts the engine measured.
    pub(crate) tool_surface_sha256: &'a str,
    pub(crate) tools_published: usize,
    pub(crate) tools_deferred: usize,
}

fn segments_json(entry: &RegisteredPrompt) -> Value {
    Value::Array(
        entry
            .sections
            .iter()
            .map(|section| {
                json!({
                    "slot": section.slot,
                    "source": section.name,
                    "stable": !section.volatile,
                    "included": section.included,
                    "chars": section.chars,
                    "bytes": section.bytes,
                    "sha256": section.sha256,
                })
            })
            .collect(),
    )
}

fn is_stub(message: &ChatMessage) -> bool {
    let bytes = message.content.as_bytes();
    let window = bytes.get(..STUB_WINDOW).unwrap_or(bytes);
    window.windows(STUB_MARKER.len()).any(|chunk| chunk == STUB_MARKER.as_bytes())
}

/// The `provenance` object of the compiled-request event.
pub(crate) fn request_provenance(facts: &RequestFacts<'_>) -> Value {
    let system = facts.messages.iter().find(|message| message.role == "system");
    let found = system.and_then(|message| find_prompt(&message.content, facts.system_sha256));
    let (segments, segments_match, extra_chars) = match &found {
        Some((entry, kind)) => (
            segments_json(entry),
            Value::from(if *kind == SystemMatch::Exact { "exact" } else { "prefix" }),
            Value::from(facts.system_chars.saturating_sub(entry.chars)),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };

    // The live message is the last user message; what follows it is this
    // turn's own tool rounds.
    let live_at = facts.messages.iter().rposition(|message| message.role == "user");
    let mut by_role: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut history_chars, mut history_bytes, mut stubbed, mut rounds) = (0usize, 0usize, 0usize, 0usize);
    let (mut turn_messages, mut turn_chars) = (0usize, 0usize);
    let mut previous_was_tool = false;
    for (index, message) in facts.messages.iter().enumerate() {
        if message.role == "system" || Some(index) == live_at {
            previous_was_tool = false;
            continue;
        }
        let in_history = live_at.is_none_or(|live| index < live);
        if in_history {
            let role = match message.role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                "tool" => "tool",
                _ => "other",
            };
            *by_role.entry(role).or_default() += 1;
            history_chars = history_chars.saturating_add(message.content.chars().count());
            history_bytes = history_bytes.saturating_add(message.content.len());
            if message.role == "tool" {
                if !previous_was_tool {
                    rounds += 1;
                }
                if is_stub(message) {
                    stubbed += 1;
                }
            }
        } else {
            turn_messages += 1;
            turn_chars = turn_chars.saturating_add(message.content.chars().count());
        }
        previous_was_tool = message.role == "tool";
    }

    let (reducer_notes, note) = take_notes(facts.run_id);
    let live = live_at.and_then(|index| facts.messages.get(index)).map_or(Value::Null, |message| {
        let chars = message.content.chars().count();
        let attachment = note.as_ref().filter(|note| note.request_chars == chars);
        json!({
            "chars": chars,
            "bytes": message.content.len(),
            "attachment": attachment.map(|note| json!({
                "prefix_chars": note.prefix_chars,
                "tail_chars": note.tail_chars,
                "events": note.events,
                "memories": note.memories,
                "documents": note.documents,
            })),
        })
    });

    json!({
        "version": 1,
        "segments": segments,
        "segments_match": segments_match,
        "system_extra_chars": extra_chars,
        "tools": {
            "published": facts.tools_published,
            "deferred": facts.tools_deferred,
            "surface_sha256": facts.tool_surface_sha256,
        },
        "history": {
            "messages": by_role,
            "tool_rounds": rounds,
            "stubbed_results": stubbed,
            "attachments_replayed": note.as_ref().and_then(|note| note.attachments_replayed),
            "chars": history_chars,
            "bytes": history_bytes,
            "after_live": { "messages": turn_messages, "chars": turn_chars },
        },
        "live": live,
        "reducers": reducer_notes.into_iter().map(ReducerNote::to_json).collect::<Vec<_>>(),
        "hooks": [],
    })
}
