# Evidence-bearing compliance controls

PRX produces an **implementation attestation**, not a regulator-issued
certificate and not legal advice. Applicability and legal classification remain
operator decisions. The pinned legal source for the controls below is the
official text of Regulation (EU) 2024/1689:
<https://eur-lex.europa.eu/eli/reg/2024/1689/oj?locale=en>.

## Release gate

```bash
prx audit attest-eu-ai-act --json
prx audit attest-eu-ai-act --json --fail-on fail
prx audit attest-eu-ai-act --json --fail-on warning
```

The first command reports without turning control results into an exit-code
gate. `--fail-on fail` rejects applicable failed controls. The strict
`--fail-on warning` mode also rejects warnings and unknowns. A passing control
must contain machine-verifiable evidence; configuration assertions identify a
revision rather than embedding secrets.

## T04: AI interaction notice

The adapter-neutral notice is emitted before the first direct natural-person
response for a channel/peer and version. Its durable acknowledgement contains
only a hashed channel/peer key, the notice version, and a timestamp; message
content is not stored.

```toml
[compliance.interaction_notice]
applicability = "required"
version = "v1"
message = "You are interacting with an AI system."
```

An exception is a legal/policy decision and must be explicit:

```toml
[compliance.interaction_notice]
applicability = "not_applicable"
exception_owner = "legal-owner"
exception_reviewed_at = "2026-07-18"
```

Exception handling: use an approved not-applicable
record. Preserve acknowledgement data so re-enabling the same version does not
duplicate notices. Increment `version` when recipients must see changed text.

## A04: PostgreSQL vector isolation

Migration 21 enables and forces row-level security on `memories`,
`document_chunks`, and `embedding_cache`. PRX sets trusted workspace/owner
scope transaction-locally; prompts and tool arguments cannot set that scope.
Missing scope fails closed. SQLite reports this PostgreSQL-specific control as
not applicable and does not inherit an RLS claim.

Run live verification against the configured PostgreSQL backend:

```bash
prx audit attest-eu-ai-act --json --fail-on fail
```

Rollback: prefer a forward repair of policy or scope propagation. Do not drop
or disable RLS on a live multi-owner database, because that can expose rows
during rollback. If an application rollback is unavoidable, keep migration 21
and forced RLS installed, restore a compatible scoped application version, and
verify cross-owner denial before reopening traffic.

## C02: declaration of conformity artifact

This control applies only after an operator records `high_risk`. For
`not_high_risk`, record a named classification owner and assessment date; the
control becomes `not_applicable`. An unclassified deployment remains unknown.

```toml
[compliance.eu_ai_act.classification]
status = "high_risk"
owner = "legal-owner"
assessed_at = "2026-07-18"

[compliance.eu_ai_act.declaration]
artifact_path = "compliance/eu-declaration.json"
artifact_version = "1"
system_name = "Operator supplied system name"
system_type = "Operator supplied system type"
system_reference = "release-or-registration-reference"
provider_name = "Legal provider name"
provider_address = "Legal provider address"
sole_responsibility_statement = "Operator-approved statement"
conformity_statement = "Operator-approved conformity statement"
applicable_union_law = ["Regulation (EU) 2024/1689"]
conformity_assessment_procedure = "Operator-supplied procedure"
issue_place = "Operator-supplied place"
issue_date = "2026-07-18"
signer_name = "Authorized signatory"
signer_function = "Authorized function"
signer_on_behalf_of = "Provider legal entity"
```

Generate only after legal/operator review:

```bash
prx audit generate-eu-declaration \
  --signature-reference external-signing-receipt:REPLACE_ME
```

The artifact is versioned and hashed. The signature reference identifies an
operator-controlled external action; PRX neither authenticates that signature
nor submits the artifact. Missing Annex V inputs fail generation.

Rollback: stop generating new artifacts and revert the generator, but retain
already issued artifacts and external signature receipts for the required
record-retention period. Never rewrite an issued artifact in place.

## M04: serious-incident workflow

For a high-risk classification, configure the durable SQLite workflow store:

```toml
[compliance.eu_ai_act]
incident_store_path = "compliance/article-73-incidents.sqlite3"
```

Create input is explicit JSON; legal review owns severity and jurisdiction:

```json
{
  "incident_id": "incident-2026-001",
  "system_reference": "release-reference",
  "awareness_at": "2026-07-18T12:00:00Z",
  "causal_link": "suspected",
  "severity": "general",
  "jurisdiction": "EU member state",
  "responsible_owner": "incident-owner",
  "initial_report": null
}
```

```bash
prx audit incident-create --input incident.json
prx audit incident-initial-report incident-2026-001 --input initial-report.txt
prx audit incident-supplement incident-2026-001 --input supplement.txt
prx audit incident-close incident-2026-001 \
  --closed-at 2026-07-19T12:00:00Z --summary closure.txt
prx audit incident-export incident-2026-001 --output incident-export.json
```

Deadlines are calculated from awareness: 15 days generally, 2 days for a
widespread infringement or the Article 3(49)(b) category, and 10 days for
death. PRX records and exports evidence but always reports
`automatically_submitted=false`; a human must authorize any destination and
submission outside PRX.

Rollback: back up the SQLite database plus its WAL/SHM files at one consistent
checkpoint and retain exports. Reverting the CLI must not delete the store.
Restore only to a version that understands schema version 1, or use a
forward-compatible exporter before application rollback.
