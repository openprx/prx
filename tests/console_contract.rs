//! Source contract of the Web Console's provider/model management pages
//! (SP-PMM §3 PMM-G).
//!
//! There is no JavaScript test framework in `console/`, and the browser is
//! out of reach here, so these tests read `console/src/**` and pin what the
//! pages must do: a credential travels once, in the body of a `PUT`, and
//! never reaches storage, a URL or the page state after it was sent; every
//! mutation goes through the management endpoints; a high-risk change is
//! confirmed with the revision its refusal named. Every Console source change
//! ships with a rebuilt, committed `console/dist` (D-038).

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The pages and shared modules of the management plane.
const MANAGEMENT_SOURCES: &[&str] = &[
    "console/src/pages/ProvidersPage.svelte",
    "console/src/pages/ModelsPage.svelte",
    "console/src/pages/RoutingPage.svelte",
    "console/src/pages/ChangesPage.svelte",
    "console/src/lib/change-flow.svelte.js",
    "console/src/lib/PlanReview.svelte",
    "console/src/lib/ErrorNotice.svelte",
];

/// The raw editor: advanced, and read-only for a file that holds a secret.
const RAW_EDITOR: &str = "console/src/pages/ConfigPage.svelte";

/// The hooks page, whose writes the gateway gates by risk (D-096).
const HOOKS_PAGE: &str = "console/src/pages/HooksPage.svelte";

/// The form fields that can receive a provider, model or endpoint the
/// gateway may have masked.
const MASKABLE_FIELDS: &[&str] = &[
    "endpointValue",
    "formProvider",
    "formModel",
    "routeProvider",
    "routeModel",
    "agentProvider",
    "agentModel",
];

/// The API methods the management pages may call, each with the endpoint
/// prefix its request goes to.
const MANAGEMENT_API: &[(&str, &str)] = &[
    ("getProviders", "'/api/providers'"),
    ("getProvider", "`/api/providers/"),
    ("setProviderCredential", "`/api/providers/"),
    ("removeProviderCredential", "`/api/providers/"),
    ("testProvider", "`/api/providers/"),
    ("getModels", "`/api/models"),
    ("getModelSettings", "`/api/models/settings"),
    ("getDefaults", "'/api/defaults'"),
    ("getRoutes", "`/api/routes"),
    ("getAgents", "'/api/agents'"),
    ("planChange", "'/api/config/plans'"),
    ("applyChange", "`/api/config/apply"),
    ("getConfigHistory", "'/api/config/history'"),
    ("rollbackConfig", "'/api/config/rollback'"),
    ("setConfigSecret", "`/api/config/secrets"),
    // G10: the Changes page reloads configuration and then reads status.
    // These are not a raw file save.
    ("reloadConfig", "'/api/config/reload'"),
    ("getStatus", "'/api/status'"),
];

/// The query parameters the management endpoints take; the gateway refuses
/// any other (so no cache-busting parameter either).
const MANAGEMENT_QUERY_KEYS: &[&str] = &["provider", "model", "operation", "executable", "name"];

/// The only keys the Console writes to `localStorage`: the session token,
/// the language and the theme. Never a provider secret.
const STORAGE_KEYS: &[&str] = &["TOKEN_STORAGE_KEY", "LANG_STORAGE_KEY", "THEME_STORAGE_KEY"];

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    fs::read_to_string(repository_root().join(relative)).unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

fn collect_sources(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display())) {
        let path = entry.expect("test: directory entry").path();
        if path.is_dir() {
            collect_sources(&path, files);
        } else if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "js" | "svelte"))
        {
            files.push(path);
        }
    }
}

/// Every `.js` and `.svelte` file under `console/src`, with its path
/// relative to the repository.
fn console_sources() -> Vec<(String, String)> {
    let mut files = Vec::new();
    collect_sources(&repository_root().join("console/src"), &mut files);
    files.sort();
    assert!(!files.is_empty(), "no console sources found");
    files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(repository_root())
                .expect("test: source under the repository")
                .to_string_lossy()
                .into_owned();
            let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {relative}: {error}"));
            (relative, source)
        })
        .collect()
}

