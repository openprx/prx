# Reproduce the local model and Office acceptance profile

This profile isolates configuration, HOME, workspace, Python packages, Node
packages and temporary files. It never copies credentials or changes the
operator's existing PRX profile. It targets macOS arm64 with Python 3.14.6,
Node 24.15.0, LibreOffice 26.8.1.1 and Poppler 26.10.0. The Python locks contain wheel hashes
for this platform; other platforms require separately resolved and verified locks.

The directories and environment are separate defaults. The native runtime
does not enforce an OS filesystem boundary around arbitrary shell commands;
an absolute path can still reach outside the profile. Keep execution traces
when validating workspace constraints.

## Create a fresh profile

Install those host prerequisites and put Node and npm on PATH. Obtain the
Office skills repository, keeping its pinned commit available:

```bash
git clone https://github.com/anthropics/skills.git /path/to/office-skills
git -C /path/to/office-skills fetch origin 53048666b05b4799081517d00e09e0a2dd688678
python3.14 scripts/local-profile/profile.py create /path/to/prx-profile \
  --skills-source /path/to/office-skills \
  --soffice /path/to/LibreOffice.app/Contents/MacOS/soffice \
  --poppler-bin /path/to/poppler/bin
```

Run from the PRX checkout. Choose a directory that does not exist. The script
archives the pinned Git commit, so uncommitted skill edits are not imported.
It installs Node packages with `npm ci --ignore-scripts` and Python packages
with `pip --require-hashes`. A failed setup remains available for diagnosis;
retry in a new directory. `profile.json` records the tool versions, skill
revision, lock hashes and LibreOffice executable hash.

The generated configuration declares `chat` and `native_tools` support for
`custom:http://127.0.0.1:18086/v1`, model `qwen38-27b-splash`. Its explicit
zero prices mean **no provider API charge for this local service**, not zero
hardware or electricity cost. Unknown-cost and capability admission checks
remain enabled. Do not reuse this price row for a paid proxy.

## Start and verify the model

Run Splash 1.3.1 with the locally provisioned
`unsloth/Qwen3.8-27B-GGUF:UD-Q6_K_L` model, context size 65536, served model
name `qwen38-27b-splash`, and loopback port 18086. Keep the weights revision
and SHA-256 in the run receipt; a model name alone does not pin model bytes.
The profile script does not download weights or start a service.

The verified target revision is `990216cf312573f2ac4060279848e0f4237600c7`;
the `incoai/Qwen3.8-27B-DFlash2` draft revision is
`015e795645c74b1a0eeef3b570031fb62e769bc5`. File hashes are recorded in
[`model-lock.json`](../scripts/local-profile/model-lock.json). Provision the
draft at that revision, then start the installed model with:

```bash
splash serve --model unsloth/Qwen3.8-27B-GGUF:UD-Q6_K_L \
  --revision 990216cf312573f2ac4060279848e0f4237600c7 \
  --draft-model /path/to/pinned-draft --offline --language-only \
  --host 127.0.0.1 --port 18086 --no-webui --max-context 65536 \
  --served-model-name qwen38-27b-splash --announce-served-name \
  --default-reasoning-effort none --queue-size 2
```

```bash
python3.14 scripts/local-profile/profile.py check /path/to/prx-profile --probe-model
python3.14 scripts/local-profile/profile.py launch /path/to/prx-profile \
  --prx /path/to/prx
```

The first command verifies packages and the served model ID. It does not
prove inference or tool execution. The second runs fullscreen `prx chat` with
an explicit environment. Only `NODE_PATH` is added to the shell environment
allowlist; no host credential variables are inherited. `soffice`, Python and
Node are available through the profile PATH.

## Office and decision-backend acceptance

In the fresh chat, exercise each installed skill (`docx`, `pptx`, `pdf`,
`xlsx`). Save a reusable script and an artifact with a fresh marker. Verify
document contents, render pages/slides, and inspect the rendering. For XLSX,
verify formulas before recalculation and cached results after a real
LibreOffice run. A successful package import or a handwritten cached value
is not this acceptance. Keep temporary output in the profile workspace.

PowerPoint MCP 2.0.7 uses its own environment pinned to MCP 1.30.0. The
generated `powerpoint-mcp.toml` is an optional operator-config fragment;
append it to `config/config.toml` before launch when testing that surface.
The setup verifies its imports. MCP handshake, tool discovery and actual
document creation/readback must be recorded separately. An Office skill
result does not validate an MCP server.

The decision backend is explicitly `builtin`. This profile **does not
activate or validate Jev**. A real Jev run requires a separately configured
endpoint and credential reference, the appropriate remote opt-in and price
evidence. Record the actual backend, attempt, usage and any fallback from
runtime events. Do not label builtin or mock outcomes as real Jev results.

For a Jev key issued by OpenRouter, select `kind = "openrouter"`. The
adapter uses `https://openrouter.ai/api/alpha/decisions`; a TypeSafe direct
endpoint requires a different credential. The current release catalog pins
`typesafe/jev-1.13` to `typesafe/jev-1.13-20260917`. Store the key under an
`openrouter` credential reference through `prx providers credential set`,
using stdin or its hidden prompt. Configure the backend and remote opt-in
through `prx causal config`; see the
[decision backend checks](configuration.md#configuration-transactions-history-and-rollback).
An expired-key response is a failed remote preflight, not a successful
decision or a reason to report builtin fallback as Jev acceptance.

For a bounded Jev acceptance run, use four candidates:

```bash
prx --config-dir /path/to/prx-profile/config causal config set policy \
  --max-candidates 4 --yes
prx --config-dir /path/to/prx-profile/config causal test --live --yes --json
```

The live test sends a small neutral fixture and may incur a provider charge.
Follow it with a real chat turn and inspect the durable decision and execution
records. In the verified profile, a 12-candidate request consumed 4,714 reported
tokens, exhausted the 4,096-token decision allowance, and correctly blocked
execution. Four candidates consumed 2,243 tokens and allowed the selected
local-model/tool path to complete. These are observations from one run, not
upper bounds or permission to disable budget checks. A snapshot mismatch must
be recorded as a failed remote attempt and an explicit fallback. Physical
usage can still be reported and charged even when the decision is rejected;
verify the spend journal separately. Missing usage must remain unknown and
must not become a fabricated zero-cost successful Jev result.

To roll back, exit the profile's chat and restore the previously selected
binary. The existing PRX profile is untouched. Stop only model processes
started for this run, after checking their identity.
