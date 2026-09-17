# Install PRX

Last verified: **July 26, 2026** for PRX 0.8.20 and Rust 1.97.1.

## Linux and macOS

Install the latest release:

```bash
curl -fsSL https://github.com/openprx/prx/releases/latest/download/install.sh | sh
```

The default destination is `~/.local/bin/prx`. If that directory is not
already in `PATH`, activate it in the current shell:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Then configure PRX:

```bash
prx onboard --interactive
```

The installer does not collect provider credentials. Onboarding is separate so
API keys are entered through PRX rather than embedded in a downloaded command.

## Windows

Run in PowerShell:

```powershell
irm https://github.com/openprx/prx/releases/latest/download/install.ps1 | iex
prx onboard --interactive
```

The PowerShell installer adds its per-user installation directory to the user
`Path`. Open a new terminal if an existing shell does not see `prx`.

## What the Installer Verifies

The binary installers:

1. Detect the operating system and CPU architecture.
2. Download the matching archive from the latest GitHub Release.
3. Download and validate the archive's SHA-256 checksum.
4. Extract only the `prx` executable.
5. Replace the destination atomically.
6. Run `prx --version` from the installed path.

Installation fails closed when the checksum is missing, malformed, or does not
match. The installer does not silently fall back to a source build.

## Supported Binary Targets

| Operating system | Architecture | Release asset |
|---|---|---|
| Linux with glibc | x86_64 | `prx-linux-amd64.tar.gz` |
| Linux with glibc | ARM64 | `prx-linux-arm64.tar.gz` |
| macOS | Intel | `prx-macos-amd64.tar.gz` |
| macOS | Apple Silicon | `prx-macos-arm64.tar.gz` |
| Windows | x86_64 | `prx-windows-amd64.zip` |

Alpine and other musl-based Linux systems must currently use a source build.
The installer reports unsupported systems explicitly instead of attempting to
run an incompatible glibc binary.

## Exact Version, Custom Directory, and Rollback

Install or roll back to an exact release:

```bash
curl -fsSL https://github.com/openprx/prx/releases/latest/download/install.sh |
  sh -s -- --version v0.8.20
```

Choose a destination:

```bash
curl -fsSL https://github.com/openprx/prx/releases/latest/download/install.sh |
  sh -s -- --install-dir "$HOME/bin"
```

Re-running the installer replaces only the `prx` executable in the selected
directory. Configuration and workspace data under `~/.openprx/` are not
modified.

Windows equivalents:

```powershell
& ([scriptblock]::Create((irm https://github.com/openprx/prx/releases/latest/download/install.ps1))) -Version v0.8.20
```

## Source Build

The repository pins Rust 1.97.1. A source install requires Rust 1.97.1 or newer,
Cargo, Git, a C/C++ build toolchain, and platform development libraries.

From a checkout:

```bash
git clone https://github.com/openprx/prx.git
cd prx
cargo build --release --locked --bin prx
install -m 0755 target/release/prx "$HOME/.local/bin/prx"
```

The Unix installer also provides an explicit source mode:

```bash
curl -fsSL https://github.com/openprx/prx/releases/latest/download/install.sh |
  sh -s -- --source
```

## Daemon Service

Installing the binary does not create or start a background service. After
onboarding, service setup remains explicit:

```bash
prx service install
prx service start
prx service status
```

The generated launchd or systemd user service preserves the installer's
absolute `PATH` entries. This keeps stdio MCP launchers such as `npx`, `uvx`,
and `bun` available to daemon-owned channel, gateway, hook, and scheduled
turns just as they are in the interactive CLI. Re-run `prx service install`
after changing the toolchain location so the service definition captures the
new path.

### Hand-written units lose the toolchain PATH

A unit written by hand (or copied from an older release) usually has no
`Environment="PATH=…"` line. systemd then starts the daemon with its own
default `PATH` — typically
`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/snap/bin` — which does not
contain `~/.cargo/bin` or `~/.local/bin`. The symptom shows up only inside the
daemon: the model runs `prx --version` through the shell tool and gets
`sh: prx: not found`, while the same command typed in a terminal works, because
the interactive shell has a different `PATH`.

`prx doctor` reports this directly. It reads the live daemon's own environment
(`/proc/<pid>/environ` on Linux, using the pid from `~/.openprx/daemon_state.json`)
and compares it against the directory the running `prx` binary lives in plus
`~/.cargo/bin`:

```text
[daemon]
  ✅ [HEALTHY] daemon process PATH resolves the deployed prx binary: /home/me/.cargo/bin:/usr/bin:/bin
```

A missing directory becomes a warning that carries the fix:

```text
[daemon]
  ⚠️  [UNKNOWN] daemon PATH does not include /home/me/.cargo/bin; shell tool cannot
      find prx — add Environment="PATH=/home/me/.cargo/bin:/usr/bin:/bin" to the unit
      or re-run `prx service install` (daemon PATH: /usr/bin:/bin)
```

Fix it either by re-running `prx service install` (which regenerates the unit
with the absolute `PATH`) or by adding the `Environment="PATH=…"` line to the
existing unit, then `systemctl --user daemon-reload` and `prx service restart`.
The check never fails the run: on non-Linux hosts, when the state file carries
no pid, or when `/proc` cannot be read, it reports a skip instead.

Apply a new binary or configuration with:

```bash
prx service restart
```

## Remove PRX

Stop and remove the service first:

```bash
prx service stop
prx service uninstall
rm -f "$HOME/.local/bin/prx"
```

This intentionally leaves `~/.openprx/` in place. Remove that directory only
when its configuration, credentials, sessions, and workspace are no longer
needed.

## Developer Bootstrap

Repository contributors can use:

```bash
./scripts/bootstrap.sh --help
```

That script supports source builds, dependency setup, and Docker onboarding. It
is not the user-facing one-click binary installer.
