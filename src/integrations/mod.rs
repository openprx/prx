#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod registry;

use crate::capability::{CapabilityAvailability, CapabilityAvailabilityLevel};
use crate::config::Config;
use anyhow::Result;

/// Evidence available to the configuration-only integration catalog.
///
/// This is deliberately not runtime health. A configured adapter remains
/// `Configured` until an executable registry or health probe establishes more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationStatus {
    /// Adapter is known, but required configuration was not detected.
    Unconfigured,
    /// Required configuration was detected; readiness is unproven.
    Configured,
    /// A built-in executable backend is registered without external setup.
    Ready,
    /// Catalog declaration has no executable backend.
    Planned,
}

/// Integration category
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationCategory {
    Chat,
    AiModel,
    Productivity,
    MusicAudio,
    SmartHome,
    ToolsAutomation,
    MediaCreative,
    Social,
    Platform,
}

impl IntegrationCategory {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat Providers",
            Self::AiModel => "AI Models",
            Self::Productivity => "Productivity",
            Self::MusicAudio => "Music & Audio",
            Self::SmartHome => "Smart Home",
            Self::ToolsAutomation => "Tools & Automation",
            Self::MediaCreative => "Media & Creative",
            Self::Social => "Social",
            Self::Platform => "Platforms",
        }
    }

    pub const fn all() -> &'static [Self] {
        &[
            Self::Chat,
            Self::AiModel,
            Self::Productivity,
            Self::MusicAudio,
            Self::SmartHome,
            Self::ToolsAutomation,
            Self::MediaCreative,
            Self::Social,
            Self::Platform,
        ]
    }
}

/// A registered integration
pub struct IntegrationEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub category: IntegrationCategory,
    pub status_fn: fn(&Config) -> IntegrationStatus,
}

impl IntegrationEntry {
    #[must_use]
    pub fn availability(&self, config: &Config) -> CapabilityAvailability {
        match (self.status_fn)(config) {
            IntegrationStatus::Unconfigured => CapabilityAvailability::declared(format!(
                "{} is declared, but required configuration was not detected",
                self.name
            )),
            IntegrationStatus::Configured => CapabilityAvailability::configured(format!(
                "{} configuration was detected; runtime readiness has not been probed",
                self.name
            )),
            IntegrationStatus::Ready => {
                CapabilityAvailability::ready(format!("{} has an executable built-in backend registered", self.name))
            }
            IntegrationStatus::Planned => CapabilityAvailability::declared(format!(
                "{} is catalog-only; no executable backend is registered",
                self.name
            )),
        }
    }
}

/// Handle the `integrations` CLI command
pub fn handle_command(command: crate::IntegrationCommands, config: &Config) -> Result<()> {
    match command {
        crate::IntegrationCommands::List => {
            list_integrations(config);
            Ok(())
        }
        crate::IntegrationCommands::Info { name } => show_integration_info(config, &name),
    }
}

fn list_integrations(config: &Config) {
    let entries = registry::all_integrations();
    let count = |level| {
        entries
            .iter()
            .filter(|entry| entry.availability(config).level == level)
            .count()
    };

    println!("Integrations ({} total):", entries.len());
    println!(
        "  Declared: {}  Configured: {}  Ready: {}  Healthy: {}",
        count(CapabilityAvailabilityLevel::Declared),
        count(CapabilityAvailabilityLevel::Configured),
        count(CapabilityAvailabilityLevel::Ready),
        count(CapabilityAvailabilityLevel::Healthy),
    );
    println!();

    for category in IntegrationCategory::all() {
        let category_entries: Vec<_> = entries.iter().filter(|entry| entry.category == *category).collect();
        if category_entries.is_empty() {
            continue;
        }

        println!("{}:", category.label());
        for entry in category_entries {
            let availability = entry.availability(config);
            println!(
                "  {:<18} {:<12} {} — {}",
                entry.name,
                availability.level.label().to_lowercase(),
                entry.description,
                availability.reason,
            );
        }
        println!();
    }
}

