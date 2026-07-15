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
