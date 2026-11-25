# Outbound connections

This page lists every way OpenPRX (`prx`, `prx-node`) contacts another host:
what triggers it, where it goes, what is sent, whether it is on by default, and
which setting controls it. It is written from the source tree; the file that
implements each row is named so it can be checked. A statement that could not
be confirmed from code says "not verified".

## What PRX does not do

- No analytics, usage statistics or telemetry reporting to any PRX or
  third-party service.
- No crash reporting.
- No installation identifier and no machine identifier is generated or sent
  with requests, with one operator-initiated exception: `prx auth login
  --provider openai-codex` creates a random identifier file
  (`<state dir>/openai-host-id`) and sends its value to OpenAI as
  `ext_agent_host_id` in the sign-in URL (row 7).
- No update check. PRX never asks a server whether a newer version exists.
  Upgrades are done by the operator with the installer (row 24).
- No remote configuration. PRX never downloads configuration, feature flags or
  model catalogs on its own: the model catalog is embedded in the binary and
  changes only with a release.

The words "telemetry" in the source (`decision_engine` telemetry store,
fitness telemetry) name local files in the workspace; they are not sent
anywhere.

## A fresh install does nothing outbound

A fresh `prx init` followed by a running daemon with no provider, no channel,
no tunnel, no MCP server and no plugin configured makes no outbound
connection. Every row below is either off until the operator configures it,
or runs only when the operator (or, for tools, a model turn that needs a
configured provider) starts it. The "Fresh install" column answers the
question for each row; the answer is "no" for all of them.

## Conventions