fn show_integration_info(config: &Config, name: &str) -> Result<()> {
    let entries = registry::all_integrations();
    let name_lower = name.to_lowercase();

    let Some(entry) = entries.iter().find(|e| e.name.to_lowercase() == name_lower) else {
        anyhow::bail!(
            "Unknown integration: {name}. Check README for supported integrations or run `prx onboard --interactive` to configure channels/providers."
        );
    };

    let catalog_state = (entry.status_fn)(config);
    let availability = entry.availability(config);
    let (icon, label) = match availability.level {
        CapabilityAvailabilityLevel::Declared => ("⚪", "Declared"),
        CapabilityAvailabilityLevel::Configured => ("🟡", "Configured"),
        CapabilityAvailabilityLevel::Ready => ("✅", "Ready"),
        CapabilityAvailabilityLevel::Healthy => ("💚", "Healthy"),
    };

    println!();
    println!(
        "  {} {} — {}",
        icon,
        console::style(entry.name).white().bold(),
        entry.description
    );
    println!("  Category: {}", entry.category.label());
    println!("  Status:   {label}");
    println!("  Reason:   {}", availability.reason);
    println!();

    // Show setup hints based on integration
    match entry.name {
        "Telegram" => {
            println!("  Setup:");
            println!("    1. Message @BotFather on Telegram");
            println!("    2. Create a bot and copy the token");
            println!("    3. Run: prx onboard");
            println!("    4. Start: prx channel start");
        }
        "Discord" => {
            println!("  Setup:");
            println!("    1. Go to https://discord.com/developers/applications");
            println!("    2. Create app → Bot → Copy token");
            println!("    3. Enable MESSAGE CONTENT intent");
            println!("    4. Run: prx onboard");
        }
        "Slack" => {
            println!("  Setup:");
            println!("    1. Go to https://api.slack.com/apps");
            println!("    2. Create app → Bot Token Scopes → Install");
            println!("    3. Run: prx onboard");
        }
        "OpenRouter" => {
            println!("  Setup:");
            println!("    1. Get API key at https://openrouter.ai/keys");
            println!("    2. Run: prx onboard");
            println!("    Access 200+ models with one key.");
        }
        "Ollama" => {
            println!("  Setup:");
            println!("    1. Install: brew install ollama");
            println!("    2. Pull a model: ollama pull llama3");
            println!("    3. Set provider to 'ollama' in config.toml");
        }
        "iMessage" => {
            println!("  Setup (macOS only):");
            println!("    Uses AppleScript bridge to send/receive iMessages.");
            println!("    Requires Full Disk Access in System Settings → Privacy.");
        }
        "GitHub" => {
            println!("  Setup:");
            println!("    1. Create a personal access token at https://github.com/settings/tokens");
            println!("    2. Add to config: [integrations.github] token = \"ghp_...\"");
        }
        "Browser" => {
            println!("  Built-in:");
            println!("    OpenPRX can control Chrome/Chromium for web tasks.");
            println!("    Uses headless browser automation.");
        }
        "Cron" => {
            println!("  Built-in:");
            println!("    Schedule tasks in ~/.openprx/workspace/cron/");
            println!("    Run: prx cron list");
        }
        "Webhooks" => {
            println!("  Built-in:");
            println!("    HTTP endpoint for external triggers.");
            println!("    Run: prx gateway");
        }
        _ => {
            if catalog_state == IntegrationStatus::Planned {
                println!("  This integration is planned. Stay tuned!");
                println!("  Track progress: https://github.com/openprx/prx");
            }
        }
    }

    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_category_all_includes_every_variant_once() {
        let all = IntegrationCategory::all();
        assert_eq!(all.len(), 9);

        let labels: Vec<&str> = all.iter().map(|cat| cat.label()).collect();
        assert!(labels.contains(&"Chat Providers"));
        assert!(labels.contains(&"AI Models"));
        assert!(labels.contains(&"Productivity"));
        assert!(labels.contains(&"Music & Audio"));
        assert!(labels.contains(&"Smart Home"));
        assert!(labels.contains(&"Tools & Automation"));
        assert!(labels.contains(&"Media & Creative"));
        assert!(labels.contains(&"Social"));
        assert!(labels.contains(&"Platforms"));
    }

    #[test]
    fn handle_command_info_is_case_insensitive_for_known_integrations() {
        let config = Config::default();
        let first_name = registry::all_integrations()
            .first()
            .expect("registry should define at least one integration")
            .name
            .to_lowercase();

        let result = handle_command(crate::IntegrationCommands::Info { name: first_name }, &config);

        assert!(result.is_ok());
    }

    #[test]
    fn handle_command_info_returns_error_for_unknown_integration() {
        let config = Config::default();
        let result = handle_command(
            crate::IntegrationCommands::Info {
                name: "definitely-not-a-real-integration".into(),
            },
            &config,
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown integration"));
    }

    #[test]
    fn handle_command_list_succeeds() {
        let config = Config::default();
        let result = handle_command(crate::IntegrationCommands::List, &config);

        assert!(result.is_ok());
    }
}
