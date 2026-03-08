use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lang {
    Rust,
    Python,
    Javascript,
    Go,
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Javascript => "javascript",
            Lang::Go => "go",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Capability {
    Tool,
    Hook,
    Middleware,
    Cron,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Capability::Tool => "tool",
            Capability::Hook => "hook",
            Capability::Middleware => "middleware",
            Capability::Cron => "cron",
        })
    }
}

#[derive(Parser, Debug)]
pub struct NewArgs {
    /// Plugin project name
    pub name: String,

    /// Programming language for the plugin
    #[arg(long, short, default_value = "rust")]
    pub lang: Lang,

    /// Plugin capability type
    #[arg(long, short, default_value = "tool")]
    pub capability: Capability,
}

/// Replace template placeholders in `content`.
fn render(content: &str, name: &str, capability: &str) -> String {
    let plugin_name = name.to_lowercase().replace(' ', "-");
    let tool_name = plugin_name.clone();
    let description = format!("A PRX {} plugin", capability);

    content
        // Python-style placeholders
        .replace("{{plugin_name}}", &plugin_name)
        .replace("{{plugin_description}}", &description)
        .replace("{{tool_name}}", &tool_name)
        .replace("{{tool_description}}", &description)
        .replace("{{author}}", "")
        // JS/Go-style placeholders
        .replace("{{PLUGIN_NAME}}", &plugin_name)
        .replace("{{PLUGIN_DESCRIPTION}}", &description)
        .replace("{{TOOL_NAME}}", &tool_name)
        .replace("{{TOOL_DESCRIPTION}}", &description)
        .replace("{{AUTHOR}}", "")
        .replace("{{WIT_PATH}}", "../../wit")
        .replace("{{PDK_VERSION}}", "0.1.0")
        // Go text/template style (used in go templates)
        .replace("{{.PluginName}}", &plugin_name)
}

// ── Embedded fallback templates ────────────────────────────────────────────

fn rust_tool_plugin_toml(name: &str) -> String {
    format!(
        r#"[plugin]
name = "{name}"
version = "0.1.0"
description = "A PRX tool plugin"
author = ""
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "{name}"
description = "Describe what this tool does"

[permissions]
required = ["log"]
optional = ["kv", "config", "http-outbound", "memory", "events"]

[resources]
max_fuel = 100_000_000
max_memory_mb = 16
max_execution_time_ms = 5000
"#
    )
}