/// The text of one method of the `api` object in `api.js`: from its name
/// to the next method (two-space indent) or the end of the object.
fn api_entry<'a>(api: &'a str, name: &str) -> &'a str {
    let start = api
        .find(&format!("\n  {name}: "))
        .unwrap_or_else(|| panic!("api.js has no method `{name}`"));
    let rest = &api[start + 1..];
    let end = rest
        .match_indices("\n  ")
        .map(|(index, _)| index)
        .find(|&index| {
            let line = &rest[index + 3..];
            line.starts_with("//")
                || line
                    .split_once(':')
                    .is_some_and(|(name, _)| !name.is_empty() && name.chars().all(char::is_alphanumeric))
        })
        .or_else(|| rest.find("\n};"))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// The body of `function name(...) { ... }` (or `async function`), braces
/// matched.
fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(&format!("function {name}("))
        .unwrap_or_else(|| panic!("no function `{name}`"));
    let open = start + source[start..].find('{').expect("test: function body");
    let mut depth = 0usize;
    for (offset, character) in source[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced body of `{name}`")
}

/// The `<input ... />` element that binds `variable`.
fn input_binding<'a>(page: &'a str, variable: &str) -> &'a str {
    let bind = page
        .find(&format!("bind:value={{{variable}}}"))
        .unwrap_or_else(|| panic!("no input binds `{variable}`"));
    let start = page[..bind].rfind("<input").expect("test: input element");
    let end = bind + page[bind..].find("/>").expect("test: input end");
    &page[start..end]
}

/// The API methods a source calls (`api.name(`).
fn api_calls(source: &str) -> BTreeSet<String> {
    source
        .match_indices("api.")
        .filter_map(|(index, _)| {
            let rest = &source[index + 4..];
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric()).collect();
            (!name.is_empty() && rest[name.len()..].starts_with('(')).then_some(name)
        })
        .collect()
}

/// G-1: the credential leaves the page once, in the body of a `PUT`; the URL
/// carries only the provider (or nothing), never the value.
#[test]
fn a_credential_is_sent_once_in_the_body_of_a_put() {
    let api = read("console/src/lib/api.js");
    for (name, body) in [
        ("setProviderCredential", "body: JSON.stringify({ value, name"),
        ("setConfigSecret", "body: JSON.stringify({ slot, value })"),
    ] {
        let entry = api_entry(&api, name);
        assert!(entry.contains("method: 'PUT'"), "{name} must PUT: {entry}");
        assert!(entry.contains(body), "{name} must send the value in the body: {entry}");
        let url = entry
            .lines()
            .find(|line| line.contains("request("))
            .unwrap_or_else(|| panic!("{name} has no request: {entry}"));
        assert!(
            !url.contains("value") && !url.contains("managementQuery") && !url.contains("URLSearchParams"),
            "{name} must not put anything but the path in its URL: {url}"
        );
    }
    assert!(
        api_entry(&api, "setProviderCredential")
            .contains("`/api/providers/${encodeURIComponent(provider)}/credential`"),
        "the credential URL is the path alone"
    );

    let page = read("console/src/pages/ProvidersPage.svelte");
    assert!(function_body(&page, "submitCredential").contains("api.setProviderCredential(selected, value,"));
    assert!(function_body(&page, "submitRootKey").contains("api.setConfigSecret('api_key', value, options)"));
}

/// G-1: the password fields are write-only (`type="password"`, never
/// autofilled) and cleared after every submit, whether it succeeded or not.
#[test]
fn the_password_fields_are_cleared_after_every_submit() {
    let page = read("console/src/pages/ProvidersPage.svelte");
    for (variable, submit) in [
        ("credentialValue", "submitCredential"),
        ("rootKeyValue", "submitRootKey"),
    ] {
        let input = input_binding(&page, variable);
        assert!(input.contains("type=\"password\""), "{variable}: {input}");
        assert!(input.contains("autocomplete=\"new-password\""), "{variable}: {input}");

        let body = function_body(&page, submit);
        let finally = body
            .find("} finally {")
            .unwrap_or_else(|| panic!("{submit} must clear the field in `finally`: {body}"));
        assert!(
            body[finally..].contains(&format!("{variable} = '';")),
            "{submit} must clear `{variable}` whatever the answer: {body}"
        );

        // Nothing else reads or keeps the value: its declaration, the
        // binding, the button's enabled check, the submit, and plain clears
        // (on a provider switch) are all.
        let outside = page.replace(body, "");
        for line in outside.lines().filter(|line| line.contains(variable)) {
            let line = line.trim();
            assert!(
                line == format!("let {variable} = $state('');")
                    || line == format!("bind:value={{{variable}}}")
                    || line == format!("{variable} = '';")
                    || (line.starts_with("disabled={") && line.contains(&format!("!{variable}.trim()"))),
                "`{variable}` is used outside its submit: {line}"
            );
        }
    }
    // No dynamic input type (a "show password" toggle), and no <form>: a
    // form submitted by the browser would put the fields in the URL.
    assert!(!page.contains("type={"), "no dynamic input type");
    assert!(!page.contains("<form"), "no form element");
}

