# Built-in execution contract baseline

These JSON documents pin the complete built-in schemas published at commit
`4c61e2a9`, before FINDING 111 flattened their provider-facing roots. Tests compare
the current private execution contracts with these independent documents before
running invalid-input probes. Changing the implementation and its generated
probe corpus together cannot change this baseline.

All 20 schema construction expressions were checked against `4c61e2a9` before
evaluating them for capture; the schema construction helpers were unchanged.
`skills_manage_lifecycle.json` also pins the lifecycle-enabled variant.
`sessions_spawn.json` uses the existing schema fixture with no configured named
agents. Configured agent names affect its description; they do not change its
validation rules. Both configuration-dependent contracts cache per instance;
other contracts cache statically.

These are historical fixtures. Any intentional contract change requires an
explicit compatibility decision and a separately explained golden update.
