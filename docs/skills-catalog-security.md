# Skills catalog lifecycle and trust boundary

PRX uses one process-level skill catalog snapshot per workspace and effective
skills configuration. Runtime inference, chat, channels, gateway sessions, and
the skills listing API reuse that snapshot. They do not run Git, update a
community checkout, or rescan the workspace on every request.

## Sources and precedence

Catalog construction is deterministic. Directory entries and the final catalog
are sorted by normalized skill name, duplicates are collapsed, and later
sources have higher precedence:

1. `open-skills` community metadata
2. OpenClaw community metadata
3. workspace `skills/` entries

When the 256-entry limit is reached, workspace entries receive admission
priority and the admitted set is then emitted in deterministic name order.

The catalog is capped at 256 entries. Community sources must already exist on
disk. Operators synchronize enabled community repositories explicitly with:

```bash
prx skills sync
```

That command is a control-plane operation and may run Git. Normal catalog loads
never clone or pull. A process-local catalog is invalidated after a successful
install, uninstall, explicit synchronization, or SkillForge integration.
Manual filesystem edits require a process restart or an explicit catalog
invalidation by the embedding application.

Workspace enablement is persisted under `skills/.openprx-disabled/`. A disabled
workspace entry suppresses a same-named community fallback, remains visible to
control-plane inventory, and is excluded from prompt selection and executable
skill-tool aliases.

## Prompt trust and limits

Community repositories are always lazy: PRX loads bounded name, description,
and location metadata but never preloads their Markdown instructions into the
system prompt.

Remote installs and SkillForge integrations contain
`.openprx-untrusted-origin.json`. While that marker exists, any manifest prompt
or Markdown body is also lazy and cannot be injected into the system prompt.
An operator may review the files and deliberately remove the marker to trust
the workspace-local instructions.

Input and output limits are defense in depth:

- catalog: 256 skills
- TOML manifest: 256 KiB
- Markdown metadata input: 64 KiB
- description: 1 KiB
- trusted instruction: 16 KiB each
- complete rendered skills prompt: 64 KiB

Prompt metadata is XML-escaped. Process-level embedding results are reused by
provider/model/dimension namespace and bounded to 2,048 cached entries.

## Installation lifecycle

CLI Git installs, gateway Git installs, and SkillForge integrations write to a
hidden same-filesystem staging directory. PRX validates that the root contains
a bounded, parseable `SKILL.toml` or `SKILL.md`, rejects symlink manifests,
writes the untrusted-origin marker for remote material, and only then exposes
the skill with an atomic rename. Invalid or failed staging trees are removed;
an existing active skill is never replaced.

Local CLI paths are explicit operator input and remain trusted. They are staged
as a link/copy, validated, and atomically activated through the same lifecycle.

Remote updates clone the recorded source into a fresh staging directory,
validate it, and swap it into place while retaining a rollback copy until the
new directory is active. Local linked skills already track their source and are
revalidated by update. Agent-initiated lifecycle changes use `skills_manage`
and pass through the common side-effect approval and audit boundary.

## Execution lifecycle

Enabled `SKILL.toml` declarations with kind `shell`, `script`, or `http` are
exported through `skill_execute` and deterministic skill aliases. Script paths
must be relative and remain inside the selected skill's canonical root. Shell
and script output uses the shared bounded, cancellation-aware process adapter;
HTTP declarations reuse `http_request` network restrictions. Unsupported kinds
remain visible in `skills_list` as unsupported and are never exported as
provider-callable ToolSpecs.

## Required tools and learning evidence

A skill may declare the tools a run must call to have followed it, with the
optional top-level `SKILL.md` frontmatter key `required-tools`, a one-line list
of canonical capability ids:

```yaml
---
name: deploy-helper
description: Helps with deploys
required-tools: [filesystem:native:shell, filesystem:native:file_read, mcp:notes:mcp__notes__search]
---
```

An id is exactly three non-empty parts, `family:namespace:public_name`, as the
capability catalog builds it:

- The family is the first matching category of the tool, checked in this order:
  `document_search` is `document`, `sessions_history` is `session`, a tool in
  the file-system category is `filesystem`, then `memory`, `web`, `im`, and a
  tool named `shell` is `shell`; every other built-in tool is `native_tool`.
  The shell tool is in the file-system category, so its id is
  `filesystem:native:shell`, not `shell:native:shell`. Skill tools are `skill`,
  MCP tools `mcp`, plugin tools `wasm`.
- The namespace is `native` for built-in tools. For MCP it is the server name
  once the tool has been discovered, and the literal `mcp` for a configured
  alias that has not been discovered yet, so the same tool can have two ids
  depending on discovery state; declare the id the running catalog shows.
- The public name is the name the model sees. An MCP tool's is
  `mcp__<server>__<tool>`, so its id is `mcp:<server>:mcp__<server>__<tool>`.
  A skill tool's is `skill__<skill>__<tool>`.

Examples: `filesystem:native:shell`, `filesystem:native:file_read`,
`native_tool:native:<name>`, `mcp:notes:mcp__notes__search`,
`skill:native:skill__<skill>__<tool>`.

Find an id by calling `capability_search`: each hit carries the canonical id in
`capability.id`. The id cannot be resolved from a bare tool name when a revision
is written, because the lifecycle store has no catalog at that point (the
catalog is built per turn from the live configuration and MCP servers), so
operators declare the full id.

At most 16 ids per skill. The same key may appear under `[skill]` in
`SKILL.toml` as an array of strings. When both files declare it, the lists must
be equal; a disagreement is rejected when the revision is validated. Writing a
new revision (an operator write or an import) refuses any other form of the
key, a repeated key or a mis-shaped id with a typed manifest error. Reading
a stored revision stays lenient: a line in an older form declares nothing and
is logged, so skills stored before the key existed stay readable. The
`[[tools]]` entries of `SKILL.toml` are not part of the requirement.

The skill lifecycle records the declared tools of an imported, operator-written
or upstream revision as its `capability_requirements`. A learned revision keeps
exactly its baseline's list: a candidate that adds or removes a required tool,
in the record or in its manifests, is refused at source review and at
publication, because that would move the oracle its own evidence is judged by.

At the end of a run the lifecycle compares the required tools with the tools
the run called. A failed run counts as evidence against the skill's guidance
only when the required tools were called, the provider and the tool backends
were available, and no tool call was refused by policy or approval. A run that
read more than one managed skill is not attributed to any of them. A skill that
declares nothing has no verifiable adherence, so its failures are never used to
propose a revision.

Tool failures count as an unavailable backend when their text (with its source
chain) names a connection error, timeout, DNS or routing failure, "error sending
request", overload, or an HTTP 429, 500, 502, 503, 504 or 529 as a whole token.
The MCP allow/deny refusal counts as a denial. Known limit: a tool that returns
an upstream failure as a successful result is invisible; a typed error kind is
the later fix. The matching is deliberately broad: a phrase such as "timeout"
inside unrelated text, or a bare `500` token in a message, counts as an
unavailable backend; this fails safe, because it only keeps a failure out of
the evidence. Only a provider failure labelled timeout, rate limit,
credentials or server error counts as an unavailable provider; the generic
agent error label does not.