/// G-1: no provider secret goes to `localStorage`, `sessionStorage`, a
/// cookie, the history or the URL. The Console stores only the session
/// token, the language and the theme.
#[test]
fn no_provider_secret_reaches_storage_or_a_url() {
    let mut stored = BTreeSet::new();
    for (path, source) in console_sources() {
        assert!(!source.contains("sessionStorage"), "{path} uses sessionStorage");
        assert!(!source.contains("indexedDB"), "{path} uses indexedDB");
        for (index, _) in source.match_indices("localStorage.setItem(") {
            let key: String = source[index + "localStorage.setItem(".len()..]
                .chars()
                .take_while(|c| *c != ',')
                .collect();
            assert!(
                STORAGE_KEYS.contains(&key.trim()),
                "{path} writes `{key}` to localStorage"
            );
            stored.insert(key.trim().to_string());
        }
    }
    assert_eq!(
        stored.len(),
        STORAGE_KEYS.len(),
        "the storage guard must see every key: {stored:?}"
    );

    for path in MANAGEMENT_SOURCES {
        let source = read(path);
        for forbidden in [
            "localStorage",
            "document.cookie",
            "navigate(",
            "location",
            "history.",
            "console.",
            "<form",
        ] {
            assert!(!source.contains(forbidden), "{path} must not use `{forbidden}`");
        }
    }
}

/// G-1/G-2: the management pages call only the management endpoints: every
/// mutation is a plan and apply, a credential or a test on the provider, and
/// never the raw file save or the whole-configuration `POST`.
#[test]
fn every_management_mutation_goes_through_the_management_endpoints() {
    let api = read("console/src/lib/api.js");
    for (name, prefix) in MANAGEMENT_API {
        let entry = api_entry(&api, name);
        let compact: String = entry.split_whitespace().collect();
        assert!(
            compact.contains(&format!("request({prefix}")),
            "{name} must call {prefix}: {entry}"
        );
    }
    let allowed: BTreeSet<&str> = MANAGEMENT_API.iter().map(|(name, _)| *name).collect();
    for path in MANAGEMENT_SOURCES {
        let source = read(path);
        for call in api_calls(&source) {
            assert!(allowed.contains(call.as_str()), "{path} calls api.{call}");
        }
        for forbidden in ["saveConfigFile", "saveConfig(", "/api/config/files", "fetch("] {
            assert!(!source.contains(forbidden), "{path} must not use `{forbidden}`");
        }
    }
}

/// The `kind` of every change `POST /api/config/plans` takes: the variants
/// of the gateway's `ChangeRequest`, in their snake-case spelling.
fn gateway_change_kinds() -> BTreeSet<String> {
    let manage = read("src/gateway/api/manage.rs");
    let start = manage
        .find("pub(super) enum ChangeRequest {")
        .expect("test: ChangeRequest enum");
    let body = &manage[start..];
    let body = &body[..body.find("\n}\n").expect("test: end of ChangeRequest")];
    body.lines()
        .filter_map(|line| line.strip_prefix("    "))
        .filter(|line| line.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|line| {
            let name: String = line.chars().take_while(char::is_ascii_alphanumeric).collect();
            let mut kind = String::new();
            for (index, c) in name.chars().enumerate() {
                if c.is_ascii_uppercase() && index > 0 {
                    kind.push('_');
                }
                kind.push(c.to_ascii_lowercase());
            }
            kind
        })
        .collect()
}

