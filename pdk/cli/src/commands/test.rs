use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::Path;
use std::process::Command;

use crate::detect::{detect_language, Language};

#[derive(Parser, Debug)]
pub struct TestArgs {
    /// Override language detection
    #[arg(long)]
    pub lang: Option<String>,

    /// Skip WASM load test even if plugin.wasm exists
    #[arg(long)]
    pub no_wasm: bool,
}

pub fn run(args: TestArgs) -> Result<()> {
    let dir = std::env::current_dir().context("Failed to get current directory")?;

    let lang = if let Some(l) = &args.lang {
        parse_lang(l)?
    } else {
        detect_language(&dir).ok_or_else(|| {
            anyhow::anyhow!(
                "Could not detect plugin language.\n\
                 Expected one of: Cargo.toml (Rust), go.mod (Go), package.json (JS), \
                 pyproject.toml / setup.py (Python)"
            )
        })?
    };

    println!(
        "{} {} plugin tests...",
        "Running".green().bold(),
        lang.to_string().yellow()
    );

    match lang {
        Language::Rust => run_rust_tests(&dir)?,
        Language::Go => run_go_tests(&dir)?,
        Language::JavaScript => run_js_tests(&dir)?,
        Language::Python => run_python_tests(&dir)?,
    }

    // WASM load test
    if !args.no_wasm {
        let wasm_path = dir.join("plugin.wasm");
        if wasm_path.exists() {
            wasm_load_check(&wasm_path)?;
        } else {
            println!(
                "  {} plugin.wasm not found — skipping WASM load test (run 'prx-plugin build' first)",
                "ℹ".blue()
            );
        }
    }

    println!("{}", "Tests complete.".green().bold());
    Ok(())
}

fn parse_lang(s: &str) -> Result<Language> {
    match s.to_lowercase().as_str() {
        "rust" => Ok(Language::Rust),
        "go" => Ok(Language::Go),
        "js" | "javascript" | "typescript" | "ts" => Ok(Language::JavaScript),
        "python" | "py" => Ok(Language::Python),
        other => anyhow::bail!(
            "Unknown language: '{}'. Valid: rust, go, javascript, python",
            other
        ),
    }
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("Failed to spawn {:?}", cmd.get_program()))?;
    if !status.success() {
        anyhow::bail!("Test command exited with status {}", status);
    }
    Ok(())
}

fn run_rust_tests(dir: &Path) -> Result<()> {
    println!("  {} cargo test", "Running".dimmed());
    run_cmd(Command::new("cargo").arg("test").current_dir(dir))
        .context("cargo test failed")?;
    Ok(())
}

fn run_go_tests(dir: &Path) -> Result<()> {
    println!("  {} go test ./...", "Running".dimmed());
    run_cmd(Command::new("go").args(["test", "./..."]).current_dir(dir))
        .context("go test failed")?;
    Ok(())
}

fn run_js_tests(dir: &Path) -> Result<()> {
    println!("  {} npm test", "Running".dimmed());
    run_cmd(Command::new("npm").arg("test").current_dir(dir))
        .context("npm test failed")?;
    Ok(())
}

fn run_python_tests(dir: &Path) -> Result<()> {
    println!("  {} pytest", "Running".dimmed());
    run_cmd(Command::new("pytest").current_dir(dir))
        .context("pytest failed (install with: pip install pytest)")?;
    Ok(())
}

/// Basic WASM load check: verify magic bytes and file is non-trivially sized.
fn wasm_load_check(wasm_path: &Path) -> Result<()> {
    println!("  {} Basic WASM load check: {}", "Running".dimmed(), wasm_path.display());

    let bytes = std::fs::read(wasm_path)
        .with_context(|| format!("Cannot read {}", wasm_path.display()))?;

    if bytes.len() < 8 {
        anyhow::bail!("plugin.wasm is too small to be valid ({} bytes)", bytes.len());
    }

    let magic = &bytes[..4];
    if magic != b"\x00asm" {
        anyhow::bail!(
            "plugin.wasm has invalid magic bytes: {:?}",
            magic
        );
    }

    let size_kb = bytes.len() as f64 / 1024.0;
    println!(
        "  {} WASM file valid ({:.1} KB)",
        "✓".green(),
        size_kb
    );
    Ok(())
}
