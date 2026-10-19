# Providers

See [provider management](provider-management.md) for inspection, configuration
transactions, credential references and recovery.

OpenPRX supports native and OpenAI-compatible LLM providers with model routing,
ordered fallback, token refresh, streaming, tools, and usage attribution. Each
binary embeds a validated [release model catalog](model-catalog.md) for offline
defaults, listings, and model evidence. `prx doctor models` (read-only) or
`prx models refresh --provider <id>` (cache-updating) adds current provider
discovery evidence when the provider exposes a model-list API.

Declared request parameters are resolved from the embedded catalog before
adapter serialization. Codex effort constraints and Kimi Code's fixed
temperature follow the selected provider/model, including streaming requests;
a matching endpoint URL supplies no model facts. See the
[parameter resolution contract](model-catalog.md#format-and-validation).

## Embedded release catalog baseline (2026-07-18)

| PRX provider | Curated default / current IDs | Endpoint and authoritative source | Live discovery |
|---|---|---|---|
| `kimi-code` | `k3`; `kimi-for-coding`; `kimi-for-coding-highspeed` | `https://api.kimi.com/coding/v1`; [Kimi Code models](https://www.kimi.com/code/docs/en/kimi-code/models.html) | `GET /models`, bearer token |
| `openai` | `gpt-5.6`; `gpt-5.6-terra`; `gpt-5.6-luna` | `https://api.openai.com/v1`; [OpenAI models](https://developers.openai.com/api/docs/models) | `GET /models`, bearer token |
| `openai-codex` | `gpt-5.2-codex`; `gpt-5-codex` | Responses API; [GPT-5.2 Codex](https://developers.openai.com/api/docs/models/gpt-5.2-codex) | OAuth catalog is service-managed |
| `anthropic` | `claude-sonnet-5`; `claude-opus-4-8`; `claude-haiku-4-5-20251001` | `https://api.anthropic.com`; [Claude models](https://platform.claude.com/docs/en/about-claude/models/overview) | Anthropic models API, `x-api-key` |
| `gemini` | `gemini-3.5-flash`; `gemini-3.1-pro-preview`; `gemini-3.1-flash-lite` | Google Generative Language API; [Gemini models](https://ai.google.dev/gemini-api/docs/models) | `v1beta/models?key=...` |
| `qwen` | `qwen3.7-max`; `qwen3.7-plus`; `qwen3.6-flash` | China `dashscope.aliyuncs.com`, international `dashscope-intl.aliyuncs.com`, US `dashscope-us.aliyuncs.com`; [Model Studio models](https://help.aliyun.com/en/model-studio/models) | regional `GET /compatible-mode/v1/models` |
| `openrouter` | dynamic `vendor/model` IDs | `https://openrouter.ai/api/v1`; [model catalog](https://openrouter.ai/docs/guides/overview/models) | public `GET /models`; response `model` identifies router-selected model |
| `xai` | `grok-4.20`; `grok-4.20-non-reasoning-latest` | `https://api.x.ai/v1`; [Grok 4.20](https://docs.x.ai/developers/models/grok-4.20-experimental-beta-0304) | `GET /models`, bearer token |
| `glm` / `zai` | global `glm-5`; China `glm-5.2` | global `api.z.ai`, China `open.bigmodel.cn`; [GLM-5](https://docs.z.ai/guides/llm/glm-5), [GLM-5.2](https://docs.bigmodel.cn/cn/guide/models/text/glm-5.2) | regional `GET /models` |
| `bedrock` | `anthropic.claude-sonnet-5`; `anthropic.claude-opus-4-8`; Haiku 4.5 regional ID | AWS Converse; [model/API compatibility](https://docs.aws.amazon.com/bedrock/latest/userguide/models-api-compatibility.html) | AWS regional catalog; invocation uses Converse/ConverseStream |
| `copilot` | `gpt-5.4`; `gpt-5.3-codex`; `claude-sonnet-4.6` | GitHub Copilot service; [supported models](https://docs.github.com/en/copilot/reference/ai-models/supported-models) | service-managed; OAuth token auto-refresh |
| `ollama` | installed model tags | default `http://localhost:11434`; [list models](https://docs.ollama.com/api/tags) | unauthenticated `GET /api/tags` locally |
| `llamacpp` | server-loaded model ID | default `http://localhost:8080/v1`; [llama.cpp server](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md) | `GET /v1/models`; capabilities depend on template/server flags |
| `vllm` | server-loaded model ID | default `http://localhost:8000/v1`; [vLLM OpenAI server](https://docs.vllm.ai/en/stable/serving/openai_compatible_server/) | `GET /v1/models`; tools depend on served model/parser |
| `litellm` | proxy alias or upstream model ID | default `http://localhost:4000`; [LiteLLM proxy](https://docs.litellm.ai/) | `GET /v1/models`; upstream capabilities are route-dependent |
| `huggingface` | router model ID | `https://router.huggingface.co/v1`; [chat completion](https://huggingface.co/docs/inference-providers/en/tasks/chat-completion) | `GET /v1/models`, bearer token |
| `compatible` | endpoint-defined | configured OpenAI-compatible `/v1` base URL | `GET <base>/models`; capabilities are endpoint/model-dependent |

Release entries are onboarding defaults and model evidence within the adapter
ceiling; they do not grant credentials or transport support. Live discovery
also reads the active auth profile when `api_key` is absent, and custom
llama.cpp/vLLM/LiteLLM/Hugging Face/compatible base URLs are honored for model
discovery. Kimi Code rejects model strings outside its three documented IDs
because that endpoint can silently accept or remap a mistyped model.

Catalog lifecycle controls new selections: active and deprecated chat rows are
listed, with deprecated rows labeled with their replacement when one exists;
retired rows stay hidden. Ollama Cloud has no live model-list endpoint, so an
authenticated onboarding fallback uses the release rows and places `:cloud`
models before local-only ids.

## Capability contracts

- OpenAI-compatible providers support non-streaming and streaming text and
  preserve provider reasoning fields where the upstream format exposes them.
  Tool and vision support remains model/endpoint dependent.
- Anthropic uses native tool blocks and preserves thinking blocks across tool
  loops. See [extended thinking](https://platform.claude.com/docs/en/build-with-claude/extended-thinking).
  Both streaming and non-streaming requests carry tools.
- OpenAI (direct) sends native `tools` with `tool_choice: "auto"` and parses
  streamed `tool_calls` deltas. Both streaming and non-streaming requests carry
  tools.
- Gemini sends native `functionDeclarations`, parses `functionCall`, sends
  `functionResponse` on the next turn, and converts validated image markers to
  Gemini `inlineData`. Gemini 3.x `thoughtSignature` values are retained across
  tool loops. Both streaming and non-streaming requests carry tools.
- Bedrock uses Converse/ConverseStream native content and tool blocks.
- Ollama uses its native chat/tool schema. llama.cpp, vLLM, LiteLLM, Hugging
  Face, and custom endpoints use the OpenAI-compatible adapter.
- OpenAI-compatible calls have a ten-second connection timeout but no
  whole-response deadline, in both buffered and streaming modes: a queued local
  request or healthy generation may run for hours. Runtime `idle_hang_secs`
  remains the single progress-aware stall guard.
- A transport error fails the current attempt explicitly. Responses API
  fallback is attempted only after an HTTP 404 proves that Chat Completions is
  unavailable; an ambiguous disconnect is never replayed through another API.
- Streaming retries are allowed only before any text, reasoning, or tool-call
  chunk has been emitted. After model output starts, an interruption fails the
  turn explicitly instead of replaying the request and risking duplicated text
  or tool side effects.
- Capability checks are request-mode-specific. Reliable failover exposes only
  the intersection supported by every candidate, so a fallback cannot silently
  invalidate a tool or vision request.

## Model descriptors and the resolver

PRX describes each `(provider, model)` pair with a typed descriptor: which
operations the model executes (`chat`, `embedding`, `decision_endpoint`,
`rerank`, `image_generation`, `audio_generation`, `video_generation`), which
chat features it has (`native_tools`, `vision_input`, `streaming`,
`structured_output`), and the tool dialects it speaks. Every fact is
`supported`, `unsupported` or `unverified`; an unverified fact never counts as
support. Evidence uses this precedence:

1. **The adapter table.** What the provider family's adapter can put on the
   wire at all. It is the upper bound: nothing can make an operation without an
   adapter executable. In this release chat has an adapter everywhere,
   embeddings only for `openai`, `openrouter` and `custom:` endpoints, and the
   decision endpoint, rerank and media generation have none.
2. **Negative discovery evidence.** `prx models refresh` and onboarding keep
   what a provider's model list says about each model (OpenRouter
   `architecture` and `supported_parameters`, Gemini
   `supportedGenerationMethods`). A provider record that rules out an
   operation or feature cannot be lifted by local configuration.
3. **Operator declarations** in `[[providers.models]]` (see
   [configuration.md](configuration.md#provider-model-descriptors)) override
   positive or absent discovery facts and release-catalog facts. They also set
   tool dialects where the adapter reads them.
4. **Positive discovery evidence.** A current provider record outranks the
   release baseline when no operator declaration settles the fact.
5. **The embedded release catalog.** `catalog/models.json` supplies the offline
   baseline and carries its release revision in descriptor provenance. A model
   absent from every evidence source is known by id only and remains
   unverified.

The resolver checks a selection against its descriptor before any provider is
constructed, so a refusal sends nothing: provider, then model, then
operation, then features, then dialects, and the credential last. A credential
is referenced, never copied, and is released only to the provider identity it
was issued for (the root `api_key`, `api_url` and `reliability.api_keys`
together, to the configured default provider). A model that is only
unverified keeps running when it is already selected, with a warning. A new
durable selection the operator makes accepts it too, marked unverified; a new
durable selection a model makes (an agent tool) refuses it until it is
declared or its metadata confirms it. A `hint:<route>` model is checked as
the `[[model_routes]]` destination it names, everywhere; an unknown hint, a
route whose model is itself a hint, and an empty hint or one containing
whitespace are refused. A configuration fails to load when two routes share a
hint or a route's `hint` contains whitespace (the router compares hints
exactly while the checks trim, so such a hint is selected unreliably). Hints
route in `prx chat`, `prx agent` (and `prx go --message`), cron and xin agent
jobs, gateway and channel turns, `delegate`, and `sessions_spawn` in task or
process mode. Each turn builds the selected route's provider, model and
credential together. A process worker checks the sealed route against its
pinned configuration before it sends a request. A direct-model override under
a routed parent uses the selected provider's connection: a named agent's own
credential stays with that agent, while a root child uses the root connection.
At routed-surface startup, each non-default route is resolved and built
independently. A refused or unbuildable route is skipped with a warning rather
than disabling the default or another route; selecting its `hint:` returns the
retained refusal without sending a provider request.

A root connection may configure an `api_url` or rotation keys without an
inline `api_key`. That is still an owner-bound Root selection: construction
asks the configured provider's own credential store for the missing primary
key, while the root URL and rotation material remain attached to that same
provider identity. A route with its own key is a separate credential and does
not inherit the root URL or root rotation keys. Each route is resolved and
built independently, so a refused route is retained as that hint's refusal and
does not disable the default provider or another route.

For the configured default provider the credential is chosen in this order:
any root connection material (the inline `api_key`, rotation keys, or an
`api_url`), then the credential reference `cred-<provider>` that
`prx providers credential set <provider>` stores under its default name (it
must be owned by that provider), then the provider's own credential store. An
unreadable credential store is logged as a warning and the provider's own store
is used.

Where the resolver runs. Every surface resolves before it builds a provider,
so a refusal sends nothing and names the model, the provider (URL userinfo
redacted), the evidence and the fix:

| Surface | What is resolved | Policy |
| --- | --- | --- |
| `prx chat` startup, `/model`, `/provider`; each `[[model_routes]]` destination is retained as either a ready route or a named refusal | chat | invocation |
| channel runtime start-up, channel `/model` (the route is left unchanged on a refusal); under the daemon, every hot reload that changes the default provider or model, the provider tables or the routes restarts the channel runtime, which resolves again, so a reload to a model that cannot chat is refused and the previous configuration stays active. A standalone `prx channels start` applies such changes only on restart | chat | invocation |
| gateway start-up and each configuration generation (a refused default rejects the generation; the previous one stays active) | chat | invocation |
| `delegate`, `sessions_spawn` (task and process workers) | chat | invocation |
| `prx agent` (`--provider`, `--model`), `prx go --message`, and cron and xin agent jobs, which run through it: the selection and every configured `[[model_routes]]` destination, retained as ready or refused once per run | chat | invocation |
| the evolution judge: judges with the resolved default model (the route destination for a `hint:`). A default that cannot be resolved or built leaves only the deterministic mock scorer, which sends nothing: its verdict is a deferral in every mode — a proposed Auto-mode change is not written — and the `evolution_judge` health component and a warning say so. A model judge whose call fails, whose reply is not a score, or that does not answer within `[self_system] evolution_judge_timeout_secs` (default 180) is deferred the same way (`judge_failed`). Evolution changes are written only after an accepted verdict; a deferred cycle is logged as `deferred` (see [evolution](evolution.md#judge-verdicts)). The memory layer's engine scores with the mock scorer, so it never applies in Auto mode | chat | invocation |
| `prx go` (resolved by the agent entry with `--message`, by chat start-up otherwise) | chat | invocation |
| memory embedders and `[[embedding_routes]]` (a refused embedder falls back to keyword recall) | embeddings | invocation |
| `prx onboard` quick setup and the interactive wizard; `POST /api/config` (and the Console) when the default changes | chat | operator |
| the agent `gateway` tool's `config.patch` (default, routes, agent bindings, embedding routes it changes) | chat / embeddings | model-made durable |
| the agent `cron` tool: a job's `model` on `add` and on `update`/`patch` (checked on the default provider; nothing is written on a refusal) | chat | model-made durable |

"Invocation" runs an id-only model with a warning. "Operator" is a durable
choice the operator makes: it refuses only a model the evidence rules out
(for example a decision-only model, or a model another provider's prefix
names) and accepts an id-only model marked unverified. Quick setup reads PRX's
embedded release models for the provider, and `POST /api/config` answers with a `warning`
field. "Model-made durable" refuses an id-only model until it is declared in
`[[providers.models]]` or confirmed by catalog metadata. `config.patch` never
changes `[[providers.models]]` or `[[providers.tool_dialects]]`: any patch
whose result differs from them — an addition, an edit, or a deletion through
`null` on the table or a parent key — is refused. `prx doctor` and
`prx models list` report how the configured default resolves. The `prx
agent` command's own selection is resolved when the agent loop is
restructured.

`/models <name>` in a channel keeps a spelling that selects another regional
endpoint (`/models qwen-us` is the US endpoint, not `qwen`) and never accepts
a URL. `local` and `none` are PRX's own embedders and always resolve; any
spelling of `openai` or `openrouter` builds its embedder, and a `custom:`
embedder is addressed at the URL its provider identity names.

**Model fallbacks on a declaring endpoint.** When a provider chain is built
from a resolved model (`create_provider_for`), each
`reliability.model_fallbacks` entry for that model is resolved too: it must
run the same request, and on an endpoint that reads dialect declarations
(`custom:`, vLLM, llama.cpp, LM Studio, LiteLLM) it must speak the same
tool dialect as the primary. Otherwise the chain is not built:

```
model fallback 'model-b' of 'model-a' on provider 'custom:http://127.0.0.1:8080/v1' speaks tool-schema dialect 'raw' and tool-call dialect 'Native', but 'model-a' speaks 'no_root_combinators' and 'Native'; one provider chain enforces one tool contract, so declare the same dialects for both or remove the fallback
```

Fix: declare the fallback's dialect (`[[providers.models]]` `model =
"model-b"` `tool_schema_dialect = "no_root_combinators"`), or move it off the
primary's `model_fallbacks`. Chat, CLI/cron/xin, channels, gateway, workers,
spawned children and the evolution judge all construct through the resolved
model factory, so the same configuration is refused on every entrypoint before
transport. Routed providers construct the default and every `hint:` destination
from separate resolved selections; two hints that name the same provider still
keep their own credential, model facts and fallback admission. A bad
non-default destination does not stop startup: its hint retains that exact
refusal and returns it if selected. The CLI `--model hint:<name>` path and a
channel `/models <provider>` switch while that hint is selected preserve the
same route-named refusal text.

At failover time, every fallback pair is checked against the primary turn's
admission facts before a request is sent. A fallback needs a valid cost entry
(except the existing local Ollama zero-cost case), and a turn that published
native tools skips any fallback whose exact model lacks verified native-tool
support. A rate-limited primary therefore cannot move the request onto an
unpriced or tool-less fallback.

## Streaming failure rules

A streamed reply is used only if it ended the way the provider's protocol says
a finished reply ends. Otherwise the turn fails with the cause, and **no tool
call from that reply is run**. A call PRX cannot confirm as finished never
runs as `{}`.

- **Anthropic.** A reply fails if:
  - it ends without `message_stop`;
  - it carries an in-stream `event: error` (for example `overloaded_error`);
  - its last block is a `tool_use` and the `stop_reason` is anything other
    than `tool_use`, `end_turn` or `stop_sequence` (for example
    `max_tokens`, `model_context_window_exceeded`, `refusal`, `pause_turn`);
  - its last block is a `tool_use` and it ends with no `stop_reason` at all.
- **OpenAI and OpenAI-compatible (including Copilot and OpenRouter).**
  - A reply fails if it:
    - ends without `[DONE]` while a call is still buffered;
    - on direct OpenAI, ends without `[DONE]` and without a `finish_reason`
      (OpenAI-compatible endpoints accept such a text-only reply);
    - carries an in-stream `{"error": ...}` object;
    - ends with `finish_reason: length` while a call is buffered.
  - `finish_reason: content_filter` fails the reply:
    - always on direct OpenAI;
    - on OpenAI-compatible endpoints only while a call is buffered. A
      text-only filtered answer ends as a cut answer.
  - A call that got its name but no argument bytes fails the reply unless
    `finish_reason: tool_calls` confirms it. That covers `stop`, other finish
    reasons, and `[DONE]` with no `finish_reason`.
  - Complete calls ended by `stop`, or by `[DONE]` without a `finish_reason`,
    are delivered.
  - A direct OpenAI reply that has a `finish_reason` and no call buffered is
    complete even if the server omits `[DONE]`.
  - A streamed tool-call index above 128 fails the reply.
- **In the tool loop.** A call whose arguments are not a complete JSON object
  (cut-off JSON, or a non-object value such as `[1]`) is never run. The model
  gets an error result on the same call id that says the call was not run and
  why. The call is kept in the transcript with `{}` as its arguments.
- **After output, no replay.** A streaming failure after any text, reasoning
  or tool-call chunk is never retried, compacted or replayed, whatever its
  message says. Before any output, the usual retries and fallbacks apply.
  Exceptions: a content-filter stop reported in the stream
  (`finish_reason: content_filter`, or an in-stream error with code/type
  `content_filter`/`content_policy_violation`) and a tool-call index over the
  limit are not retried on the same provider, but the next fallback candidate
  is still tried. A filter refusal returned as an HTTP 400 is an ordinary
  non-retryable error, and vendor finish reasons such as GLM `sensitive` or
  DashScope `data_inspection_failed` are not filter stops.
- **No whole-request deadline.** Direct Anthropic and OpenAI streaming
  requests use a ten-second connection timeout and no whole-response
  deadline, like OpenAI-compatible ones. `idle_hang_secs` ends a stalled turn;
  `idle_hang_secs = 0` disables that bound.
- **Abandoned streams.** When a turn is abandoned (for example by the idle
  guard), the streaming task stops and its connection is closed on Anthropic,
  OpenAI, OpenAI-compatible, Copilot and OpenRouter, directly or behind the
  retry/fallback wrapper. This holds whether or not the server has already
  answered. Gemini, Ollama, Bedrock and OpenAI Codex streams are not covered
  by this rule (a Codex stream's wait for response headers has no bound and
  is not released when the turn is abandoned; its body is bounded by an idle
  timeout, 45 s by default).

## Tool-schema dialects

A provider declares which rewrite of the published tool schemas it puts on the
wire, and the same declaration decides which document a model's tool call is
validated against — a constraint the provider strips is never enforced against
a model that was never shown it. `ToolSchemaDialect` names the rewrite:

| Dialect | Wire document |
| --- | --- |
| `raw` | The published document, byte for byte. |
| `gemini` | `SchemaCleanr::clean_for_gemini`. |
| `anthropic` | `SchemaCleanr::clean_for_anthropic` (`$ref`/`$defs`/`definitions`). |
| `no_root_combinators` | Root `allOf` of plain object fragments merged into the root; nested combinators untouched; every other root combinator refused. |

`no_root_combinators` is for endpoints that reject `allOf`, `anyOf` or `oneOf`
at the top level of a tool schema. PRX writes those keywords to express two
kinds of rule: a requirement conditional on an `action` discriminator, and a
choice between alternative required sets. Neither has a lossless expression as
a single object — merging the branches would require every branch's fields at
once, and dropping the keyword would require none of them. Rather than reshape
the contract or quietly omit the tool, the request fails before transport with
one diagnostic naming every refused tool and its keyword, the dialect, where
the dialect was declared and the configuration that would fix it (see
[Refused tools and how to exclude them](#refused-tools-and-how-to-exclude-them)).
On a current build roughly a third of the published tools are refused this way
on such an endpoint; the rest lower or pass through unchanged. Fail-closed is
the operator's decision (OQ-26): the rules are not carried into
prompt-visible descriptions to keep the tools available.

**What "merged losslessly" means, exactly.** The same document is enforced
against the model's answer, so a merge that is even slightly more permissive
than the published contract is PRX enforcing less than the tool author wrote.
A root `allOf` is merged only when *every* member is a plain object fragment:

- its keys are a subset of `type`, `properties`, `required`, `description`,
  `title` — an allowlist, so an unforeseen keyword refuses rather than being
  dropped;
- apart from the pure annotations `$comment`, `examples`, `default`,
  `deprecated`, `readOnly` and `writeOnly`, which constrain nothing and are
  dropped from the member (never hoisted onto the root) — only that member's
  prompt hint is lost, and what PRX enforces is unchanged. `$id`, `$schema`,
  `$anchor`, `$dynamicAnchor` and `$vocabulary` are not annotations (they
  rebase references or switch what the keywords mean) and stay refused;
- its `properties`, if present, is an object and its `required`, if present,
  is an array; a member of any other shape is refused rather than skipped;
- its `type` is absent or exactly `object`, and so is the root's (a member's
  `object` is carried into a root that declared none);
- it declares no `additionalProperties`, or declares literally `true`, which
  constrains nothing;
- no property name is declared differently in two members, or in a member and
  the root.

The merged document is the union of `properties` and the union of `required`
— `required` only ever grows, so no requirement can be dropped — and its
`additionalProperties` is the root's, untouched. A member's
`additionalProperties` is never hoisted: it is scoped to that member's own
`properties`, so carrying it over the union would admit exactly the names the
member forbade. For the same reason, a merge that widens the `properties` of a
root which declares `additionalProperties` is refused as well. Everything else
— `if`/`then`/`else`, `not`, a nested combinator, `$ref`, `$defs`, `enum`,
`const`, a bound, a non-object member type, a conflicting property — is
refused, naming the keyword. This matters beyond PRX's own tools: an MCP
server's `inputSchema` is forwarded verbatim into the tool contract, so the
shapes that reach this pass are not all ones PRX authored.

**The default is `raw`, for every endpoint, including `custom:<url>`.** PRX
cannot know what an arbitrary endpoint accepts, a URL is not evidence, and a
"conservative" default that lowered everything would also relax PRX's own
argument validation across the fleet. An operator declares a different dialect
per provider, or per `(provider, model)`, in `[[providers.tool_dialects]]`:

```toml
[[providers.tool_dialects]]
provider    = "custom:http://127.0.0.1:8080/v1"  # as in default_provider / fallback_providers / --provider
model       = "incoai/Qwen3.8-27B-Splash"        # optional; omitted = every model of this provider
tool_schema = "no_root_combinators"              # optional: "raw" | "no_root_combinators"
tool_call   = "xml_function_tags"                # optional: "native" | "xml_function_tags"; entries without `model` only
```

| Field | Values | Meaning |
| --- | --- | --- |
| `provider` | a `custom:<http(s) url>` endpoint, or `vllm`, `llamacpp`, `lmstudio`, `litellm` (any alias) | The provider identity the entry applies to. |
| `model` | a model id, optional | Only that model (ASCII case ignored); omitted means every model. |
| `tool_schema` | `raw` (default), `no_root_combinators` | The dialect tool schemas are put on the wire in, and validated against. |
| `tool_call` | `native` (default), `xml_function_tags` | Whether `<function=NAME>{json}</function>` in the content is read as a tool call. |

- **Scope.** Only providers whose constructor reads the declaration accept an
  entry: a `custom:<url>` OpenAI-compatible endpoint, and the four catalog
  entries that address an OpenAI-compatible server the operator runs. Any
  other provider — `gemini`, `anthropic`, `anthropic-custom:<url>`, a hosted
  catalog vendor, a `wasm:` provider, an unknown name — is refused at load
  with an error naming the entry, rather than accepted and ignored.
- **Identity, not spelling.** An entry matches the provider the way credential
  scoping does (C1's canonical identity): `custom:http://h:8080/v1`,
  `CUSTOM:http://H:8080/v1/` and a padded spelling are one endpoint; a
  different scheme, port or path is another; `v-llm` is `vllm`. A catalog
  entry is keyed by its name, whatever `api_url` points it at.
- **Most specific wins.** A model entry overrides the provider-wide entry for
  that model, so `tool_schema = "raw"` on one model pins it back to the
  published document under a provider-wide `no_root_combinators`.
- **Refused at load, never "last wins":** two entries for the same identity and
  model; an entry with neither `tool_schema` nor `tool_call`; an empty
  `provider` or `model`; `tool_call` on an entry with `model` (the response
  parser cannot tell models apart, so a per-model grammar would silently apply
  to every model of the endpoint); and any other value, including `gemini` and
  `anthropic` — those name a provider family's fixed rewrite, and declaring one
  for a single model would validate that model's calls against less than it
  was shown.
- **Hot reload.** The declarations are part of the configuration generation
  providers are rebuilt from, so an edited entry applies to providers built
  after the reload, like every other provider setting. The long-lived channel
  `image` tool resolves the provider and model catalog owned by the invoking
  generation; `sessions_spawn` uses the child provider binding admitted for
  that invocation. Provider, model, temperature, credential and capability
  declarations therefore move together after a reload.
- **Consequence of `no_root_combinators`.** Tools whose published contract uses
  a conditional or alternative root combinator are refused on that endpoint,
  and a request that includes any of them fails before transport, naming every
  one of them, the keyword and the entry: ``Declared by: `[[providers.tool_dialects]]`
  (provider '…', every model)``. See
  [Refused tools and how to exclude them](#refused-tools-and-how-to-exclude-them).
  Exclusion is by exact published tool name, and an MCP server can publish a
  new tool at runtime (`list_changed`), so a tool it adds later can start
  failing turns on that endpoint until it is excluded too.
- **One file owns the table.** Declare every entry in `config.toml` or in a
  single `config.d/` fragment. A fragment's array would replace the main
  file's wholesale, so two files declaring the table is a load error naming
  both, not a silent last-file-wins.
- **An entry nothing uses** (not the default provider, a fallback, a model
  route, a router model or an agent) still loads, with a warning naming the
  entry: it is usually a mistyped port or path.
- **Secrets in the URL.** A `custom:` URL may carry `user:password@`. The
  table then holds that credential, exactly as `default_provider` does. The
  real URL is used only to reach the endpoint. PRX redacts the userinfo, as
  `custom:http://[redacted]@host/v1`, in the places listed below. What is
  guaranteed:
  - A provider label on its own (every chain member name, config value and
    tool output below) is redacted whenever the URL parser the transport
    uses reads a user name or a password from it, whatever the password
    holds and however the separator is spelled (`http://`, `http:/`,
    `http:`, `http:\\`, `http:\`, extra slashes, tab, LF or CR anywhere).
  - Free text (an error chain, an upstream error body, a log line, a hook or
    observer text) is scanned for every `scheme:` followed by the separators
    the parser accepts: for `http`, `https`, `ws`, `wss` and `ftp` any number
    of `/` or `\`, none included, and whatever follows them (whitespace too,
    so prose such as `https: see a@b` is redacted, the safe direction); for
    any other scheme (`postgres`, `redis`, `socks5`, …) at least two. Tab, LF
    and CR inside the scheme or the separator are skipped, and they also end
    the text before a URL, as does any character that cannot be part of a
    scheme (a URL glued to `502`, `1.`, `--` or `:` is found). The userinfo
    runs to the last `@` before the first `/`, `?` or `#`, and for a special
    scheme also `\` (a non-special scheme keeps `\` in its userinfo, as in
    `postgres://CORP\svc:pw@db`), but never into a later URL that carries its
    own userinfo. An authority written with `//` that does not parse (a `/`
    inside the password) is redacted through the last `@` before the next
    such URL. The scan never panics on any input, including non-ASCII text.

    What is tested: fixed tables (every spelling above, non-ASCII text around
    `:` and `@`, `\` in a non-special userinfo, glued schemes) and two seeded
    property tests. One generates 40,000 texts mixing credentialed URLs (all
    special spellings with and without the `custom:`/`anthropic-custom:`
    prefix, non-special schemes, empty and non-empty users including
    `CORP\svc`, passwords holding quotes, spaces, `@`, `:`, `/`, `\`, `?`,
    `#` and non-ASCII characters, hosts with ports, IPv6 and IDNA) with words,
    digits, punctuation, quotes, tab, LF, CR and non-ASCII text (three-byte
    East-Asian syllables, emoji, accented letters, typographic punctuation,
    NBSP). For every text, no
    password the parser reads survives, redacting twice changes nothing and
    no marker is doubled. The other runs 30,000 arbitrary strings built from
    the characters the scanner branches on and multibyte characters, and
    asserts that redaction never panics.

    Known limits of the free-text scan:
    - a scheme glued to a preceding letter is found only when it is written
      with at least two separators: two `/`, or, for a special scheme at the
      end of the glued word, any two of `/` and `\` (`xhttps://u:pw@h`,
      `xhttps:\\u:pw@h/`, `xhttps:/\u:pw@h/` and `completionshTTp` + line
      break + `S:\\u:pw@h` are redacted; `xhttp:/u:pw@h` is not, and
      neither is prose such as `news:alice@example.com`);
    - the password of a non-special URL (`redis`, `postgres`, …) that holds
      a special scheme followed by two or more `\` and a later `@`, such as
      `redis://u:ws:\\a@b@h`, is cut at that inner URL, so the part before
      it is printed. (A `/` always ends an authority, so no other inner URL
      can sit inside a password the parser reads.) A scheme word with a colon
      and no separators inside a password (`svc:k9-wss:Q…`), or `x:\\` in a
      non-special password, is not cut;
    - over-redaction: a URL with no path followed by text holding `@` before
      any `/`, `?` or `#`, on any later line, loses that text too (this is
      also what the parser reads); likewise a bare `http:`, `https:`, `ws:`,
      `wss:` or `ftp:` word in prose (`ftp: server refused login for
      anna@corp` becomes `ftp:[redacted]@corp`);
    - idempotence: redacting twice changes nothing on every tested text; in a
      rare shape where a URL's scheme starts inside the previous URL's
      authority, the second pass can redact more (never less).

    The scan runs in time linear in the text: the positions of `@` and of
    every authority terminator are indexed once, the scheme search looks at
    most 64 bytes back from a colon, and the conservative fallback does not
    parse an authority longer than 1 KiB (it treats it as unparseable, which
    only redacts more). A scaling test checks that four times the input
    costs at most eight times the time on the adversarial shapes, and that
    1 MB error bodies of every shape are sanitized promptly. The error
    sanitizer also scrubs at most the first 16 KiB of a text, since it
    prints only 200 characters, and an upstream error body is read into
    memory up to 64 KiB.

  The places:
  - this key's errors and warnings, the failover summary and every retry
    line, the warmup and observer log lines, and Prometheus labels;
  - channel replies; the `session_status`, `gateway`, `agents_list` and
    `delegate` tool output; the vision capability refusal;
  - hook payloads, WASM lifecycle hooks and the plugin event bus;
  - route, attempt and outcome records, usage records, and the
    `<provider>/<model>` sender or recipient of conversation events (a
    sender that is an identity, such as a webhook `reply_target`, is stored
    as given);
  - `prx status`, `GET /api/status`, `prx config show`, `GET /api/config`
    (including `memory.embedding_provider`), `prx doctor`, `prx onboard`,
    `prx models`, and the chat banner, status bar, `/provider` and `/export`.

  **Still printed as configured:** saved chat session files (the label is
  read back on resume), WASM request middleware payloads, `agent.rs` and
  control-ladder traces, the heuristic router's logs and trace, the
  `sessions_spawn` provider-build error, DEBUG `provider=` lines in
  `delegate` and `sessions_spawn`, and `Unknown provider: …` for a label the
  factory cannot read. Records written by an earlier release are not
  rewritten. Prefer a credential in `api_key` over URL userinfo.

  **Pricing.** Usage records name the provider redacted. A `[cost].prices`
  key spelled with the credentials (`custom:http://u:pw@h/v1/model`) still
  prices that provider's spend: an exact key wins, otherwise the key whose
  redacted spelling matches. Two credentialed keys that redact to the same
  spelling (two accounts on one endpoint, same model) are a configuration
  error: the configuration does not load, reload or save, and the error names
  the key in its redacted form only. Should such a pair ever be reached at
  runtime anyway, the highest matching price is charged, so the spend always
  reaches the budget. Keep one key per endpoint and model, preferably without
  credentials (put them in `api_key`). The config API and
  `prx config show` show such a key with its userinfo redacted; posting it
  back unchanged keeps the stored key, and a redacted key that matches no
  stored key is refused. Every configured input, output, cache-write and
  cache-read price must be finite and nonnegative; invalid prices are refused
  at configuration validation rather than treated as free usage.
- **A declaration PRX suggests for such a provider** (after a refusal or an
  upstream rejection) prints it as `custom:http://<user>:<password>@host/v1`:
  write the provider exactly as in your configuration, credentials included,
  because a declaration matches by provider identity and a `[redacted]`
  label matches nothing.
- **`vllm`, `llamacpp`, `lmstudio`, `litellm` are one identity each**, whatever
  URL they reach: a `vllm` fallback built without the root `api_url` (so
  pointing at `localhost:8000`) shares the entry with a remote root `vllm`.

**Migration.** Each entry is the `(provider, model)` key plus two capability
fields of the per-(provider, model) capability descriptor, which will read this
table, emit one descriptor entry per row, and retire it; the value sets are
subsets of the descriptor's, so no semantic translation is needed.

`anthropic-custom:<url>` resolves to `anthropic`, matching the provider that
label constructs. A caller that knows the model — the chat driver — asks for
the dialect of the member and model that serve the request: an endpoint
answers its declaration for exactly that model, a router the dialect of the
destination the model routes to, a failover chain the dialect every
(member, model) pair that may serve it agrees on. A caller that does not know
the model gets the dialect every member and model would put on the wire, or
`raw` when they differ. Nothing ranks rewrites any more (`gemini` removes
keywords, `no_root_combinators` restructures the root; neither subsumes the
other): a declaration for one model of an endpoint no longer changes what
another model's turn is held to, and `raw`, the published document, is the
strict answer whenever no single rewrite is exact.

The agent loop (`prx agent`, cron and xin agent jobs) asks the provider
object, as `prx chat` does: each turn is validated against the dialect for the
model it requests, and after the call against the dialect of the chain member
that actually served it (a fallback across provider families is held to the
fallback's own rewrite). A declared `no_root_combinators` endpoint is
therefore validated against the merged document it was shown. The
`loop_enforcement_accepts_exactly_what_the_declared_wire_document_accepts`
test pins that the loop's dialect, the provider's dialect and the bytes an
endpoint actually receives are the same contract for a representative schema.

### Refused tools and how to exclude them

Fail-closed is **per request**: a turn whose offered tool slice contains even
one refused tool is not sent at all. The first unrouted turn of the
fullscreen chat offers the whole registry by design, so on a
`no_root_combinators` endpoint it is refused unless the tools are excluded.
On a current build these fourteen published tools carry a root conditional or
alternative and are refused:

`chat_profile_update`, `composio`, `cron`, `document_get_chunk`,
`document_ingest`, `gateway`, `git_operations`, `mcp_call`, `memory_get`,
`nodes`, `proxy_config`, `skills_manage`, `subagents`, `xin`

(`tests/tool_schema_contract.rs` pins this list; a change is a finding either
way). MCP servers' `inputSchema`s are refused or lowered by the same rules.

The refusal is one error that lists every refused tool with its keyword
(`cron[allOf], memory_get[anyOf], …`) and then spells out the levers:

- **Every model:** `always_exclude = ["cron", "memory_get", …]` under
  `[tool_tiering]`.
- **This model only:** a `[tool_tiering.model_allowlists]` entry for the model
  that lists the tools it may use, without the refused ones.
- **One MCP alias** (`<prefix>__<server>__<tool>`): `deny_tools = ["<tool>"]`
  under `[mcp.servers.<server>]`. Tiering excludes whole tools, so it cannot
  remove a single alias of the MCP router; `deny_tools` does, for every model.
- **If the declaration is wrong** and the endpoint accepts root combinators:
  `tool_schema = "raw"` for that `(provider, model)`.

Limits: `[tool_tiering]` applies only on the paths that tier tools. Sub-agents
(`delegate`, `sessions_spawn`), session workers and `process_message` do not,
so they still refuse on such an endpoint (follow-up DIALECT-REACH).

**Never retried against the same provider.** The refusal is decided before
transport and is the same every time, so the reliability layer records it
once and moves on: no backoff, no second attempt, and not the
`reliability.api_keys` rotation instances of the same provider either. It
does fail over: a `fallback_providers` entry or a model fallback receives the
schema in its own dialect (an undeclared endpoint receives the published
document). The failover summary keeps the whole refusal; it is not clipped to
200 characters like upstream error bodies.

**What each surface reports.** The CLI, the REPL and the chat surfaces print
the whole refusal. An IM channel tells the user that the assistant's tool
configuration cannot be used with its AI service, that sending the message
again will not help, and gives the error ID; the tool list stays in the log
(failure class `tool_schema_refused`). The gateway webhook and the console
session API answer the authenticated caller with HTTP 500 and
`{"error": "tool_schema_refused", "tools": [...], "message": "..."}`, and log
the whole refusal.

**An undeclared endpoint that rejects a root combinator** answers with its own
400 or 422. When that body names `allOf`, `anyOf` or `oneOf` next to "tool"
or "schema", the error adds the declaration that would fix it:
``declare `[[providers.tool_dialects]]` provider = "…" model = "…" tool_schema = "no_root_combinators"``.
The hint changes no behaviour and PRX never infers the dialect from it.

## Textual tool-call grammars

Some chat templates answer a tool request with a textual grammar in the message
content — `<function=NAME>{json}</function>` is the Qwen/Llama spelling —
alongside, or instead of, the provider's native `tool_calls` array. Reading it
decides whether prose an endpoint produced becomes an executed tool call, so it
is a declared capability, not a courtesy.

**The default is off, fleet-wide.** `ToolCallDialect::Native`, which reads only
the native `tool_calls` array, applies to every endpoint unless an operator
declares `tool_call = "xml_function_tags"` for it in a provider-wide
`[[providers.tool_dialects]]` entry (see above). The grammar is read per
endpoint, not per model: the response parser does not know which model
produced a response, so a model-scoped `tool_call` entry is refused at load.

**Non-streaming turns only.** The declared grammar is read from buffered
responses. A streaming turn — the interactive chat surface streams — reads
only native `tool_calls`, and a `<function=…>` block in its text stays text:
nothing is executed, which is the fail-closed side. Reading it from a stream
would mean holding back text already destined for the screen until a block
closes; that is a separate follow-up (STREAM-XMLFN), not part of the native
streaming tool calls the direct providers now send.

**This changed the behaviour of every OpenAI-compatible endpoint.** The
`<function=` parser used to live unconditionally in the OpenAI-compatible
response path, so every label routed through it — including `custom:<url>` —
read that grammar out of prose without declaring it. They no longer do: an
endpoint that relied on the grammar now returns its content as text and
executes nothing until the dialect is declared for it. That is the fail-closed
direction, and it is the one user-visible regression of the change.

On a declared endpoint, a block that fails either check is reported rather than
skipped: exactly one redacted `tracing::warn!` per rejected block, naming the
provider, the tool and the reason. The checks are that the name is one this
request registered — a request that registered no tools has no such name, so
every block is rejected — and that the body parses as JSON. A rejected block is
never executed.

## Routing, fallback, and cost truth

Provider construction and availability checks use the same credential
resolver. A route receives only its own credential. Both streaming and
non-streaming execution retain ordered provider/model attempt traces, and the
successful attempt supplies final PRX provider/model attribution. If an
aggregator such as OpenRouter performs an additional internal fallback, its
response `model` is the authoritative upstream-model signal.

CLI `--provider` overrides and named delegates follow the same ownership rule:
the root `api_key` and `api_url` are reused only for the configured default
provider. A different provider resolves its route credential or active auth
profile unless the named delegate supplies an explicit `api_key`.

Static prices are recorded in the embedded release catalog only when the
provider publishes token prices for a stable model ID. Kimi Code is
subscription/quota billed, so PRX deliberately does not invent a per-token cost
for its coding aliases. OpenRouter rows may reuse the stable upstream tariff for
recognized Anthropic, Google/Gemini, Moonshot, OpenAI and xAI model IDs; other
OpenRouter and proxy pricing remains dynamic except for exact release rows with
a published price. The release values are checked against the providers'
published pricing: [Anthropic](https://platform.claude.com/docs/en/about-claude/pricing),
[Gemini](https://ai.google.dev/gemini-api/docs/pricing), and
[xAI pricing](https://docs.x.ai/developers/pricing), and
[Moonshot/Kimi token tariffs](https://platform.kimi.ai/docs/pricing/chat).
An uncertain model-specific price is omitted instead of inferred from another
generation. Router ranking cost is a separate catalog hint and is never used as
the input/output billing tariff.

OpenAI tariff snapshot (2026-10-17), USD per million tokens:

| Exact catalog model | Input | Cached input | Output | Published source |
| --- | ---: | ---: | ---: | --- |
| `gpt-4o` | 2.50 | 1.25 | 10.00 | [GPT-4o](https://developers.openai.com/api/docs/models/gpt-4o) |
| `gpt-4o-mini` | 0.15 | 0.075 | 0.60 | [GPT-4o Mini](https://developers.openai.com/api/docs/models/gpt-4o-mini) |
| `gpt-5.2-codex` | 1.75 | 0.175 | 14.00 | [GPT-5.2-Codex](https://developers.openai.com/api/docs/models/gpt-5.2-codex) |
| `gpt-5.6`, `gpt-5.6-sol` | 4.00 | 0.40 | 20.00 | [GPT-5.6 Sol and its alias](https://developers.openai.com/api/docs/models/gpt-5.6-sol) |
| `gpt-5.6-luna` | 0.20 | 0.02 | 1.20 | [GPT-5.6 Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna) |
| `gpt-5.6-terra` | 2.00 | 0.20 | 12.00 | [GPT-5.6 Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra) |
| `o1-preview` | 15.00 | 7.50 | 60.00 | [o1 Preview](https://developers.openai.com/api/docs/models/o1-preview) |

For the GPT-5.6 family, the cited pages price requests above 272,000 input
tokens at twice the input/cached-input rate and 1.5 times the output rate for
the whole request. Cache writes cost 1.25 times the applicable uncached input
rate. The catalog records both tiers; these are not Router ranking costs.
Sol's published promotional tariff runs at least through 2026-11-21, so a later
release must recheck it rather than treating it as permanent.

The same 2026-10-17 check pins Gemini 3.1 Flash-Lite text/image/video input,
cached input and text output at 0.25/0.025/1.50. Gemini 3.1 Pro Preview uses
2.00/0.20/12.00 through 200,000 input tokens and 4.00/0.40/18.00 above that
threshold, as published in [Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing).
The token catalog does not represent explicit-cache hourly storage or search
fees. Kimi K2.6 uses the published 0.95 cache-miss input, 0.16 cache-hit input
and 4.00 output tariff; its cache-write field uses the miss rate, not an extra
charge on top of input. The Kimi page's rendered table is also published in its
[Markdown source](https://platform.kimi.ai/docs/pricing/chat.md).

Gemini 1.5 Pro, Gemini 2.0 Flash, Kimi K2.5 and Grok 3 are left unpriced in this
release: the fetched official pricing pages did not establish their exact
current tariffs. Old rates and zero cached-input placeholders are not reused as
proof. Exact operator price overrides still take precedence; release lifecycle
eligibility is unchanged by removing an unverified tariff.
The same rule applies to the older Claude 3 Haiku aliases and Claude 3.5
Sonnet, whose exact rates were absent from the fetched current table. Direct
Claude Sonnet 5 now matches its separately verified 2.00/10.00/2.50/0.20
input/output/default-cache-write/cache-read row on the cited Anthropic page.
OpenRouter Kimi K2.5 is also left unpriced: its model page and public model
metadata returned different input rates, so neither the old Moonshot rate nor
one arbitrarily selected provider rate is substituted. OpenRouter
[Jev 1.13](https://openrouter.ai/typesafe/jev-1.13) publishes 0.042 input and
zero output per million tokens; the catalog no longer invents an output charge.

Automatic Router candidates require an active release row, explicit Router
hints and a context limit, plus the same effective billing-tariff check used by
onboarding. This includes verified upstream tariffs for supported OpenRouter
IDs; a ranking hint alone never makes a row priced. The release includes active
[Gemini 3.1 Flash-Lite](https://ai.google.dev/gemini-api/docs/models/gemini-3.1-flash-lite)
and [Gemini 3.1 Pro Preview](https://ai.google.dev/gemini-api/docs/models/gemini-3.1-pro-preview)
candidates with their published limits. Direct OpenAI
tariffs remain available, but automatic hints wait for the catalog parameter
wire mapping: the current adapter always sends temperature and cannot choose
the required reasoning settings. GPT-5.2-Codex additionally supports Responses,
not that adapter's Chat Completions endpoint. Pricing is not transport proof.
`prx doctor` now reports when the actual registry expansion and provider filter
leave no Router candidates, including a direct-OpenAI-only configuration; the
diagnostic does not change the selected default or invent a fallback. Explicit
`[[router.models]]` entries remain operator choices and use the same filter.
For the default OpenRouter provider, [Claude Sonnet 5](https://openrouter.ai/anthropic/claude-sonnet-5)
has a 1,000,000-token context and a separately published 2.00/10.00 input/output
tariff. Its [public model metadata](https://openrouter.ai/api/v1/models) lists
0.20 cached input and 2.50 default cache writes per million tokens. One-hour
cache writes and regional endpoint uplifts are separate tariffs and need a
matching operator override; they are not silently treated as the default rate.
Router latency, cost ranking and initial quality values are project-owned
selection hints, not provider performance guarantees or billing prices.

OpenRouter `:free` variants settle at zero token cost unless an operator override
is present. The deprecated `:online` variant stays unpriced because its
separately billed web-search component is not representable by PRX's token-only
catalog tariff; use an exact `[cost.prices]` override or migrate to OpenRouter's
server web-search tool. The variant and its extra search pricing are documented
by [OpenRouter](https://openrouter.ai/docs/guides/features/server-tools/web-search).

`[cost.prices]` is an operator-override table and wins over the release catalog.
When neither source has a matching price, only `ollama` is treated as zero-cost.
LM Studio, llama.cpp, vLLM, and custom loopback endpoints need an explicit
`cost.prices` entry before PRX can produce a cost estimate.

The Router's built-in candidates are limited to active release rows with a
published billing tariff. Retired or unpriced rows remain available only through
an explicit operator route; they are never automatic candidates. Onboarding
labels a selectable catalog row when its effective token tariff is unavailable.

The catalog's `ollama/*` entry seeds only the Router's conservative local
candidate. It does not assert vision, context-window, or verification facts for
an arbitrary local model; exact discovery or `[[providers.models]]` declarations
remain authoritative for those models.

The canonical routing, usage, and cost settlement boundary is described in
[provider routing and cost lifecycle](provider-routing-cost-lifecycle.md).
The 2026-10-04 local candidate adds source-bound Router ranking and bounded KNN
history for an actual `LocalHashEmbedding` instance. The configured local
embedding factory retains that concrete instance for Router construction;
embedded callers inject their configured engine through `AgentBuilder::router`.
A dynamic embedder's name does not establish local computation authority.
History is read only after a purpose-bound selected Retrieve grant. Selecting a
model then requires PMM binding and a new dependent decision before execution.

SQLite, PostgreSQL and Lucid's local SQLite delegation support this scoped
history and learning. The final serving attempt and authenticated terminal,
rather than the planned model, determine attribution. A failed read or
readmission closes the turn without a provider attempt; authenticated replay
preserves that historical result without granting new execution authority.
See the [cost lifecycle](provider-routing-cost-lifecycle.md#embedded-local-routing)
for domain, retention and session limits.

Separately billed Router embeddings, Signal vision summaries and AutoMix
premium attempts require a source-owned token, money and deadline allowance.
The supported OpenAI/compatible chat and OpenAI-compatible embedding transports
reserve before each physical send and settle reported usage separately. Their
actual endpoint, credential and model must match the captured factory targets;
provider display labels do not grant accounting authority. Other transports
refuse the auxiliary call before dispatch until their real sends are instrumented.

AutoMix preserves the main cheap-call outcome and its existing settlement. A
served premium reply has a separate verified auxiliary outcome/reply link and
physical accounting scope; it is not charged again as aggregate main usage or
trained as a cheap-model answer. Recording-disabled execution does not acquire
synthetic durable replay evidence. Candidate compilation, behavior, cancellation,
replay and performance acceptance remain separate gates.

## Rate limiting (HTTP 429)

Rate-limit handling is entirely *reactive*. PRX never paces requests on its own
and never caps in-flight provider calls; everything below is triggered by an
explicit signal from the upstream.

- **`Retry-After` is honored on both paths.** Streaming carries it in the
  structured `StreamError::RateLimited` variant; non-streaming attaches it to
  the provider error as a `RetryAfterHint` source read from the real response
  header. When present it decides the wait; the local exponential backoff is
  only the floor.
- **Honored ceiling: 120 s.** Organization-level throttling commonly answers
  `Retry-After: 60`, so the previous 30 s cap truncated the delay and guaranteed
  a second 429. A hint above the ceiling is *not* truncated — the candidate is
  abandoned and the requested delay is reported in the aggregated error.
- **Retries never depend on the header.** A 429/503 without `Retry-After` is
  still retried on the same provider/model up to `reliability.provider_retries`,
  on both the streaming and the non-streaming path, before any failover.
- **Backoff is jittered.** Local exponential waits are spread over
  `[nominal/2, nominal]`; an honored `Retry-After` gets a small additive spread
  (never shorter than the server asked). Without this, requests throttled in the
  same instant retry in lock-step and re-create the burst that caused the 429.
- **Business 429s still fail fast.** Plan/quota/balance responses (including the
  Z.AI `code` values `1113` / `1311`) skip the retry budget on both paths. The
  code match requires a `code`-like key, so digits inside a `request_id` or a
  token counter can no longer condemn an ordinary 429.
- **One 429 defers the others.** A per-provider cool-down deadline is shared by
  every provider chain in the process, so concurrent requests (including
  in-process `mode = "task"` sub-agents) wait out a throttle learned by any one
  of them. It stores a time, not a permit count — concurrency stays unbounded.
  Sub-agents started with `mode = "process"` are separate OS processes and do
  not observe this gate.
- **Sub-agents and delegated agents get the full chain.** A `sessions_spawn`
  provider override or a `delegate` agent builds a resilient provider, not a
  bare one. The pinned vendor is respected: `reliability.fallback_providers` and
  the rotation `reliability.api_keys` are dropped for those chains, while the
  retry budget, backoff and `model_fallbacks` carry over. The child's model is
  resolved before the chain is built (see "Where the resolver runs").
- **Telemetry.** The daemon's state file (`daemon_state.json`) carries a
  `rate_limits` block with per-provider counters (total, streaming,
  `Retry-After` honored, recovered, exhausted, gate deferrals) and a per-model
  breakdown. It is **not** on `GET /health`, which this page claimed until it
  was checked: that endpoint serializes `status`, `paired`, `readiness` and
  `runtime` only. The block is attached by `health::snapshot_json()`, which no
  gateway route calls: the daemon's state writer is what puts it on disk.

## OpenAI Codex notes

- Malformed/unexpected response payloads fail fast with
  `provider_response_parse_error`.
- Stream idle timeout is controlled by
  `ZEROCLAW_CODEX_STREAM_IDLE_TIMEOUT_SECS` (default 45 seconds, minimum 5).

A Root connection with `api_url` or `reliability.api_keys` but no `api_key`
keeps that owner-bound endpoint and rotation material, then performs the same
provider-store lookup as an otherwise unconfigured key: active auth profile,
environment credential, OAuth cache or subscription login, according to the
provider. Keyless providers still build when that lookup finds nothing. OpenAI
Codex is the exception in the other direction: it always uses its own login and
ignores Root API keys, URL and rotation keys. A Root connection is never
released to a different provider identity.