/// G-2: every change the Console plans is a kind the gateway takes, and
/// every typed kind has a Console surface (the raw file is the raw
/// editor's, through `PUT /api/config/files`).
#[test]
fn the_console_plans_every_typed_change_the_gateway_takes() {
    let gateway = gateway_change_kinds();
    assert!(
        gateway.contains("add_provider") && gateway.contains("raw_file"),
        "{gateway:?}"
    );
    let mut planned = BTreeSet::new();
    for path in MANAGEMENT_SOURCES {
        let source = read(path);
        for (index, _) in source.match_indices("('").chain(source.match_indices("kind: '")) {
            let (head, literal_start) = if source[index..].starts_with("kind: '") {
                ("plan", index + "kind: '".len())
            } else {
                let head_start = source[..index]
                    .rfind(|c: char| !c.is_alphanumeric())
                    .map_or(0, |position| position + 1);
                (&source[head_start..index], index + 2)
            };
            if !head.starts_with("plan") {
                continue;
            }
            let kind: String = source[literal_start..].chars().take_while(|c| *c != '\'').collect();
            assert!(
                gateway.contains(&kind),
                "{path} plans `{kind}`, which the gateway does not take"
            );
            planned.insert(kind);
        }
    }
    let typed: BTreeSet<String> = gateway.into_iter().filter(|kind| kind != "raw_file").collect();
    assert_eq!(planned, typed, "every typed change needs a Console surface");
}