- `prx/<version>` is the default `User-Agent` (`PRX_USER_AGENT` in
  `src/lib.rs`, the Cargo package version). Other `User-Agent` values are
  listed in [User-Agent and identifying headers](#user-agent-and-identifying-headers).
- "Content" is the class of data sent: *prompt* is the system prompt, history,
  tool results and tool schemas of a turn; *credential* is the API key, token
  or signature that authenticates the call.
- `[proxy]` (see [Configuration](configuration.md#outbound-proxy-proxy)) routes
  PRX-managed HTTP through a proxy. Inherited `HTTP_PROXY`/`HTTPS_PROXY`
  variables are honoured by clients that do not disable proxies. `web_fetch`
  and remote media downloads connect directly and ignore both.

## Table of outbound connections

| # | Flow | Trigger | Destination | What is sent | Default | Controlled by | Fresh install |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | Model providers (chat, streaming, tools) | A turn on a configured provider (`prx chat`, `prx go`, gateway, channels, cron, Xin, delegated children) | The provider's own API host from the provider catalog (for example `api.openai.com`, `api.anthropic.com`, `openrouter.ai`, `generativelanguage.googleapis.com`, `api.x.ai`), or the `api_url` / `custom:<url>` endpoint the operator set. `src/providers/mod.rs`, `src/providers/info.rs` | Prompt and credential. `User-Agent: prx/<version>`. OpenRouter and `custom:` OpenAI-compatible endpoints also send `HTTP-Referer` and `X-Title` (row 8). Gemini sends the key as a `?key=` query parameter; Anthropic as `x-api-key` and `anthropic-version`; others as `Authorization: Bearer` | Off until `default_provider` and a credential are set | `default_provider`, `api_url`, `[reliability] fallback_providers`, `[agents.<name>]`, `[[model_routes]]`, `[providers] app_identification` | No |
| 2 | Model discovery: `prx models refresh` | The operator runs it | The named or default provider's model-list endpoint (`/models` on the provider host; local servers for `ollama`, `llamacpp`, `litellm`, `vllm`). `src/providers/model_discovery.rs` | Credential of that provider (configured key, stored profile, or its standard environment variable) where the provider needs one. `User-Agent: prx/<version>` | Does nothing, and says so, when no provider is configured and none is named | `--provider`, `default_provider` | No |
| 3 | Model discovery: `prx doctor models` | The operator runs it | By default only the providers configured in this install (default provider, fallbacks, named agents, model routes, embedding routes, stored credentials). With `--all-providers`, every catalog provider's model-list endpoint, several of them unauthenticated | As row 2. A key from the process environment is sent only to a provider that is configured or named with `--provider`; a provider that is not configured never receives one, even with `--all-providers` | Configured providers only; one line lists the providers probed and the ones skipped | `--provider`, `--all-providers`, `--use-cache` | No |
| 4 | Onboarding model fetch | The operator picks a provider in the `prx init` wizard | The selected provider's model-list endpoint | As row 2 | Interactive only | The wizard | No |
| 5 | `prx go -k <key>` | The operator runs it | The provider named by `--provider`, or inferred from an explicit key prefix: `sk-ant-` Anthropic, `sk-or-` OpenRouter, `sk-` OpenAI. A key with any other prefix is not sent anywhere; the command asks for `--provider` | Credential and prompt, as row 1 | Interactive only | `-p/--provider`, `-k`, `--from-env`, `--from-stdin` | No |
| 6 | Doctor probes | `prx doctor` with an embedding endpoint configured | The configured custom embedding endpoint (`HEAD`, then `OPTIONS`, 2 s timeout, no body). `src/doctor/implementation/section_1.rs` | No credential, no body, no `User-Agent` set by PRX | Only with a custom embedding endpoint | `[memory]` embedding settings | No |
| 7 | OAuth and device-code logins | `prx auth login --provider openai-codex`, `xai`, `openrouter`; MiniMax token refresh | `auth.openai.com`, `auth.x.ai`, `openrouter.ai` (`/auth`, `/api/v1/auth/keys`), `account.minimax.io` / `account.minimaxi.com`. `src/auth/*.rs`, `src/providers/oauth.rs` | PKCE challenge and state, OAuth client id, authorization code, refresh token. OpenAI sign-in also sends a random per-install `ext_agent_host_id` (stored in `<state dir>/openai-host-id`) and `agent_name_hint`. Codex requests then carry `originator: pi`, `chatgpt-account-id` and `OpenAI-Beta: responses=experimental` to `chatgpt.com/backend-api/codex/responses` | Off until the operator logs in | `prx auth` | No |
| 8 | App identification headers | Rows 1 and 2 for OpenRouter, and every request to a `custom:` OpenAI-compatible endpoint | Same destination as the request | `HTTP-Referer: https://github.com/openprx/prx` and `X-Title: OpenPRX`; `[providers.endpoint_headers]` values for a custom endpoint | On | `[providers] app_identification = false` removes both headers everywhere (the `User-Agent` stays); `[providers.endpoint_headers]` | No |
| 9 | Messaging channels | A configured channel is started (`prx daemon`, `prx channel start`) | Telegram `api.telegram.org`; Discord `discord.com` and `wss://gateway.discord.gg`; Slack `slack.com`; WhatsApp Cloud `graph.facebook.com`; Lark/Feishu `open.larksuite.com` / `open.feishu.cn`; DingTalk `api.dingtalk.com`; QQ `bots.qq.com`, `api.sgroup.qq.com`; Linq `api.linqapp.com`; Mattermost and Nextcloud Talk at the configured server URL. IRC, e-mail, Signal and iMessage use the servers or local programs the operator configures. `src/channels/*.rs` | Channel credential, the message text and attachments the agent sends, and polling requests. Telegram file URLs embed the bot token. Per-channel request headers: not verified | Off: the channel list is empty | `[channels_config.<name>]`; `[proxy]` selectors `channel.*` | No |
| 10 | Inbound webhook receiver | An external system calls the gateway | None outbound. `src/webhook/mod.rs` is the receiver of authenticated events; replies, if any, go through the channel in row 9 | Not applicable | Off | `[webhook]` | No |
| 11 | Hook webhook action | A configured hook event fires | The `url` of the hook action. `src/hooks/webhook.rs` | The hook event envelope, redacted by `redacted_hook_payload`, with `x-webhook-signature` and `x-webhook-timestamp` headers when a secret is configured; custom headers from the action. One retry after 500 ms for a transport failure, 5xx or 429. Failures log an error class, never the URL, a header value or the body | Off: no hooks configured | Hook configuration (`src/hooks/config.rs`); `[proxy]` selector `hook.webhook` | No |
| 12 | Tunnels | `[tunnel]` is set and the gateway starts | The vendor binary PRX launches (`cloudflared`, `tailscale`, `ngrok`) or the operator's `custom` command contacts its vendor. What those programs send is outside PRX: not verified. `src/tunnel/*.rs` | Whatever the launched program sends | `provider = "none"` | `[tunnel] provider` | No |
| 13 | MCP servers | A configured server is started or called | A `stdio` server runs the operator's command; an HTTP server is called at its configured `url`. `src/tools/mcp.rs`, `src/config/schema/integrations.rs` | Tool-call arguments and the server's configured `env` values (credential) | Off: `servers` is empty | `[mcp.servers.<name>]`, `[mcp] workspace_servers` | No |
| 14 | `web_search` tool | A model turn calls the tool | DuckDuckGo `html.duckduckgo.com` (default) or Brave `api.search.brave.com`. `src/tools/web_search_tool.rs` | The search query. DuckDuckGo: a Chrome-style `User-Agent` string (see below). Brave: `X-Subscription-Token` and `Accept: application/json`, no `User-Agent` set by PRX | Registered always; runs only when a model turn calls it | `[web_search] provider`, `brave_api_key` | No |
| 15 | `web_fetch` tool | A model turn calls the tool | The URL the model chooses, public hosts only (private and loopback addresses are refused; redirects are re-checked). `src/tools/web_fetch.rs` | A `GET` with `User-Agent: Mozilla/5.0 (compatible; PRX/1.0; +https://openprx.dev)`. No credential. It connects directly and ignores proxies | Registered always; runs only when called | `[browser] allowed_domains` (empty means any public host) | No |
| 16 | `http_request` tool | A model turn calls the tool | The URL the model chooses, public hosts, restricted by `[browser] allowed_domains`. `src/tools/http_request.rs` | The method, headers and body the model supplies, with `User-Agent: prx/<version>`. Routed through the proxy only when `tool.http_request` is listed in `proxy.services` | Registered always; runs only when called | `[http_request]`, `[browser] allowed_domains`, `[proxy]` | No |
| 17 | Other built-in tools | A model turn calls them | `composio`: `backend.composio.dev` (needs `composio.api_key`); `pushover`: `api.pushover.net`; media and artifact downloads from a remote URL (`src/media/artifact.rs`, direct connection, no proxy, no `User-Agent`); audio transcription at `[media] audio_ollama_url` (default `http://localhost:11434`) | Tool arguments and the tool's credential | Composio only with a key; Pushover only with its credentials | Tool sections | No |
| 18 | Skills sync | `prx skills sync`, or the agent's `skills_manage` action `sync` | The repositories in `[skills] sync_repositories`; default `https://github.com/besoeasy/open-skills` and `https://github.com/openclaw/openclaw`. `src/skills/community_sync.rs` | `git clone --depth 1` or `git pull --ff-only`: Git's own `User-Agent`. Each URL is printed before it is contacted. An empty list does nothing and says so | Never at startup or on a catalog load; only on the command or action | `[skills] sync_repositories` | No |
| 19 | Skills install and update | `prx skills install <git url>`, `skills_manage` `install` / `update` | The Git URL given (a local path contacts nothing). `src/skills/mod.rs` | `git clone --depth 1`: Git's own `User-Agent` | On request only | The command or action | No |
| 20 | Skills discovery | `GET /api/skills/discover` on the gateway | `api.github.com/search/repositories`. `src/skills/discovery.rs`, `src/gateway/api/skills.rs` | The search query and the skill-discovery `User-Agent` (see [User-Agent and identifying headers](#user-agent-and-identifying-headers)). A `Bearer` token only when `[skills] github_token` is set; the `GITHUB_TOKEN` environment variable is never used | On request only; unauthenticated | `[skills] github_token` | No |
| 21 | Embeddings | Memory indexing and recall with a remote embedding provider | `api.openai.com`, `openrouter.ai/api/v1`, or a `custom:` endpoint. `src/memory/embeddings.rs` | The text to embed and a credential. The default `local` provider is an in-process hash embedding and contacts nobody | `memory.embedding_provider = "local"` | `[memory] embedding_provider` | No |
| 22 | Observability | Events are emitted | `otel`/`otlp`: the configured OTLP endpoint, or `http://localhost:4318` when none is set. `prometheus`: nothing is pushed; `/metrics` is served by the local gateway for pull. `log`: local log. `src/observability/*.rs`, `src/gateway/implementation/section_2.rs` | OTLP spans and metrics. Provider labels have URL user information removed before every backend | `backend = "none"` | `[observability] backend`, `otel_endpoint`, `otel_service_name` | No |
| 23 | Advisory decision engine remote backend | A turn with `[decision_engine] enabled = true` and a remote backend | The configured endpoint (a System One compatible endpoint, `api.typesafe.ai`, or OpenRouter Decisions). `src/decision_engine/prediction/*.rs` | The decision request (prompt-derived analysis input) and a credential. `User-Agent: prx/<version>` | `enabled = false`; `remote_enabled = false` | `[decision_engine]`, `[decision_engine.backend]` | No |
| 24 | Installers | The operator runs `install.sh`, `install.ps1` or `scripts/bootstrap.sh` | `github.com/openprx/prx/releases` (release archive and its `.sha256`, verified before install); `github.com/openprx/prx` for a source build; `static.rust-lang.org` and `sh.rustup.rs` when Rust must be installed. `install.sh`, `scripts/bootstrap.sh` | `curl` or `git` requests with their own default `User-Agent`; no identifier is added. `install.ps1`: not verified | Operator action | The installer | No |
| 25 | Self-evolution judge | The evolution scheduler runs with the skills lifecycle enabled | The model provider in row 1: the judge and proposer are ordinary model calls on the configured provider. `src/self_system/evolution/judge.rs`, `src/daemon/mod.rs` | Evolution evidence as a prompt, credential as row 1 | Off: runs only when `[skills_lifecycle] enabled = true` | `[skills_lifecycle]`, `[self_system]` | No |
| 26 | WASM plugins | A plugin calls `http-outbound` or `websocket-outbound` | The URLs matched by the plugin manifest `permissions.http_allowlist`. `src/plugins/outbound.rs` | What the plugin sends. An empty allow-list denies everything; loopback, private, link-local and cloud-metadata addresses are refused, also for redirect hops | `[plugins] enabled = false` | `[plugins]`, the plugin manifest | No |
| 27 | Remote nodes | The agent uses a node (`nodes` tool) or `prx-node` is started | A configured `[[nodes.nodes]] endpoint` (HTTPS, or loopback HTTP). `src/nodes/*.rs` | JSON-RPC calls signed with the configured bearer token and HMAC secret; commands and file contents for `exec`, `read`, `write` | Off: no nodes configured | `[nodes]` | No |
| 28 | A2A peers | A handoff to a configured peer | The peer URL. Which requests the client sends: not verified (`src/messaging/a2a/`). `discovery_advertised = false` by default; whether any code advertises to a registry: not verified | Not verified | Off | `[a2a]` | No |
| 29 | Local services | Gateway console and `prx tasks` | Loopback only (`prx tasks` talks to the local gateway) | Gateway token | Local | `[gateway]` | No |

## User-Agent and identifying headers

| Request | Header |
| --- | --- |
| Providers, model discovery, OAuth token calls, `http_request`, decision engine HTTP backends | `User-Agent: prx/<version>` |
| OpenRouter, and `custom:` OpenAI-compatible endpoints | plus `HTTP-Referer: https://github.com/openprx/prx` and `X-Title: OpenPRX` unless `[providers] app_identification = false` |
| OpenAI Codex (`chatgpt.com/backend-api/codex/responses`) | `User-Agent: prx/<version>`, `originator: pi`, `OpenAI-Beta: responses=experimental`, `chatgpt-account-id` |
| `web_fetch` | `Mozilla/5.0 (compatible; PRX/1.0; +https://openprx.dev)`. The `openprx.dev` address is text in the header; PRX does not request it |
| `web_search` with DuckDuckGo | `Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36` |
| Skills discovery | `OpenPRX-Skill` immediately followed by `Forge/0.1` (the product name is split here only because a repository scanner rejects the removed module name; the value is the constant in `src/skills/discovery.rs`) |
| Hook webhook, channels, `doctor` probe, media downloads | not set by PRX or not verified: see the file named in the row |
| Installers | the `User-Agent` of `curl` or `git` |

`https://openprx.dev/credentials/approval-grant/v2` in
`src/acl/approval_grant.rs` is a JSON-LD context identifier written into a
locally stored credential. PRX does not fetch it.

## What the system prompt tells the model provider

Row 1 sends the system prompt on every turn. It includes the host name, the
operating system, the absolute workspace path, the model name and the time
zone abbreviation (`src/context/sources/runtime.rs`, `workspace.rs`,
`datetime.rs`), plus the workspace identity files. `prx init` writes the
user's `$USER` name into `USER.md` and `BOOTSTRAP.md`
(`src/identity/scaffold.rs`); those files are then part of the prompt.

These stay on by default because an agent uses them. `[agent.prompt]
share_host_identity = false` replaces the host name with `host` and the
workspace path with `.` in the prompt. It does not change tools, which keep
real paths, and it does not edit workspace files an operator or `prx init`
already wrote.

## Checking this yourself

Route PRX through a logging proxy and list the hosts it contacts. The
ready-made recipe is in the engineering notes for the release gate; in short,
set `[proxy] https_proxy` and `http_proxy` (or inherit `HTTPS_PROXY`) to a
local CONNECT-logging proxy, start `prx daemon` with nothing configured, and
observe that the log stays empty. Direct connections (`web_fetch`, media
downloads) bypass the proxy: observe them with an operating-system tool such
as `strace -f -e trace=connect`.
