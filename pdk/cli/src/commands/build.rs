use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::Path;
use std::process::Command;

use crate::detect::{detect_language, Language};

#[derive(Parser, Debug)]
pub struct BuildArgs {
    /// Build in release mode (Rust only)
    #[arg(long)]
    pub release: bool,

    /// Override language detection
    #[arg(long)]
    pub lang: Option<String>,
}

pub fn run(args: BuildArgs) -> Result<()> {
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
        "{} {} plugin...",
        "Building".green().bold(),
        lang.to_string().yellow()
    );

    match lang {
        Language::Rust => build_rust(&dir, args.release),
        Language::Go => build_go(&dir),
        Language::JavaScript => build_js(&dir),
        Language::Python => build_python(&dir),
    }
}

fn parse_lang(s: &str) -> Result<Language> {
    match s.to_lowercase().as_str() {
        "rust" => Ok(Language::Rust),
        "go" => Ok(Language::Go),
        "js" | "javascript" | "typescript" | "ts" => Ok(Language::JavaScript),
        "python" | "py" => Ok(Language::Python),
        other => bail!("Unknown language: '{}'. Valid: rust, go, javascript, python", other),
    }
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("Failed to spawn {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("Build command exited with status {}", status);
    }
    Ok(())
}

fn build_rust(dir: &Path, release: bool) -> Result<()> {
    // Verify cargo-component is available
    let check = Command::new("cargo")
        .args(["component", "--version"])
        .output();
    if check.is_err() || !check.unwrap().status.success() {
        eprintln!(
            "{} cargo-component not found. Install with:\n  cargo install cargo-component",
            "Warning:".yellow().bold()
        );
    }

    let mut cmd = Command::new("cargo");
    cmd.arg("component").arg("build").current_dir(dir);
    if release {
        cmd.arg("--release");
    }

    println!(
        "  {} cargo component build{}",
        "Running".dimmed(),
        if release { " --release" } else { "" }
    );
    run_cmd(&mut cmd)?;

    // Locate the output .wasm file
    let profile = if release { "release" } else { "debug" };
    let wasm_glob = dir.join("target/wasm32-wasip2").join(profile);
    if wasm_glob.exists() {
        let wasm = find_wasm_in(&wasm_glob);
        if let Some(w) = wasm {
            println!("  {} {}", "Output:".green(), w.display());
        }
    }
    println!("{}", "Build complete.".green().bold());
    Ok(())
}

fn build_go(dir: &Path) -> Result<()> {
    let mut cmd = Command::new("tinygo");
    cmd.args(["build", "-target", "wasm32-wasip2", "-o", "plugin.wasm", "."])
        .current_dir(dir);
    println!("  {} tinygo build -target wasm32-wasip2 -o plugin.wasm .", "Running".dimmed());
    run_cmd(&mut cmd).with_context(|| {
        "tinygo not found. Install from https://tinygo.org/getting-started/install/"
    })?;
    println!("{} plugin.wasm", "Output:".green());
    println!("{}", "Build complete.".green().bold());
    Ok(())
}

fn build_js(dir: &Path) -> Result<()> {
    // Step 1: TypeScript compile (if tsconfig.json exists)
    if dir.join("tsconfig.json").exists() {
        let mut cmd = Command::new("npx");
        cmd.args(["tsc"]).current_dir(dir);
        println!("  {} npx tsc", "Running".dimmed());
        run_cmd(&mut cmd).context("TypeScript compilation failed")?;
    }

    // Step 2: jco componentize
    let mut cmd = Command::new("npx");
    cmd.args([
        "jco",
        "componentize",
        "dist/plugin.js",
        "--wit",
        "../../wit",
        "--world",
        "tool",
        "-o",
        "plugin.wasm",
    ])
    .current_dir(dir);
    println!("  {} npx jco componentize ...", "Running".dimmed());
    run_cmd(&mut cmd).context("jco componentize failed")?;
    println!("{} plugin.wasm", "Output:".green());
    println!("{}", "Build complete.".green().bold());
    Ok(())
}

fn build_python(dir: &Path) -> Result<()> {
    // Detect entry module
    let module = if dir.join("plugin.py").exists() {
        "plugin"
    } else {
        bail!("No plugin.py found in current directory");
    };

    let mut cmd = Command::new("componentize-py");
    cmd.args([
        "--wit-path",
        "../../wit",
        "--world",
        "tool",
        "componentize",
        module,
        "-o",
        "plugin.wasm",
    ])
    .current_dir(dir);
    println!("  {} componentize-py componentize {} -o plugin.wasm", "Running".dimmed(), module);
    run_cmd(&mut cmd).with_context(|| {
        "componentize-py not found. Install with: pip install componentize-py"
    })?;
    println!("{} plugin.wasm", "Output:".green());
    println!("{}", "Build complete.".green().bold());
    Ok(())
}

fn find_wasm_in(dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir).ok()?.find_map(|entry| {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.extension()? == "wasm" {
            Some(path)
        } else {
            None
        }
    })
}