fn rust_tool_cargo_toml(name: &str) -> String {
    format!(
        r#"[workspace]

[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
prx-pdk = {{ path = "../../rust/prx-pdk" }}
wit-bindgen = {{ version = "0.51", default-features = false, features = ["macros"] }}

[package.metadata.component]
package = "prx:plugin@0.1.0"
"#
    )
}

fn rust_tool_src_lib_rs(name: &str) -> String {
    let struct_name = to_pascal_case(name);
    format!(
        r#"use prx_pdk::{{export_plugin, host, ToolResult}};

struct {struct_name};

export_plugin!({struct_name});

impl prx_pdk::Guest for {struct_name} {{
    fn get_spec() -> String {{
        serde_json::json!({{
            "name": "{name}",
            "description": "Describe what this tool does",
            "params": {{
                "type": "object",
                "properties": {{
                    "input": {{
                        "type": "string",
                        "description": "Input value"
                    }}
                }},
                "required": ["input"]
            }}
        }})
        .to_string()
    }}

    fn execute(args_json: String) -> ToolResult {{
        host::log::info("execute called");

        let args: serde_json::Value = match serde_json::from_str(&args_json) {{
            Ok(v) => v,
            Err(e) => {{
                return ToolResult {{
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                }};
            }}
        }};

        let input = args["input"].as_str().unwrap_or("").to_string();

        ToolResult {{
            success: true,
            output: format!("Processed: {{input}}"),
            error: None,
        }}
    }}
}}
"#
    )
}

fn rust_hook_src_lib_rs(name: &str) -> String {
    let struct_name = to_pascal_case(name);
    format!(
        r#"use prx_pdk::{{export_plugin, host, HookResult}};

struct {struct_name};

export_plugin!({struct_name});

impl prx_pdk::HookGuest for {struct_name} {{
    fn on_request(request_json: String) -> HookResult {{
        host::log::info("on-request called");
        HookResult {{
            action: "continue".to_string(),
            modified_request: None,
            error: None,
        }}
    }}

    fn on_response(response_json: String) -> HookResult {{
        host::log::info("on-response called");
        HookResult {{
            action: "continue".to_string(),
            modified_request: None,
            error: None,
        }}
    }}
}}
"#
    )
}

fn python_tool_plugin_py(name: &str) -> String {
    format!(
        r#"""
{name} — PRX Tool Plugin
"""

from __future__ import annotations
from prx_pdk import ToolResult, host, prx_tool


@prx_tool(
    name="{name}",
    description="Describe what this tool does",
    params={{
        "type": "object",
        "properties": {{
            "input": {{"type": "string", "description": "Input value"}},
        }},
        "required": ["input"],
    }},
)
def execute(input: str) -> ToolResult:
    host.log.info("execute called")
    return ToolResult(success=True, output=f"Processed: {{input}}")
"#
    )
}

fn python_tool_plugin_toml(name: &str) -> String {
    format!(
        r#"[plugin]
name = "{name}"
version = "0.1.0"
description = "A PRX tool plugin"
author = ""
capability = "tool"

[permissions]
required = []
"#
    )
}

fn js_tool_plugin_ts(name: &str) -> String {
    format!(
        r#"import {{ log, resultOk, resultErr }} from "@prx/pdk";
import type {{ ToolSpec, PluginResult }} from "@prx/pdk";

export function getSpec(): ToolSpec {{
  return {{
    name: "{name}",
    description: "Describe what this tool does",
    params: {{
      type: "object",
      properties: {{
        input: {{ type: "string", description: "Input value" }},
      }},
      required: ["input"],
    }},
  }};
}}

export function execute(argsJson: string): PluginResult {{
  log.info("execute called");
  const args = JSON.parse(argsJson);
  return resultOk(`Processed: ${{args.input}}`);
}}
"#
    )
}

fn js_tool_package_json(name: &str) -> String {
    format!(
        r#"{{
  "name": "{name}",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "scripts": {{
    "build": "tsc",
    "componentize": "jco componentize dist/plugin.js --wit ../../wit --world tool -o plugin.wasm",
    "build:wasm": "npm run build && npm run componentize"
  }},
  "dependencies": {{
    "@prx/pdk": "0.1.0"
  }},
  "devDependencies": {{
    "@bytecodealliance/jco": "^1.6.0",
    "typescript": "^5.0.0"
  }}
}}
"#
    )
}

fn js_tool_plugin_toml(name: &str) -> String {
    format!(
        r#"[plugin]
name = "{name}"
version = "0.1.0"
description = "A PRX tool plugin"
author = ""
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "{name}"
description = "Describe what this tool does"

[permissions]
required = ["log"]
optional = ["kv", "config"]
"#
    )
}

fn js_tsconfig() -> &'static str {
    r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ES2022",
    "moduleResolution": "bundler",
    "strict": true,
    "outDir": "dist",
    "declaration": true
  },
  "include": ["src/**/*"]
}
"#
}

fn go_tool_main_go(name: &str) -> String {
    format!(
        r#"// {name} is a PRX tool plugin.
//
// Build:
//
//	tinygo build -target wasm32-wasip2 -o plugin.wasm .
package main

import (
	"encoding/json"
	"unsafe"

	"github.com/openprx/prx-pdk-go/host/log"
)

//go:wasmexport get-spec
func getSpec() (ptr *uint8, length uint32) {{
	log.Debug("get-spec called")
	b := []byte(toolSpecJSON())
	return &b[0], uint32(len(b))
}}

//go:wasmexport execute
func execute(inputPtr *uint8, inputLen uint32) (outPtr *uint8, outLen uint32) {{
	log.Info("execute called")

	inputBytes := unsafe.Slice(inputPtr, inputLen)
	var args map[string]interface{{}}
	if err := json.Unmarshal(inputBytes, &args); err != nil {{
		return result(false, "", err.Error())
	}}

	input, _ := args["input"].(string)
	return result(true, "Processed: "+input, "")
}}

func toolSpecJSON() string {{
	return `{{
  "name": "{name}",
  "description": "Describe what this tool does",
  "params": {{
    "type": "object",
    "properties": {{
      "input": {{"type": "string", "description": "Input value"}}
    }},
    "required": ["input"]
  }}
}}`
}}

type pluginResult struct {{
	Success bool   `json:"success"`
	Output  string `json:"output"`
	Error   string `json:"error,omitempty"`
}}

func result(success bool, output, errMsg string) (*uint8, uint32) {{
	r := pluginResult{{Success: success, Output: output, Error: errMsg}}
	b, _ := json.Marshal(r)
	return &b[0], uint32(len(b))
}}

func main() {{}}
"#
    )
}

fn go_tool_go_mod(name: &str) -> String {
    format!(
        r#"module github.com/openprx/prx-pdk-go/plugins/{name}

go 1.22

require github.com/openprx/prx-pdk-go v0.1.0
"#
    )
}

fn go_tool_plugin_toml(name: &str) -> String {
    format!(
        r#"[plugin]
name = "{name}"
version = "0.1.0"
description = "A PRX tool plugin"
author = ""
wasm = "plugin.wasm"

[[capabilities]]
type = "tool"
name = "{name}"
description = "Describe what this tool does"

[permissions]
required = ["log"]
optional = ["kv", "config"]
"#
    )
}

fn go_tool_build_sh(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
tinygo build -target wasm32-wasip2 -o plugin.wasm .
echo "Built plugin.wasm for {name}"
"#
    )
}

// ── Helper: PascalCase ─────────────────────────────────────────────────────

fn to_pascal_case(s: &str) -> String {
    s.split(['-', '_', ' '])
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect()
}

// ── Template file copy helper ──────────────────────────────────────────────

/// Try to copy template files from PDK templates directory.
/// Returns Ok(true) if templates were found and copied, Ok(false) if not found.
fn try_copy_from_pdk_templates(
    cli_dir: &Path,
    lang: &str,
    capability: &str,
    dest: &Path,
    name: &str,
) -> Result<bool> {
    // CLI lives at pdk/cli/, templates at pdk/<lang>/templates/<capability>/
    let template_dir = cli_dir
        .parent()
        .unwrap_or(cli_dir)
        .join(lang)
        .join("templates")
        .join(capability);

    if !template_dir.exists() {
        return Ok(false);
    }

    for entry in fs::read_dir(&template_dir)? {
        let entry = entry?;
        let src = entry.path();
        if src.is_file() {
            let fname = src
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("template file has no name: {}", src.display()))?
                .to_string_lossy();
            // Strip .tmpl suffix for destination
            let dest_name = if fname.ends_with(".tmpl") {
                fname.trim_end_matches(".tmpl").to_string()
            } else {
                fname.to_string()
            };

            let content = fs::read_to_string(&src)?;
            let rendered = render(&content, name, capability);
            fs::write(dest.join(&dest_name), rendered)?;
        }
    }
    Ok(true)
}

// ── Main command runner ────────────────────────────────────────────────────

pub fn run(args: NewArgs) -> Result<()> {
    let name = &args.name;
    let lang = args.lang;
    let cap = args.capability;

    let dest = Path::new(name);
    if dest.exists() {
        bail!("Directory '{}' already exists", name);
    }
    fs::create_dir_all(dest)?;

    println!(
        "{} new plugin '{}' (lang={}, capability={})",
        "Creating".green().bold(),
        name.cyan(),
        lang.to_string().yellow(),
        cap.to_string().yellow(),
    );

    // Try to locate CLI directory for template lookup
    let cli_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| Path::new(".").to_path_buf());

    let lang_str = lang.to_string();
    let cap_str = cap.to_string();

    let used_pdk_templates =
        try_copy_from_pdk_templates(&cli_dir, &lang_str, &cap_str, dest, name)
            .unwrap_or(false);

    if used_pdk_templates {
        println!("  {} PDK templates", "Using".dimmed());
    } else {
        // Use embedded templates
        println!("  {} embedded templates", "Using".dimmed());
        write_embedded_templates(dest, name, lang, cap)?;
    }

    // Always write a README if not already present
    let readme = dest.join("README.md");
    if !readme.exists() {
        fs::write(
            &readme,
            format!(
                "# {name}\n\nA PRX {cap_str} plugin written in {lang_str}.\n\n## Build\n\nSee the PRX plugin documentation.\n"
            ),
        )?;
    }

    println!(
        "{} Plugin '{}' created in ./{}/",
        "Done".green().bold(),
        name.cyan(),
        name,
    );
    println!(
        "  Next: cd {name} && prx-plugin build"
    );

    Ok(())
}

fn write_embedded_templates(
    dest: &Path,
    name: &str,
    lang: Lang,
    cap: Capability,
) -> Result<()> {
    match (lang, cap) {
        // ── Rust ──────────────────────────────────────────────────────────
        (Lang::Rust, Capability::Tool) => {
            fs::write(dest.join("Cargo.toml"), rust_tool_cargo_toml(name))?;
            fs::write(dest.join("plugin.toml"), rust_tool_plugin_toml(name))?;
            let src_dir = dest.join("src");
            fs::create_dir_all(&src_dir)?;
            fs::write(src_dir.join("lib.rs"), rust_tool_src_lib_rs(name))?;
        }
        (Lang::Rust, Capability::Hook) => {
            fs::write(dest.join("Cargo.toml"), rust_tool_cargo_toml(name))?;
            fs::write(dest.join("plugin.toml"), rust_tool_plugin_toml(name))?;
            let src_dir = dest.join("src");
            fs::create_dir_all(&src_dir)?;
            fs::write(src_dir.join("lib.rs"), rust_hook_src_lib_rs(name))?;
        }
        (Lang::Rust, _) => {
            // Middleware / cron — fall back to tool skeleton
            fs::write(dest.join("Cargo.toml"), rust_tool_cargo_toml(name))?;
            fs::write(dest.join("plugin.toml"), rust_tool_plugin_toml(name))?;
            let src_dir = dest.join("src");
            fs::create_dir_all(&src_dir)?;
            fs::write(src_dir.join("lib.rs"), rust_tool_src_lib_rs(name))?;
        }

        // ── Python ────────────────────────────────────────────────────────
        (Lang::Python, _) => {
            fs::write(dest.join("plugin.py"), python_tool_plugin_py(name))?;
            fs::write(dest.join("plugin.toml"), python_tool_plugin_toml(name))?;
        }

        // ── JavaScript / TypeScript ───────────────────────────────────────
        (Lang::Javascript, _) => {
            let src_dir = dest.join("src");
            fs::create_dir_all(&src_dir)?;
            fs::write(src_dir.join("plugin.ts"), js_tool_plugin_ts(name))?;
            fs::write(dest.join("package.json"), js_tool_package_json(name))?;
            fs::write(dest.join("tsconfig.json"), js_tsconfig())?;
            fs::write(dest.join("plugin.toml"), js_tool_plugin_toml(name))?;
        }

        // ── Go ────────────────────────────────────────────────────────────
        (Lang::Go, _) => {
            fs::write(dest.join("main.go"), go_tool_main_go(name))?;
            fs::write(dest.join("go.mod"), go_tool_go_mod(name))?;
            fs::write(dest.join("plugin.toml"), go_tool_plugin_toml(name))?;
            let build_sh = dest.join("build.sh");
            fs::write(&build_sh, go_tool_build_sh(name))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&build_sh)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&build_sh, perms)?;
            }
        }
    }
    Ok(())
}
