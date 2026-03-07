use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde::Deserialize;
use std::{fs, path::PathBuf};

/// Required exports per capability type.
const TOOL_EXPORTS: &[&str] = &["get-spec", "execute"];
const HOOK_EXPORTS: &[&str] = &["on-request", "on-response"];
const MIDDLEWARE_EXPORTS: &[&str] = &["handle"];
const CRON_EXPORTS: &[&str] = &["run"];

#[derive(Parser, Debug)]
pub struct ValidateArgs {
    /// Path to the .wasm file to validate (default: plugin.wasm)
    pub file: Option<PathBuf>,
}

// ── plugin.toml types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PluginToml {
    plugin: PluginMeta,
    capabilities: Option<Vec<Capability>>,
    permissions: Option<Permissions>,
}

#[derive(Debug, Deserialize)]
struct PluginMeta {
    name: String,
    version: String,
    description: Option<String>,
    #[allow(dead_code)]
    author: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Capability {
    #[serde(rename = "type")]
    kind: String,
    #[allow(dead_code)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Permissions {
    required: Option<Vec<String>>,
    optional: Option<Vec<String>>,
}

// ── WASM magic number / section parsing ───────────────────────────────────

const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
// WASM Component magic is same prefix + version [0x0a, 0x00, 0x01, 0x00]
const WASM_COMPONENT_VERSION: [u8; 4] = [0x0a, 0x00, 0x01, 0x00];
const WASM_MODULE_VERSION: [u8; 4] = [0x01, 0x00, 0x00, 0x00];

/// Very lightweight export name extraction from a WASM binary.
/// Looks for the export section (id=7) and reads export names.
fn extract_wasm_exports(bytes: &[u8]) -> Vec<String> {
    // Skip 8-byte header
    if bytes.len() < 8 {
        return vec![];
    }
    let mut pos = 8;
    let mut exports = Vec::new();

    while pos < bytes.len() {
        if pos + 1 > bytes.len() {
            break;
        }
        let section_id = bytes[pos];
        pos += 1;

        // Read LEB128 section size
        let (section_size, leb_bytes) = read_leb128(&bytes[pos..]);
        pos += leb_bytes;
        let section_end = pos + section_size as usize;

        if section_id == 7 {
            // Export section
            let (count, leb) = read_leb128(&bytes[pos..]);
            let mut cursor = pos + leb;
            for _ in 0..count {
                if cursor >= section_end {
                    break;
                }
                // name length
                let (name_len, leb2) = read_leb128(&bytes[cursor..]);
                cursor += leb2;
                if cursor + name_len as usize > section_end {
                    break;
                }
                let name_bytes = &bytes[cursor..cursor + name_len as usize];
                cursor += name_len as usize;
                if let Ok(name) = std::str::from_utf8(name_bytes) {
                    exports.push(name.to_string());
                }
                // skip external kind (1 byte) + index (leb128)
                if cursor < section_end {
                    cursor += 1; // kind
                }
                let (_, leb3) = read_leb128(&bytes[cursor..]);
                cursor += leb3;
            }
            break; // found what we need
        }

        pos = section_end;
    }
    exports
}

fn read_leb128(bytes: &[u8]) -> (u64, usize) {
    let mut result = 0u64;
    let mut shift = 0;
    let mut count = 0;
    for &byte in bytes.iter().take(10) {
        count += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }
    (result, count)
}

// ── Main ──────────────────────────────────────────────────────────────────

pub fn run(args: ValidateArgs) -> Result<()> {
    let wasm_path = args.file.unwrap_or_else(|| PathBuf::from("plugin.wasm"));

    if !wasm_path.exists() {
        bail!("File not found: {}", wasm_path.display());
    }

    println!(
        "{} {}",
        "Validating".green().bold(),
        wasm_path.display().to_string().cyan()
    );

    let bytes = fs::read(&wasm_path)
        .with_context(|| format!("Cannot read {}", wasm_path.display()))?;

    // ── 1. Check WASM magic ──────────────────────────────────────────────
    if bytes.len() < 8 || bytes[..4] != WASM_MAGIC {
        bail!(
            "{} Not a valid WASM file (bad magic bytes)",
            "Error:".red().bold()
        );
    }
    println!("  {} Valid WASM magic bytes", "✓".green());

    // ── 2. Detect module vs component ───────────────────────────────────
    let is_component = bytes[4..8] == WASM_COMPONENT_VERSION;
    let is_module = bytes[4..8] == WASM_MODULE_VERSION;

    if is_component {
        println!("  {} WASM Component (WASIp2)", "✓".green());
    } else if is_module {
        println!(
            "  {} WASM Module (not a Component — PRX requires a WASM Component)",
            "!".yellow()
        );
    } else {
        println!("  {} Unknown WASM version bytes: {:?}", "!".yellow(), &bytes[4..8]);
    }

    // ── 3. File size check ───────────────────────────────────────────────
    let size_kb = bytes.len() as f64 / 1024.0;
    println!("  {} Size: {:.1} KB", "ℹ".blue(), size_kb);

    // ── 4. Read plugin.toml ──────────────────────────────────────────────
    let toml_path = wasm_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("plugin.toml");

    let mut capability_kind: Option<String> = None;

    if toml_path.exists() {
        println!("  {} plugin.toml found", "✓".green());
        let toml_str = fs::read_to_string(&toml_path)?;
        match toml::from_str::<PluginToml>(&toml_str) {
            Ok(manifest) => {
                validate_manifest(&manifest);
                capability_kind = manifest
                    .capabilities
                    .as_ref()
                    .and_then(|caps| caps.first())
                    .map(|c| c.kind.clone());
            }
            Err(e) => {
                println!(
                    "  {} plugin.toml parse error: {}",
                    "✗".red(),
                    e
                );
            }
        }
    } else {
        println!(
            "  {} plugin.toml not found (skipping manifest checks)",
            "!".yellow()
        );
    }

    // ── 5. Export check ──────────────────────────────────────────────────
    let exports = extract_wasm_exports(&bytes);
    let cap = capability_kind.as_deref().unwrap_or("tool");
    let required = required_exports(cap);

    if !exports.is_empty() {
        println!("  {} Exports detected: {}", "ℹ".blue(), exports.join(", "));
        let mut missing: Vec<&&str> = required
            .iter()
            .filter(|&&exp| !exports.iter().any(|e| e.contains(exp)))
            .collect();
        missing.sort();
        if missing.is_empty() {
            println!(
                "  {} All required exports present for capability '{}'",
                "✓".green(),
                cap
            );
        } else {
            println!(
                "  {} Missing exports for capability '{}': {}",
                "✗".red(),
                cap,
                missing
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    } else {
        println!(
            "  {} Could not extract export list (WASM Component format may require wasm-tools)",
            "!".yellow()
        );
    }

    println!("{}", "Validation complete.".green().bold());
    Ok(())
}

fn required_exports(cap: &str) -> &'static [&'static str] {
    match cap {
        "tool" => TOOL_EXPORTS,
        "hook" => HOOK_EXPORTS,
        "middleware" => MIDDLEWARE_EXPORTS,
        "cron" => CRON_EXPORTS,
        _ => TOOL_EXPORTS,
    }
}

fn validate_manifest(manifest: &PluginToml) {
    let meta = &manifest.plugin;
    println!("    {} name = {}", "·".dimmed(), meta.name.cyan());
    println!("    {} version = {}", "·".dimmed(), meta.version.cyan());
    if let Some(desc) = &meta.description {
        println!("    {} description = {}", "·".dimmed(), desc);
    }

    if let Some(perms) = &manifest.permissions {
        let req = perms.required.as_deref().unwrap_or(&[]);
        let opt = perms.optional.as_deref().unwrap_or(&[]);
        if !req.is_empty() {
            println!("    {} required permissions: {}", "·".dimmed(), req.join(", "));
        }
        if !opt.is_empty() {
            println!("    {} optional permissions: {}", "·".dimmed(), opt.join(", "));
        }
    }

    // Warn about missing capability declaration
    if manifest.capabilities.is_none() || manifest.capabilities.as_ref().map_or(true, |c| c.is_empty()) {
        println!(
            "    {} No [[capabilities]] declared in plugin.toml",
            "!".yellow()
        );
    }
}
