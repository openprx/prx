//! Owner-scoped discovery with bounded input, output, enumeration and cooperative time.
//! This projection never loads bundles, experience bodies, suites or private approvals.
use super::consolidate::{PATTERNS_DIR, verify_content};
use super::generation::{ACTIVE_DIR, ActiveBinding};
use super::knowledge::{KnowledgePattern, PatternId, PatternIndex, PatternIndexEntry};
use super::product::*;
use super::store::{MAX_RECORD_BYTES, SkillLifecycleStore, check_private, record_digest};
use super::types::{SCHEMA_VERSION, SkillId, SkillRevision};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

const SKILLS: usize = 16;
const PATTERNS: usize = 8;
const DIRECTORY_ENTRIES: u32 = 512;
const INDEX_ENTRIES: usize = 1024;
const INPUT_BYTES: u64 = 2 * 1024 * 1024;
const OUTPUT_BYTES: usize = 32 * 1024;
const WORK_TIME: Duration = Duration::from_millis(500);

struct ReadBudget {
    deadline: Instant,
    bytes: u64,
}
impl ReadBudget {
    fn alive(&self) -> Result<(), &'static str> {
        if Instant::now() >= self.deadline {
            Err("cooperative scan deadline reached")
        } else if self.bytes >= INPUT_BYTES {
            Err("catalog input byte limit reached")
        } else {
            Ok(())
        }
    }
    fn read<T: DeserializeOwned>(&mut self, root: &Path, path: &Path) -> Result<T, &'static str> {
        self.alive()?;
        // Validate every store-relative component; final open also refuses symlinks.
        let relative = path.strip_prefix(root).map_err(|_| "record is outside target store")?;
        let mut checked = root.to_path_buf();
        check_private(root).map_err(|_| "target store is unsafe")?;
        for component in relative.components() {
            self.alive()?;
            checked.push(component);
            check_private(&checked).map_err(|_| "catalog record unavailable or unsafe")?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options.open(path).map_err(|_| "catalog record unavailable")?;
        let metadata = file.metadata().map_err(|_| "catalog metadata unavailable")?;
        let remaining = INPUT_BYTES - self.bytes;
        if !metadata.is_file() || metadata.len() > MAX_RECORD_BYTES || metadata.len() >= remaining {
            return Err("catalog record exceeds input bounds");
        }
        let cap = remaining.min(MAX_RECORD_BYTES + 1);
        let mut bytes = Vec::new();
        (&mut file)
            .take(cap)
            .read_to_end(&mut bytes)
            .map_err(|_| "catalog read failed")?;
        self.bytes += bytes.len() as u64;
        self.alive()?;
        if bytes.len() as u64 > MAX_RECORD_BYTES || bytes.len() as u64 == cap {
            return Err("catalog record exceeds input bounds");
        }
        serde_json::from_slice(&bytes).map_err(|_| "catalog record cannot be decoded")
    }
}
impl ExperimentCatalog {
    fn empty() -> Self {
        Self {
            state: ExperimentCatalogState::Complete,
            skills: Vec::new(),
            next_after_skill: None,
            observations: vec!["candidate IDs only; plan revalidates eligibility and provenance".into()],
            scanned_entries: 0,
            input_bytes: 0,
        }
    }
    fn issue(&mut self, issue: &'static str) {
        self.state = ExperimentCatalogState::Partial;
        if self.observations.len() < 8 && !self.observations.iter().any(|item| item == issue) {
            self.observations.push(issue.into());
        }
    }
}
fn short(value: &str, cap: usize) -> String {
    value
        .chars()
        .scan(0usize, |size, ch| {
            *size += ch.len_utf8();
            (*size <= cap).then_some(ch)
        })
        .collect()
}
impl SkillLifecycleStore {
    pub(super) fn experiment_catalog(&self, request: &ExperimentCatalogRequest) -> ExperimentCatalog {
        let mut budget = ReadBudget {
            deadline: Instant::now() + WORK_TIME,
            bytes: 0,
        };
        let mut output = ExperimentCatalog::empty();
        // The original shared owner lock; no new authority, catalog or budget registry.
        let _lock = match self.lock_store(true, WORK_TIME) {
            Ok(lock) => lock,
            Err(_) => {
                output.issue("target owner read lock unavailable");
                output.state = ExperimentCatalogState::Unavailable;
                return output;
            }
        };
        match self.catalog_page(request, &mut budget, &mut output) {
            Ok(()) => {}
            Err(issue) => output.issue(issue),
        }
        output.input_bytes = budget.bytes;
        if output.state == ExperimentCatalogState::Partial && output.skills.is_empty() {
            output.state = ExperimentCatalogState::Unavailable;
        }
        output
    }
    fn catalog_page(
        &self,
        request: &ExperimentCatalogRequest,
        budget: &mut ReadBudget,
        output: &mut ExperimentCatalog,
    ) -> Result<(), &'static str> {
        budget.alive()?;
        let directory = self.root().join(ACTIVE_DIR);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => {
                check_private(&directory).map_err(|_| "active directory is unsafe")?;
                entries
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("active directory unavailable"),
        };
        let mut ids = BTreeSet::new();
        for entry in entries {
            budget.alive()?;
            // Include invalid names and the single overflow probe in scan accounting.
            output.scanned_entries += 1;
            if output.scanned_entries > DIRECTORY_ENTRIES {
                return Err("active directory scan limit reached; no complete page available");
            }
            let entry = entry.map_err(|_| "active directory entry unavailable")?;
            let name = entry.file_name();
            let name = name.to_str().ok_or("active directory name unavailable")?;
            if let Some(stem) = name.strip_suffix(".json") {
                match SkillId::parse(stem) {
                    Ok(id) => {
                        ids.insert(id);
                    }
                    Err(_) => output.issue("unexpected active directory entry skipped"),
                }
            } else {
                output.issue("unexpected active directory entry skipped");
            }
        }
        let pattern_entries = self.catalog_index(budget, output);
        for id in ids
            .into_iter()
            .filter(|id| request.after_skill.as_ref().is_none_or(|after| id > after))
        {
            if output.skills.len() == SKILLS {
                output.issue("skill page limit reached");
                output.next_after_skill = output.skills.last().map(|skill| skill.skill_id.clone());
                break;
            }
            budget.alive()?;
            let result = self.catalog_skill(&id, &pattern_entries, budget, output);
            let skill = match result {
                Ok(skill) => skill,
                Err(issue) => {
                    output.issue(issue);
                    continue;
                }
            };
            output.skills.push(skill);
            // Reserve room for cursor, counters and final observations; JSON escaping counts.
            if serde_json::to_vec(output)
                .map_err(|_| "catalog output cannot be encoded")?
                .len()
                > OUTPUT_BYTES - 2048
            {
                output.skills.pop();
                output.issue("encoded output page limit reached");
                output.next_after_skill = output.skills.last().map(|skill| skill.skill_id.clone());
                break;
            }
        }
        Ok(())
    }
    fn catalog_index(
        &self,
        budget: &mut ReadBudget,
        output: &mut ExperimentCatalog,
    ) -> BTreeMap<SkillId, Vec<PatternIndexEntry>> {
        let mut grouped: BTreeMap<SkillId, Vec<PatternIndexEntry>> = BTreeMap::new();
        let index: PatternIndex = match budget.read(self.root(), &self.root().join(PATTERNS_DIR).join("index.json")) {
            Ok(index) => index,
            Err(issue) => {
                output.issue(issue);
                return grouped;
            }
        };
        if index.schema_version != SCHEMA_VERSION || &index.owner_scope != self.owner_scope() {
            output.issue("pattern index owner or schema mismatch");
            return grouped;
        }
        if index.entries.len() > INDEX_ENTRIES {
            output.issue("pattern index scan limit reached");
        }
        for entry in index.entries.into_iter().take(INDEX_ENTRIES) {
            if let Err(issue) = budget.alive() {
                output.issue(issue);
                break;
            }
            if let Some(skill) = &entry.skill_id {
                grouped.entry(skill.clone()).or_default().push(entry);
            }
        }
        grouped
    }
    fn catalog_skill(
        &self,
        id: &SkillId,
        entries: &BTreeMap<SkillId, Vec<PatternIndexEntry>>,
        budget: &mut ReadBudget,
        output: &mut ExperimentCatalog,
    ) -> Result<ExperimentCatalogSkill, &'static str> {
        let binding: ActiveBinding = budget.read(
            self.root(),
            &self.root().join(ACTIVE_DIR).join(format!("{}.json", id.as_str())),
        )?;
        if binding.schema_version != SCHEMA_VERSION
            || &binding.owner_scope != self.owner_scope()
            || binding.skill_id != *id
        {
            return Err("active binding owner, identity or schema mismatch");
        }
        let record: SkillRevision = budget.read(
            self.root(),
            &self
                .root()
                .join("revisions")
                .join(id.as_str())
                .join(binding.revision_id.as_str())
                .join("revision.json"),
        )?;
        if record.schema_version != SCHEMA_VERSION
            || &record.owner_scope != self.owner_scope()
            || record.skill_id != *id
            || record.revision_id != binding.revision_id
            || record_digest(&record).map_err(|_| "revision digest unavailable")? != binding.record_digest
        {
            return Err("active revision does not match pinned identity");
        }
        let mut skill = ExperimentCatalogSkill {
            skill_id: id.clone(),
            display_name: short(&record.display_name, 128),
            description: short(&record.description, 256),
            patterns: Vec::new(),
        };
        for entry in entries.get(id).into_iter().flatten() {
            if skill.patterns.len() == PATTERNS {
                output.issue("candidate pattern window limit reached");
                break;
            }
            let pattern: KnowledgePattern = match budget.read(
                self.root(),
                &self
                    .root()
                    .join(PATTERNS_DIR)
                    .join(format!("{}.json", entry.pattern_id)),
            ) {
                Ok(pattern) => pattern,
                Err(issue) => {
                    output.issue(issue);
                    continue;
                }
            };
            if pattern.schema_version != SCHEMA_VERSION
                || &pattern.owner_scope != self.owner_scope()
                || pattern.pattern_id != entry.pattern_id
                || pattern.record_id != entry.pattern_id.as_str()
                || pattern.skill_id.as_ref() != Some(id)
                || PatternIndexEntry::of(&pattern) != *entry
                || verify_content(&pattern).is_err()
                || PatternId::from_digest(&pattern.content_digest()) != pattern.pattern_id
            {
                output.issue("candidate pattern identity or content mismatch");
                continue;
            }
            if pattern.usable_for_proposal() {
                skill.patterns.push(ExperimentCatalogPattern {
                    pattern_id: pattern.pattern_id.to_string(),
                    statement: short(&pattern.statement, 256),
                });
            }
        }
        Ok(skill)
    }
}