/// G-3: a rollback shows what the restore writes (`plan`, every later
/// change included) and the recorded change it undoes (`undoes`), asks for
/// exactly the confirmation the 428 names, and resends bound to the
/// revision that 428 reported.
#[test]
fn a_rollback_shows_both_plans_and_confirms_with_the_refused_revision() {
    let api = read("console/src/lib/api.js");
    let entry = api_entry(&api, "rollbackConfig");
    assert!(entry.contains("headers: revisionHeaders(revision)"), "{entry}");
    assert!(
        entry.contains("body: JSON.stringify({ record_id: recordId, ...flags })"),
        "{entry}"
    );

    // The confirmations are exactly the gateway's rollback flags.
    let manage = read("src/gateway/api/manage.rs");
    let start = manage.find("pub struct RollbackBody {").expect("test: RollbackBody");
    let body = &manage[start..];
    let body = &body[..body.find("\n}").expect("test: end of RollbackBody")];
    let flags: Vec<&str> = body
        .lines()
        .filter_map(|line| line.trim().strip_suffix(": bool,"))
        .collect();
    let page = read("console/src/pages/ChangesPage.svelte");
    let expected = format!(
        "const ROLLBACK_CONFIRMATIONS = [{}];",
        flags
            .iter()
            .map(|flag| format!("'{flag}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        page.contains(&expected),
        "the Console must know every rollback flag: {expected}"
    );

    let send = function_body(&page, "sendRollback");
    assert!(
        send.contains("api.rollbackConfig(record.id, flags, { revision: boundRevision })"),
        "{send}"
    );
    assert!(send.contains("const refused = e?.body?.revision;"), "{send}");
    assert!(send.contains("revision: refused,"), "{send}");
    assert!(
        send.contains("plan: e.body.plan ?? ''") && send.contains("undoes: e.body.undoes ?? ''"),
        "{send}"
    );
    assert!(page.contains("{review.plan}") && page.contains("{review.undoes}"));
    assert!(page.contains("{result.plan}") && page.contains("{result.undoes}"));

    let confirm = function_body(&page, "confirmReview");
    assert!(
        confirm.contains("ROLLBACK_CONFIRMATIONS.includes(review.required)")
            && confirm.contains("{ ...review.flags, [review.required]: true }"),
        "{confirm}"
    );
    assert!(
        confirm.contains("sendRollback(review.record, flags, review.revision)"),
        "{confirm}"
    );
}

/// G-4: the raw editor is labelled "Advanced (raw)", keeps a file the
/// server marks `editable: false` (a secret-bearing file, served redacted)
/// read-only (a missing flag counts as read-only), and binds every save to
/// the loaded revision with `If-Match`.
#[test]
fn the_raw_editor_is_advanced_and_secret_files_are_read_only() {
    let english: serde_json::Value =
        serde_json::from_str(&read("console/src/lib/i18n/en.json")).expect("test: en.json is JSON");
    assert_eq!(english["nav"]["config"], "Advanced (raw)");
    assert_eq!(english["config"]["advancedTitle"], "Advanced (raw)");

    let page = read(RAW_EDITOR);
    assert!(page.contains("<h2>{t('config.advancedTitle')}</h2>"));
    assert!(
        page.contains("files.find((f) => f.filename === activeFile)?.editable === true"),
        "only a file the server marks editable can be edited"
    );
    assert!(
        page.lines().any(|line| line.trim() == "readonly={!activeEditable}"),
        "the textarea must carry the readonly attribute, not only the class"
    );
    assert!(
        page.contains("disabled={!hasChanges || saving || !activeEditable}"),
        "save must be disabled"
    );
    assert!(
        function_body(&page, "saveFile").contains("if (!activeFile || !hasChanges || !activeEditable) return;"),
        "a read-only file is never saved"
    );

    let save = function_body(&page, "saveConfirmingHighRisk");
    assert!(
        save.contains("api.saveConfigFile(filename, content, { revision: loadedRevision })"),
        "{save}"
    );
    assert!(
        save.contains("api.saveConfigFile(filename, content, { confirmHighRisk: true, revision })"),
        "{save}"
    );
    assert!(save.contains("window.confirm(confirmationText(e))"), "{save}");
    let calls = api_calls(&page);
    assert!(
        calls
            .iter()
            .all(|call| ["getConfigFilesWithRevision", "getConfig", "saveConfigFile"].contains(&call.as_str())),
        "the raw editor saves only through the file endpoint: {calls:?}"
    );
    assert!(
        page.contains("<ErrorNotice error={saveError} />"),
        "a refused save shows its reason"
    );
}

/// D-087/D-096: a refused hooks write (a 403 at `supervised` autonomy for a
/// Medium or High hook change) shows the gateway's `reason`: the page keeps
/// the refusal whole and renders it with `ErrorNotice`.
#[test]
fn the_hooks_page_shows_the_reason_of_a_refused_write() {
    let page = read(HOOKS_PAGE);
    assert!(page.contains("import ErrorNotice from '../lib/ErrorNotice.svelte';"));
    assert!(page.contains("<ErrorNotice error={actionError} />"));
    assert!(page.contains("<ErrorNotice error={loadError} />"));
    assert!(
        !page.contains(".message"),
        "the hooks page must not flatten a refusal to its message"
    );
    for fallback in ["saveFailed", "deleteFailed"] {
        assert!(
            page.contains(&format!(
                "actionError = error instanceof Error ? error : t('hooks.{fallback}');"
            )),
            "{fallback}"
        );
    }
}

/// D-099 1: switching provider clears both password fields (and the
/// reference name) before anything else, so a value typed for one provider
/// is never filed under another or kept hidden.
#[test]
fn switching_provider_clears_both_password_fields() {
    let page = read("console/src/pages/ProvidersPage.svelte");
    let select = function_body(&page, "select");
    let switched = select.find("selected = provider;").expect("test: select switches");
    for clear in ["credentialValue = '';", "rootKeyValue = '';", "credentialName = '';"] {
        let at = select
            .find(clear)
            .unwrap_or_else(|| panic!("select must `{clear}`: {select}"));
        assert!(at < switched, "`{clear}` must come before the switch: {select}");
    }
}

/// D-099 2: a masked text from the gateway (`[redacted]`, `[REDACTED]`,
/// `***`) is display-only. It is never prefilled into an editable field, a
/// change holding one is never sent, and a masked provider cannot be
/// addressed (its name would go into a URL).
#[test]
fn masked_values_are_never_prefilled_or_sent() {
    let flow = read("console/src/lib/change-flow.svelte.js");
    let masked = function_body(&flow, "isMasked");
    assert!(
        masked.contains("value.toLowerCase().includes('[redacted]')") && masked.contains("value.includes('***')"),
        "{masked}"
    );
    let preview = flow.find("  async preview(change) {").expect("test: preview");
    let preview = &flow[preview..];
    let guard = preview
        .find("if (holdsMask(change)) {")
        .expect("test: preview refuses a mask");
    assert!(flow.contains("transport = { plan: api.planChange, apply: api.applyChange }"));
    let snapshot = preview
        .find("const snapshot = JSON.parse(JSON.stringify(change));")
        .expect("test: preview snapshots the unmasked change");
    let send = preview
        .find("this.transport.plan(snapshot)")
        .expect("test: preview plans");
    assert!(
        guard < snapshot && snapshot < send,
        "the mask guard must precede snapshot and request"
    );

    // Every form field that can receive a loaded provider, model or endpoint
    // is filled only through `unmasked(...)` (or cleared).
    for path in [
        "console/src/pages/ProvidersPage.svelte",
        "console/src/pages/ModelsPage.svelte",
        "console/src/pages/RoutingPage.svelte",
    ] {
        let page = read(path);
        for line in page.lines().map(str::trim) {
            for field in MASKABLE_FIELDS {
                if let Some(value) = line.strip_prefix(&format!("{field} = ")) {
                    assert!(
                        value == "'';" || value.starts_with("unmasked("),
                        "{path} fills `{field}` without unmasking: {line}"
                    );
                }
            }
        }
    }

    let providers = read("console/src/pages/ProvidersPage.svelte");
    assert!(function_body(&providers, "select").contains("if (isMasked(provider)) return;"));
    assert!(providers.contains("disabled={credentialBusy || testBusy || isMasked(entry.provider)}"));
    assert!(providers.contains("disabled={flow.busy || isMasked(entry.provider)}"));
    // An empty endpoint field never means "clear": clearing is its own action.
    let endpoint = function_body(&providers, "planEndpoint");
    assert!(
        endpoint.contains("if (!url) return;") && !endpoint.contains("null"),
        "{endpoint}"
    );
    assert!(function_body(&providers, "planClearEndpoint").contains("api_url: null"));

    // An update leaves out the fields it does not change.
    let routing = read("console/src/pages/RoutingPage.svelte");
    let route = function_body(&routing, "planRoute");
    assert!(
        route.contains("if (provider && provider !== loaded?.provider) change.provider = provider;")
            && route.contains("if (model && model !== loaded?.model) change.model = model;"),
        "{route}"
    );
}

/// R-PMMG-confirm N-1/N-3: a route update omits only what equals the same
/// route (hint and operation) as loaded by "Edit"; that comparison is
/// forgotten on an operation switch and after an apply, and so are the
/// per-form masked-value warnings.
#[test]
fn a_route_update_compares_only_with_the_same_loaded_route() {
    let routing = read("console/src/pages/RoutingPage.svelte");
    assert!(function_body(&routing, "editRoute").contains("loadedRoute = { ...route, operation: routeOperation };"));
    let route = function_body(&routing, "planRoute");
    assert!(
        route.contains("loadedRoute?.hint === hint && loadedRoute?.operation === routeOperation"),
        "{route}"
    );
    let switch = function_body(&routing, "showRoutes");
    assert!(
        switch.contains("loadedRoute = null;") && switch.contains("routeMaskedPrefill = false;"),
        "{switch}"
    );
    assert!(routing.contains("const flow = new ChangeFlow(afterApply);"));
    let applied = function_body(&routing, "afterApply");
    for reset in [
        "loadedRoute = null;",
        "routeMaskedPrefill = false;",
        "agentMaskedPrefill = false;",
    ] {
        assert!(applied.contains(reset), "afterApply must `{reset}`: {applied}");
    }
    assert!(!routing.contains("let maskedPrefill"), "the warning is per form");

    let models = read("console/src/pages/ModelsPage.svelte");
    assert!(models.contains("const flow = new ChangeFlow(afterApply);"));
    assert!(function_body(&models, "afterApply").contains("maskedPrefill = false;"));
}

/// D-099 3: the hooks page has no enable/disable control (the route never
/// existed, and a hook-state write belongs to HOOKS-ENV under D-096).
#[test]
fn the_hooks_page_has_no_toggle() {
    let api = read("console/src/lib/api.js");
    assert!(!api.contains("toggleHook"));
    assert!(!api.contains("/api/hooks/${encodeURIComponent(id)}/toggle"));
    let page = read(HOOKS_PAGE);
    for gone in ["toggle", "hooksEnabled", "formEnabled", "globalToggleHint"] {
        assert!(!page.contains(gone), "the hooks page still has `{gone}`");
    }
}

/// D-099 4: every management page loads once on mount without tracking its
/// state, so typing into a filter sends nothing until the operator asks.
#[test]
fn management_pages_load_on_mount_and_on_request_only() {
    for path in MANAGEMENT_SOURCES.iter().filter(|path| path.contains("/pages/")) {
        let page = read(path);
        let imports = page.lines().find_map(|line| {
            let line = line.trim();
            let names = line.strip_prefix("import { ")?.strip_suffix(" } from 'svelte';")?;
            names
                .split(',')
                .map(str::trim)
                .any(|name| name == "untrack")
                .then_some(names)
        });
        assert!(imports.is_some(), "{path} must import untrack from svelte");
        let effect = page
            .find("$effect(() => {")
            .unwrap_or_else(|| panic!("{path} has no mount effect"));
        let body = &page[effect..];
        let body = &body[..body.find("});").expect("test: effect end")];
        assert!(body.contains("untrack("), "{path} tracks state in its load: {body}");
        assert_eq!(page.matches("$effect(").count(), 1, "{path} has another effect");
    }
    let models = read("console/src/pages/ModelsPage.svelte");
    assert!(
        !models.contains("oninput") && !models.contains("onchange"),
        "no load per keystroke"
    );
}

/// D-099 5: no server text is rendered as HTML; the confirmed retry is one
/// send, never a loop; a rollback round is confirmed only by the button.
#[test]
fn rendering_and_confirmation_are_bounded() {
    for (path, source) in console_sources() {
        assert!(!source.contains("{@html"), "{path} renders HTML");
        assert!(!source.contains("innerHTML"), "{path} writes innerHTML");
    }
    let flow = read("console/src/lib/change-flow.svelte.js");
    let send = function_body(&flow, "sendConfirming");
    assert_eq!(
        send.matches("send({").count(),
        2,
        "exactly the first send and one retry: {send}"
    );
    assert!(!send.contains("while") && !send.contains("for ("), "{send}");
    let changes = read("console/src/pages/ChangesPage.svelte");
    assert_eq!(
        changes.matches("confirmReview").count(),
        2,
        "confirmReview is defined once and called only from its button"
    );
    assert!(changes.contains("onclick={confirmReview}"));
}

/// The management reads send only the query parameters the endpoints take
/// (the gateway refuses any other with 400).
#[test]
fn management_reads_send_only_the_query_parameters_the_endpoints_take() {
    let api = read("console/src/lib/api.js");
    for (name, _) in MANAGEMENT_API {
        let entry = api_entry(&api, name);
        assert!(!entry.contains("URLSearchParams"), "{name} builds its own query");
        assert!(!entry.contains("Date.now"), "{name} busts the cache");
        for (index, _) in entry.match_indices("['") {
            let key: String = entry[index + 2..].chars().take_while(|c| *c != '\'').collect();
            assert!(MANAGEMENT_QUERY_KEYS.contains(&key.as_str()), "{name} sends `{key}`");
        }
    }
}

/// G-3: a high-risk change is confirmed by the operator after seeing the
/// refusal, and the confirmed retry names the revision the 428 reported in
/// `If-Match` (D-049 B), never `*` and never the flag alone.
#[test]
fn a_high_risk_change_is_confirmed_with_the_refused_revision() {
    let api = read("console/src/lib/api.js");
    let headers = function_body(&api, "revisionHeaders");
    assert!(headers.contains("{ 'If-Match': `\"${revision}\"` }"), "{headers}");
    assert!(function_body(&api, "confirmSuffix").contains("'?confirm_high_risk=true'"));
    for name in ["applyChange", "setConfigSecret"] {
        let entry = api_entry(&api, name);
        assert!(entry.contains("confirmSuffix(confirmHighRisk)"), "{name}: {entry}");
        assert!(entry.contains("headers: revisionHeaders(revision)"), "{name}: {entry}");
    }

    let flow = read("console/src/lib/change-flow.svelte.js");
    let send = function_body(&flow, "sendConfirming");
    assert!(send.contains("send({ revision, confirmHighRisk: false })"), "{send}");
    assert!(send.contains("const refused = e?.body?.revision;"), "{send}");
    assert!(
        send.contains("e?.status !== 428 || !refused || !window.confirm(confirmationText(e))"),
        "{send}"
    );
    assert!(
        send.contains("send({ revision: refused, confirmHighRisk: true })"),
        "{send}"
    );
    // The operator sees the refusal, its reasons and the refused plan.
    let text = function_body(&flow, "confirmationText");
    assert!(
        text.contains("error.body?.risk_reasons") && text.contains("error.body.plan"),
        "{text}"
    );
    // An apply names the revision its plan was computed against.
    assert!(flow.contains("transport = { plan: api.planChange, apply: api.applyChange }"));
    assert!(
        flow.contains("(options) => this.transport.apply(pending.change, options),\n        pending.plan.revision")
    );
}

/// D-087 (1): a `403` at `supervised` autonomy carries a `reason`; the
/// Console shows it next to the error on every management page.
#[test]
fn a_refusal_shows_the_gateway_reason() {
    let api = read("console/src/lib/api.js");
    assert!(
        api.contains("this.reason = typeof this.body?.reason === 'string' ? this.body.reason : '';"),
        "ApiError must keep the reason"
    );
    let notice = read("console/src/lib/ErrorNotice.svelte");
    assert!(notice.contains("typeof error?.reason === 'string' ? error.reason : ''"));
    assert!(notice.contains("{#if reason}") && notice.contains("<p>{reason}</p>"));
    for path in MANAGEMENT_SOURCES.iter().filter(|path| path.contains("/pages/")) {
        let page = read(path);
        assert!(
            page.contains("<ErrorNotice error="),
            "{path} must show refusals with ErrorNotice"
        );
        assert!(
            !page.contains("error.message") && !page.contains("e.message"),
            "{path} must not flatten a refusal to its message"
        );
    }
}

/// Every language carries every key, and every key a management source
/// names exists.
#[test]
fn every_language_has_every_management_key() {
    fn flatten(prefix: &str, value: &serde_json::Value, keys: &mut BTreeSet<String>) {
        if let Some(object) = value.as_object() {
            for (key, child) in object {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&path, child, keys);
            }
        } else {
            keys.insert(prefix.to_string());
        }
    }
    let mut languages = Vec::new();
    for language in ["en", "zh", "ru", "ka"] {
        let text = read(&format!("console/src/lib/i18n/{language}.json"));
        let value: serde_json::Value = serde_json::from_str(&text).expect("test: dictionary is JSON");
        let mut keys = BTreeSet::new();
        flatten("", &value, &mut keys);
        languages.push((language, keys));
    }
    let (_, english) = &languages[0];
    for (language, keys) in &languages {
        assert_eq!(keys, english, "{language} differs from en");
    }
    for path in MANAGEMENT_SOURCES.iter().chain([&RAW_EDITOR, &HOOKS_PAGE]) {
        let source = read(path);
        for (index, _) in source.match_indices("t('") {
            // `t(` itself, not the tail of another name (`setConfigSecret('`).
            if source[..index].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            let key: String = source[index + 3..].chars().take_while(|c| *c != '\'').collect();
            assert!(english.contains(&key), "{path} names the missing key `{key}`");
        }
    }
}

#[test]
fn model_settings_controls_use_catalog_constraints_and_the_transaction_flow() {
    let page = read("console/src/pages/ModelsPage.svelte");
    for fragment in [
        "api.getModelSettings(entry.provider, entry.model)",
        "settingsCatalog.levels",
        "settingsCatalog.parameters",
        "declaration.min",
        "declaration.max",
        "declaration.step",
        "declaration.levels",
        "set_model_settings",
        "flow.preview(",
    ] {
        assert!(page.contains(fragment), "missing model settings contract: {fragment}");
    }
    assert!(
        page.contains("request !== settingsRequest"),
        "late reads must not overwrite the active model"
    );
}
