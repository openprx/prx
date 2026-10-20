# Provider management

This reference describes the local candidate as of 2026-10-04. Full runtime,
browser, concurrency and independent acceptance remain pending. Consult
[provider descriptors](providers.md#model-descriptors-and-the-resolver),
[release model catalog](model-catalog.md),
[configuration](configuration.md), [Console](web-console.md),
[cost lifecycle](provider-routing-cost-lifecycle.md) and the
[decision plane](decision-plane.md) for the related contracts.

## Identity, operations and credentials

A provider/model descriptor separates executable operations from model features.
Supported, unsupported and unverified are distinct states; an unverified model
name does not establish native-tool support. Operator declarations cannot create
an adapter for an unsupported operation. A decision endpoint declaration does
not make that model a chat model.

Routes and credentials retain their provider identity. A different provider
must resolve its own credential rather than inherit the primary provider key.
`credential_ref` refers to stored provider-owned material; it is not the secret
itself. Changing an endpoint can change the recipient of a credential and is a
high-risk configuration change. Do not put secrets in URL parameters, command
arguments or exported inspection output.

The `prx routes` CLI currently accepts `--operation chat` (the default) and
`--operation embedding`. Do not infer support for routing other operations from
the descriptor vocabulary. The decision backend has its own management commands.
A configured embedding route establishes selection, not a bounded paid-send
permit. The local Router history path requires a concrete local hash embedder
and a selected history-read grant; paid embedding and premium retry contracts
remain open. See [routing and cost lifecycle](provider-routing-cost-lifecycle.md#embedded-local-routing)
for the candidate's scoped history, learning and replay limits.

## Inspect before changing

| Command | Behavior |
| --- | --- |
| `prx providers list` | Supported and configured providers |
| `prx providers show <PROVIDER>` | Identity, uses, declarations and credential status |
| `prx providers credential status` | Credential status without secret values |
| `prx providers test <PROVIDER>` | Resolve configuration without a live model-list request |
| `prx providers test <PROVIDER> --live` | Send a model-list request; not a generation |
| `prx routes list --operation embedding` | Inspect embedding routes |
| `prx models list [--provider <PROVIDER>]` | List live cached models when present, otherwise models from the embedded release catalog |
| `prx models list --operation <OPERATION> [--executable]` | List the combined release, discovery and operator evidence used by the resolver |
| `prx config status` | Revision, default resolution and unfinished history |
| `prx config history` | Transaction records |
| `prx config history show <ID>` | Redacted plan and recorded result |

A successful model-list request does not prove generation, tool execution,
usage accounting or decision-endpoint behavior. Inspect the exact selected
provider/model and operation before interpreting a capability result.

Built-in defaults, prices, context windows, Router hints and model feature
facts come from the embedded release catalog. TOML retains operator choices:
`[cost.prices]`, `[[router.models]]`, agent compaction settings and
`[[providers.models]]` override the corresponding release facts at their
documented boundaries. A configuration rollback therefore restores choices;
the binary rollback restores the embedded catalog revision.

Model selection also checks whether the adapter can express the release row's
declared parameters. An unsupported adapter mapping is refused locally. Existing
Codex reasoning-effort overrides still reach request resolution; recognized
effort values clamp to the model's declared subset, while unrecognized values
fail locally. No parameter setting can change a provider's credential owner.

Provider add/update/remove, model declarations, defaults and routes use the
management change flow. For example, `prx providers add <PROVIDER> --fallback`
previews adding that provider to the fallback chain; `--yes` applies the plan.
High-risk changes also require confirmation naming the reported revision.
Provider or route removal is refused while an active selection references it.
Credential set/remove are separate explicit mutations: use protected standard
input, an environment-variable name or the hidden prompt for set. They are not
read-only inspection commands.

## Transactions and activation

A transaction stages and validates the stored configuration tree, checks secret
boundaries and captures the current revision. Under the shared writer lock it
checks that revision again, records a sealed before-image and commits the file
journal. A stale revision is refused rather than overwriting a newer writer.

Runtime activation is a separate result. A running process can reject the new
configuration; compensation restores the before-image only if this transaction
still owns the revision. A CLI write may report `activation_deferred`, pending
the running process's watcher. Do not report a new provider as active solely
because its configuration file was written.

History lives in `.config-history/` beside `config.toml`. Before-images are
sealed with the installation key even when ordinary configuration encryption is
off. Only redacted history descriptions are exposed. The newest 20 finished
records are retained; `pending` and `not_restored` records survive retention.
Preserve the matching key and history during authorized recovery. A replaced
key cannot decrypt the old before-images.

An interrupted transaction can leave `pending`; it is not proof that activation
succeeded. Inspect current revision and history before retrying. A replacement
credential value is not saved for rollback: `credential_replaced` is explicitly
non-restorable. Config rollback restores files from before the selected record
and can undo later changes to those files. Preview with
`prx config rollback <ID>`; application requires the management confirmation
flow. Restoring removed credentials or disabling encryption needs the separate
explicit flags described by the command. Do not purge history to resolve an
unfinished transaction.

## Console API

These routes are under the authenticated `/api` mount. Mutations retain
management validation, revision and high-risk confirmation checks.

| Method | Path |
| --- | --- |
| GET, POST | `/providers` |
| GET, PATCH, DELETE | `/providers/{id}` |
| PUT, DELETE | `/providers/{id}/credential` |
| POST | `/providers/{id}/test` |
| GET, POST, PATCH, DELETE | `/models` |
| GET, PUT | `/defaults` |
| GET, POST | `/routes` |
| PATCH, DELETE | `/routes/{hint}` |
| GET | `/agents` |
| PUT | `/agents/{name}` |
| POST | `/config/plans`, `/config/apply`, `/config/rollback` |
| GET | `/config/history` |

Use the returned plan, revision and activation result. An HTTP success for
inspection is not authority to mutate configuration or run a paid request.
The Console binds each plan to its displayed revision. Page teardown stops
confirmation resends, but cannot retract a transaction already sent. Provider,
model and routing reads reject responses superseded by a newer request; provider
credentials and test responses remain associated with the selected provider.
A confirmed credential write and a failed status refresh are separate outcomes.
Decision-credential replacement also reports a previous credential left behind
instead of implying that rotation cleanup completed.

Console browser acceptance remains a separate requirement from route presence.

## Migration and recovery limits

`prx providers migrate` reports legacy inline keys, dialect declarations,
unverified selections and alias-stored tokens. `--apply` constructs a migration
plan; `--yes` writes it through the transaction flow. Inspect the plan before
adopting it and retain its rollback record. Migration does not create evidence
that a provider supports an unverified model capability.

Configuration rollback is not a binary downgrade guarantee. Before adopting an
older binary, verify its parser, credential references and storage readers
against the actual configuration and retained data. Never delete unknown schema
metadata to force compatibility. Final release-specific downgrade instructions
and complete PMM acceptance are still pending.

## Model settings transactions

`ManagedChange::SetModelSettings` replaces a model's persisted level and
explicit parameter overrides. Default selection accepts optional settings in
the same transaction. Both operations validate against the release catalog,
retain revision/history/rollback checks and refuse invalid durable values.
The Console generates controls from catalog declarations and submits the
existing plan/apply protocol. See [commands and API examples](model-catalog.md#saving-a-model-level).
