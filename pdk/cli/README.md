# prx-plugin CLI

Command-line tool for managing PRX WASM plugins.

## Installation

```bash
cd pdk/cli
cargo build --release
# Binary: target/release/prx-plugin
```

## Commands

### `prx-plugin new <name>`

Create a new plugin project from a template.

```bash
prx-plugin new my-weather-tool --lang rust --capability tool
prx-plugin new my-hook --lang python --capability hook
prx-plugin new my-middleware --lang javascript --capability middleware
prx-plugin new my-cron-job --lang go --capability cron
```

**Options:**
- `--lang rust|python|javascript|go` (default: `rust`)
- `--capability tool|hook|middleware|cron` (default: `tool`)

Templates are sourced from the PDK `templates/` directories when available,
falling back to embedded templates for standalone CLI installations.

### `prx-plugin build`

Build the plugin in the current directory. Language is auto-detected:

| File           | Language   | Build command                              |
|----------------|------------|--------------------------------------------|
| `Cargo.toml`   | Rust       | `cargo component build [--release]`        |
| `go.mod`       | Go         | `tinygo build -target wasm32-wasip2 -o plugin.wasm .` |
| `package.json` | JavaScript | `npx tsc && npx jco componentize ...`      |
| `pyproject.toml` / `setup.py` | Python | `componentize-py componentize plugin.py -o plugin.wasm` |

```bash
prx-plugin build           # debug build (Rust)
prx-plugin build --release # release build (Rust)
```

### `prx-plugin validate <file.wasm>`

Validate a compiled `.wasm` file.

```bash
prx-plugin validate plugin.wasm
prx-plugin validate          # defaults to ./plugin.wasm
```

Checks performed:
- Valid WASM magic bytes (`\0asm`)
- WASM Component vs Module detection
- `plugin.toml` parsing and manifest validation
- Required export names for the declared capability type

### `prx-plugin test`

Run the plugin's language-specific test suite.

```bash
prx-plugin test
```

| Language   | Test command  |
|------------|---------------|
| Rust       | `cargo test`  |
| Go         | `go test ./...` |
| JavaScript | `npm test`    |
| Python     | `pytest`      |

If `plugin.wasm` exists, a basic WASM load check (magic bytes + size) is also performed.

### `prx-plugin pack`

Pack `plugin.wasm` + `plugin.toml` into a `.prxplugin` archive (tar.gz).

```bash
prx-plugin pack
prx-plugin pack --output dist/my-tool-v1.0.prxplugin
```

Archive contents:
```
my-tool-0.1.0.prxplugin  (tar.gz)
├── plugin.wasm
├── plugin.toml
├── README.md          (if present)
├── LICENSE            (if present)
├── CHANGELOG.md       (if present)
└── checksums.sha256
```

## Capability Types

| Capability   | Required exports           |
|--------------|---------------------------|
| `tool`       | `get-spec`, `execute`     |
| `hook`       | `on-request`, `on-response` |
| `middleware` | `handle`                  |
| `cron`       | `run`                     |

## Prerequisites

Install the required toolchain for your chosen language:

- **Rust**: `cargo install cargo-component`
- **Go**: [TinyGo](https://tinygo.org/getting-started/install/)
- **JavaScript**: `npm install -g @bytecodealliance/jco`
- **Python**: `pip install componentize-py`
