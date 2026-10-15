use super::input::TuiInput;
use super::keys::{InputOutcome, KeyDispatch};
use super::{AtPathCandidate, SlashModelCandidate, SlashProviderModelCatalog};
use crate::chat::commands::{CommandArgCandidate, CommandArgSource, CommandSpec, command_specs};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenuState {
    pub filter: String,
    pub entries: Vec<SlashMenuEntry>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashMenuEntry {
    pub label: String,
    pub args_hint: String,
    pub description: String,
    pub insert_text: String,
    pub append_space: bool,
}

pub(crate) struct SlashMenuSources<'a> {
    pub live_sessions: &'a [crate::chat::sessions::SwitcherEntry],
    pub saved_sessions: &'a [crate::chat::session::SavedSessionPickerEntry],
    pub provider_model_catalog: &'a [SlashProviderModelCatalog],
    pub at_path_candidates: &'a [AtPathCandidate],
    pub current_provider: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlashCursorContext {
    Command {
        filter: String,
    },
    Argument {
        command: String,
        arg_index: usize,
        filter: String,
        previous_args: Vec<String>,
    },
    AtPath {
        filter: String,
    },
}

impl SlashMenuState {
    #[must_use]
    pub fn new(filter: &str) -> Self {
        Self {
            filter: filter.to_string(),
            entries: filtered_command_entries(filter),
            selected: 0,
        }
    }

    #[must_use]
    pub fn new_with_entries(filter: &str, entries: Vec<SlashMenuEntry>) -> Self {
        Self {
            filter: filter.to_string(),
            entries,
            selected: 0,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn refresh(&mut self, filter: &str) {
        if self.filter == filter {
            self.clamp_selected();
            return;
        }
        self.filter.clear();
        self.filter.push_str(filter);
        self.entries = filtered_command_entries(filter);
        self.selected = 0;
    }

    pub fn refresh_with_entries(&mut self, filter: &str, entries: Vec<SlashMenuEntry>) {
        if self.filter == filter && self.entries == entries {
            self.clamp_selected();
            return;
        }
        self.filter.clear();
        self.filter.push_str(filter);
        self.entries = entries;
        self.selected = 0;
    }

    pub fn clamp_selected(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        }
    }

    pub const fn select_prev(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else if self.selected == 0 {
            self.selected = self.entries.len().saturating_sub(1);
        } else {
            self.selected -= 1;
        }
    }

    pub const fn select_next(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = (self.selected + 1) % self.entries.len();
        }
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<&SlashMenuEntry> {
        self.entries.get(self.selected)
    }
}

impl SlashMenuEntry {
    pub(super) fn from_command(spec: CommandSpec) -> Self {
        Self {
            label: spec.name.to_string(),
            args_hint: spec.args_hint.to_string(),
            description: spec.description.to_string(),
            insert_text: spec.name.to_string(),
            append_space: true,
        }
    }

    pub(super) fn argument(value: impl Into<String>, description: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            args_hint: String::new(),
            description: description.into(),
            insert_text: value,
            append_space: true,
        }
    }

    pub(super) fn at_path(candidate: &AtPathCandidate) -> Self {
        Self {
            label: candidate.path.clone(),
            args_hint: String::new(),
            description: if candidate.is_dir {
                "directory".to_string()
            } else {
                "file".to_string()
            },
            insert_text: format!("@{}", candidate.path),
            append_space: !candidate.is_dir,
        }
    }
}

#[must_use]
pub fn slash_provider_model_catalog_from_config(config: &crate::config::Config) -> Vec<SlashProviderModelCatalog> {
    let mut catalog: Vec<SlashProviderModelCatalog> = Vec::new();
    if let (Some(provider), Some(model)) = (config.default_provider.as_deref(), config.default_model.as_deref()) {
        push_model_candidate(&mut catalog, provider, model, "Configured default model");
    }
    for route in &config.model_routes {
        push_model_candidate(
            &mut catalog,
            &route.provider,
            &route.model,
            format!("Model route hint: {}", route.hint),
        );
    }
    for model in &config.router.models {
        push_model_candidate(&mut catalog, &model.provider, &model.model_id, "Router model candidate");
    }
    catalog
}

pub(super) fn push_model_candidate(
    catalog: &mut Vec<SlashProviderModelCatalog>,
    provider: &str,
    model: &str,
    description: impl Into<String>,
) {
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() || model.is_empty() {
        return;
    }
    let description = description.into();
    if let Some(entry) = catalog
        .iter_mut()
        .find(|entry| entry.provider.eq_ignore_ascii_case(provider))
    {
        if !entry.models.iter().any(|candidate| candidate.name == model) {
            entry.models.push(SlashModelCandidate {
                name: model.to_string(),
                description,
            });
        }
        return;
    }
    catalog.push(SlashProviderModelCatalog {
        provider: provider.to_string(),
        models: vec![SlashModelCandidate {
            name: model.to_string(),
            description,
        }],
    });
}

pub(super) fn filtered_command_entries(filter: &str) -> Vec<SlashMenuEntry> {
    let needle = filter.trim_start_matches('/').to_ascii_lowercase();
    command_specs()
        .iter()
        .copied()
        .filter(|spec| {
            if needle.is_empty() {
                return true;
            }
            command_matches_filter(*spec, &needle)
        })
        .map(SlashMenuEntry::from_command)
        .collect()
}

pub(super) fn command_matches_filter(spec: CommandSpec, needle: &str) -> bool {
    let name = spec.name.trim_start_matches('/').to_ascii_lowercase();
    if name.contains(needle) {
        return true;
    }
    spec.aliases
        .iter()
        .any(|alias| alias.trim_start_matches('/').to_ascii_lowercase().contains(needle))
}

pub(crate) fn sync_slash_menu_for_sources(
    input: &TuiInput,
    slash_menu: &mut Option<SlashMenuState>,
    sources: SlashMenuSources<'_>,
) {
    let Some(context) = input.completion_cursor_context() else {
        *slash_menu = None;
        return;
    };
    match context {
        SlashCursorContext::Command { filter } => {
            let entries = filtered_command_entries(&filter);
            if entries.is_empty() {
                *slash_menu = None;
            } else if let Some(menu) = slash_menu.as_mut() {
                menu.refresh(&filter);
            } else {
                *slash_menu = Some(SlashMenuState::new_with_entries(&filter, entries));
            }
        }
        SlashCursorContext::Argument {
            command,
            arg_index,
            filter,
            previous_args,
        } => {
            let entries = argument_candidate_entries(&command, arg_index, &filter, &previous_args, sources);
            if entries.is_empty() {
                *slash_menu = None;
            } else if let Some(menu) = slash_menu.as_mut() {
                menu.refresh_with_entries(&filter, entries);
            } else {
                *slash_menu = Some(SlashMenuState::new_with_entries(&filter, entries));
            }
        }
        SlashCursorContext::AtPath { filter } => {
            let entries = at_path_candidate_entries(&filter, sources.at_path_candidates);
            if entries.is_empty() {
                *slash_menu = None;
            } else if let Some(menu) = slash_menu.as_mut() {
                menu.refresh_with_entries(&filter, entries);
            } else {
                *slash_menu = Some(SlashMenuState::new_with_entries(&filter, entries));
            }
        }
    }
}

pub(super) fn argument_candidate_entries(
    command: &str,
    arg_index: usize,
    filter: &str,
    previous_args: &[String],
    sources: SlashMenuSources<'_>,
) -> Vec<SlashMenuEntry> {
    let Some(spec) = command_specs()
        .iter()
        .find(|spec| spec.name == command || spec.aliases.iter().any(|alias| *alias == command))
        .copied()
    else {
        return Vec::new();
    };
    let source = if spec.name == "/provider" && arg_index == 1 {
        CommandArgSource::ProviderModels
    } else if arg_index == 0 {
        spec.arg.source
    } else {
        CommandArgSource::None
    };
    let needle = filter.to_ascii_lowercase();
    let mut entries = match source {
        CommandArgSource::None | CommandArgSource::FreeText => Vec::new(),
        CommandArgSource::Static(candidates) => static_candidate_entries(candidates),
        CommandArgSource::Themes => theme_candidate_entries(),
        CommandArgSource::LiveSessions => live_session_candidate_entries(sources.live_sessions),
        CommandArgSource::SavedSessions => saved_session_candidate_entries(sources.saved_sessions),
        CommandArgSource::Providers => provider_candidate_entries(),
        CommandArgSource::CurrentProviderModels => {
            model_candidate_entries(sources.provider_model_catalog, sources.current_provider)
        }
        CommandArgSource::ProviderModels => {
            let provider = previous_args.first().map_or("", String::as_str);
            model_candidate_entries(sources.provider_model_catalog, provider)
        }
    };
    if needle.is_empty() {
        return entries;
    }
    entries.retain(|entry| {
        entry.label.to_ascii_lowercase().contains(&needle)
            || entry.description.to_ascii_lowercase().contains(&needle)
            || entry.insert_text.to_ascii_lowercase().contains(&needle)
    });
    entries
}

pub(super) fn static_candidate_entries(candidates: &[CommandArgCandidate]) -> Vec<SlashMenuEntry> {
    candidates
        .iter()
        .map(|candidate| SlashMenuEntry::argument(candidate.value, candidate.description))
        .collect()
}

pub(super) fn theme_candidate_entries() -> Vec<SlashMenuEntry> {
    ["dark", "light", "monokai"]
        .into_iter()
        .map(|name| SlashMenuEntry::argument(name, "Chat color theme"))
        .collect()
}

pub(super) fn live_session_candidate_entries(entries: &[crate::chat::sessions::SwitcherEntry]) -> Vec<SlashMenuEntry> {
    entries
        .iter()
        .filter(|entry| !entry.is_transcript())
        .map(|entry| {
            SlashMenuEntry::argument(
                format!("#{}", entry.seq),
                format!("{} {} - {}", entry.kind, entry.status, entry.title),
            )
        })
        .collect()
}

pub(super) fn saved_session_candidate_entries(
    entries: &[crate::chat::session::SavedSessionPickerEntry],
) -> Vec<SlashMenuEntry> {
    let mut out = vec![SlashMenuEntry::argument("last", "Most recently saved session")];
    out.extend(entries.iter().map(|entry| {
        let title = if entry.title.is_empty() {
            "(untitled)".to_string()
        } else {
            entry.title.clone()
        };
        SlashMenuEntry::argument(
            entry.id.clone(),
            format!("{} turns - {} / {}", entry.turn_count, title, entry.model),
        )
    }));
    out
}

pub(super) fn provider_candidate_entries() -> Vec<SlashMenuEntry> {
    crate::providers::list_providers()
        .into_iter()
        .map(|provider| {
            let mut description = provider.display_name.to_string();
            if provider.local {
                description.push_str(" - local");
            }
            SlashMenuEntry::argument(provider.name, description)
        })
        .collect()
}

pub(super) fn model_candidate_entries(catalog: &[SlashProviderModelCatalog], provider: &str) -> Vec<SlashMenuEntry> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Vec::new();
    }
    catalog
        .iter()
        .find(|entry| entry.provider.eq_ignore_ascii_case(provider))
        .map_or_else(Vec::new, |entry| {
            entry
                .models
                .iter()
                .map(|model| SlashMenuEntry::argument(model.name.clone(), model.description.clone()))
                .collect()
        })
}

