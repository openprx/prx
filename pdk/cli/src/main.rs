use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
mod detect;

use commands::{build, new, pack, test, validate};

/// PRX WASM Plugin CLI — create, build, validate, test, and pack plugins.
#[derive(Parser)]
#[command(
    name = "prx-plugin",
    version,
    about = "Manage PRX WASM plugins",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new plugin project from a template
    New(new::NewArgs),
    /// Build the plugin in the current directory
    Build(build::BuildArgs),
    /// Validate a compiled .wasm file
    Validate(validate::ValidateArgs),
    /// Run tests for the plugin in the current directory
    Test(test::TestArgs),
    /// Pack plugin.wasm + plugin.toml into a .prxplugin archive
    Pack(pack::PackArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::New(args) => new::run(args),
        Commands::Build(args) => build::run(args),
        Commands::Validate(args) => validate::run(args),
        Commands::Test(args) => test::run(args),
        Commands::Pack(args) => pack::run(args),
    }
}