pub(super) fn at_path_candidate_entries(filter: &str, candidates: &[AtPathCandidate]) -> Vec<SlashMenuEntry> {
    let needle = filter.to_ascii_lowercase();
    candidates
        .iter()
        .filter(|candidate| {
            if needle.is_empty() {
                return true;
            }
            let path = candidate.path.to_ascii_lowercase();
            path.contains(&needle) || fuzzy_path_match(&path, &needle)
        })
        .take(50)
        .map(SlashMenuEntry::at_path)
        .collect()
}

pub(crate) fn fuzzy_path_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut chars = needle.chars();
    let Some(mut wanted) = chars.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == wanted {
            if let Some(next) = chars.next() {
                wanted = next;
            } else {
                return true;
            }
        }
    }
    false
}

pub(crate) fn dispatch_slash_menu_key_with_sources(
    input: &mut TuiInput,
    slash_menu: &mut Option<SlashMenuState>,
    key: KeyEvent,
    sources: SlashMenuSources<'_>,
) -> KeyDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
    let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
    if up || down {
        let Some(menu) = slash_menu.as_mut() else {
            return KeyDispatch::Consumed;
        };
        if up {
            menu.select_prev();
        } else {
            menu.select_next();
        }
        return KeyDispatch::Consumed;
    }

    if (key.code == KeyCode::Enter || key.code == KeyCode::Tab) && key.modifiers == KeyModifiers::NONE {
        if let Some(entry) = slash_menu.as_ref().and_then(SlashMenuState::selected_entry).cloned() {
            match input.completion_cursor_context() {
                Some(SlashCursorContext::Command { .. }) => {
                    if key.code == KeyCode::Enter && entry.args_hint.is_empty() && input.slash_command_suffix_is_empty()
                    {
                        input.replace_slash_command_token(&entry.insert_text, false);
                        let text = input.text();
                        input.record_history(text.clone());
                        input.clear();
                        *slash_menu = None;
                        return KeyDispatch::Submitted(text);
                    }
                    input.replace_slash_command_token(&entry.insert_text, true);
                }
                Some(SlashCursorContext::Argument { .. }) => {
                    input.replace_slash_argument_token(&entry.insert_text, entry.append_space);
                }
                Some(SlashCursorContext::AtPath { .. }) => {
                    input.replace_at_path_token(&entry.insert_text, entry.append_space);
                }
                None => {}
            }
        }
        sync_slash_menu_for_sources(input, slash_menu, sources);
        return KeyDispatch::Consumed;
    }

    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        if matches!(
            input.completion_cursor_context(),
            Some(SlashCursorContext::Command { .. })
        ) {
            input.clear();
        }
        *slash_menu = None;
        return KeyDispatch::Consumed;
    }

    if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        return KeyDispatch::Consumed;
    }

    match key.code {
        KeyCode::Char(_)
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End => match input.handle_key(key) {
            InputOutcome::Submitted(text) => {
                *slash_menu = None;
                KeyDispatch::Submitted(text)
            }
            InputOutcome::Cancelled => {
                *slash_menu = None;
                KeyDispatch::Cancelled
            }
            InputOutcome::Consumed | InputOutcome::Unhandled => {
                sync_slash_menu_for_sources(input, slash_menu, sources);
                KeyDispatch::Consumed
            }
            InputOutcome::Ignored => KeyDispatch::Ignored,
        },
        _ => KeyDispatch::Consumed,
    }
}
