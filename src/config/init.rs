use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ── Spec preset enum ────────────────────────────────────────────

/// Configuration preset for `prx init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Spec {
    /// Bare-minimum: memory + agent only
    Minimal,
    /// Production server: memory + agent + network + security + tools + integrations
    Server,
    /// Everything enabled
    Full,
}

impl Spec {
    /// Human-readable name for display.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Server => "server",
            Self::Full => "full",
        }
    }

    /// Generate the full configuration tree into `target_dir`, printing the
    /// report to stdout/stderr.
    ///
    /// `prx init` returns from `main` *before* any tracing subscriber is
    /// installed, so anything this path logged through `tracing` went nowhere:
    /// the operator saw neither the preserved-key list, nor the archive
    /// directory, nor the warning that their whole configuration had been
    /// reset. The report is therefore written directly to stdout/stderr, and
    /// [`InitReport::into_result`] turns a reset into a non-zero exit.
    pub async fn generate(self, target_dir: &Path, force: bool) -> Result<()> {
        let report = self.generate_reported(target_dir, force).await?;
        report.print();
        report.into_result()
    }

    /// [`generate`](Self::generate), then the decision backend chosen with
    /// `prx init --decision-*` (CDP-14). The choice is checked before
    /// anything is written (a hosted kind is refused: it needs a credential);
    /// it is then applied to the generated configuration through the shared
    /// management service, as its own recorded transaction.
    ///
    /// # Errors
    ///
    /// A refused choice (nothing written), or the generation's or the
    /// transaction's failure.
    // `prx init` has no log subscriber; stdout is its only channel, as for
    // `InitReport::print`.
    #[allow(clippy::print_stdout)]
    pub async fn generate_with_decision(
        self,
        target_dir: &Path,
        force: bool,
        decision: Option<&crate::causal_cli::DecisionSetup>,
    ) -> Result<()> {
        if let Some(decision) = decision {
            decision.check()?;
        }
        self.generate(target_dir, force).await?;
        if let Some(decision) = decision {
            let config = super::Config::load_from_path(&target_dir.join("config.toml"), target_dir.join("workspace"))?;
            let plan = decision.apply(config).await?;
            print!("{plan}");
            println!("Decision backend configured ({}).", decision.kind.as_str());
        }
        Ok(())
    }

    /// [`generate`](Self::generate) without the printing, so callers (and
    /// tests) can inspect exactly what the operator is told.
    pub async fn generate_reported(self, target_dir: &Path, force: bool) -> Result<InitReport> {
        // 1. Check for existing configuration
        let config_path = target_dir.join("config.toml");
        let regenerating = config_path.exists();
        if regenerating && !force {
            bail!(
                "Configuration already exists at {}. Use --force to overwrite.",
                target_dir.display()
            );
        }

        // 2. Render the complete managed configuration generation.
        let fragments = vec![
            ("memory.toml".to_string(), memory_template(self)),
            ("channels.toml".to_string(), channels_template(self)),
            ("network.toml".to_string(), network_template(self)),
            ("security.toml".to_string(), security_template(self)),
            ("scheduler.toml".to_string(), scheduler_template(self)),
            ("agent.toml".to_string(), agent_template(self)),
            ("identity.toml".to_string(), identity_template(self)),
            ("routing.toml".to_string(), routing_template(self)),
            ("tools.toml".to_string(), tools_template(self)),
            ("integrations.toml".to_string(), integrations_template(self)),
            ("nodes.toml".to_string(), nodes_template(self)),
            ("cost.toml".to_string(), cost_template(self)),
            ("observability.toml".to_string(), observability_template(self)),
        ];

        // 3. Regenerating over an existing tree refreshes structure, comments and
        // defaults only: every value an operator wrote explicitly is carried over.
        // `--force` is a repair operation, not a reset.
        let main_toml = main_config_template(self);
        let workspace_dir = target_dir.join("workspace");
        let mut report = InitReport::default();
        let archive = if regenerating {
            archive_previous_config(target_dir, &config_path)?
        } else {
            None
        };
        // Recorded before anything is written, because the sidecar this run
        // writes describes this build's defaults, not the previous build's.
        let previous_defaults = if regenerating {
            read_managed_defaults(target_dir)
        } else {
            None
        };
        let managed_defaults = render_managed_defaults(&main_toml, &fragments)?;
        let mut causal_guard = false;
        let mut preserved_revision = None;
        let (effective_main, effective_fragments) = if regenerating {
            let (merged_main, merged_fragments, outcome) =
                preserve_previous_settings(&config_path, &main_toml, &fragments, previous_defaults.as_ref())?;
            report.record_preservation(&outcome, archive.as_deref());
            causal_guard = outcome.causal_guard;
            preserved_revision = outcome.base_revision;
            (merged_main, merged_fragments)
        } else {
            (main_toml.clone(), fragments.clone())
        };

        // 4. Stage and validate the complete effective configuration before
        // mutation. Unknown/legacy fragment names remain excluded by the loader
        // and are retained unchanged by the existing mutation planner.
        let plan = match super::files::plan_mutation(&config_path, &workspace_dir, effective_main, effective_fragments)
        {
            Ok(plan) => plan,
            Err(error) if causal_guard => return Err(CausalRegenerationError(error).into()),
            Err(error) if error.is::<crate::causal_tree::config::MigrationError>() => return Err(error),
            // A readable tree without decision-plane settings can retain the
            // existing repair fallback for other invalid settings; it
            // is not allowed to do so quietly. The operator's values are in the
            // archive, the reason is printed, and `into_result` exits non-zero
            // so neither a human nor a script can mistake this for a clean run.
            Err(error) if regenerating => {
                report.record_reset(
                    &format!("the preserved settings do not validate against this build ({error:#})"),
                    archive.as_deref(),
                );
                super::files::plan_mutation(&config_path, &workspace_dir, main_toml, fragments)?
            }
            Err(error) => return Err(error),
        };
        if let Some(revision) = preserved_revision.as_deref() {
            // The same snapshot supplied all preserved values. Reject a
            // concurrent edit instead of overwriting it.
            super::files::commit_mutation_guarded(plan, Some(revision), |_| Ok(())).await?;
        } else {
            super::files::commit_mutation_atomically(plan).await?;
        }

        // 5. The mutation is on disk from here on. A failure in the steps that
        // follow is still a failure, but it must not take the report with it:
        // returning `Err` here used to discard the reset warning and the
        // archive path, leaving the operator with defaults, an exit code and
        // an empty stdout. The failure is carried in the report instead, so
        // `print` still shows what committed and `into_result` still exits
        // non-zero — now naming both the mutation and the error that stopped
        // the run.
        if let Err(error) = finish_after_commit(target_dir, &workspace_dir, &managed_defaults) {
            report
                .notices
                .insert(0, format!("PRX configuration committed ({spec})", spec = self.name()));
            report
                .notices
                .insert(1, format!("  Config dir: {}", target_dir.display()));
            report
                .notices
                .push("  Config files: config.toml + 13 managed fragments".to_string());
            report.record_post_commit_failure(&error);
            return Ok(report);
        }

        // 6. Summary
        report
            .notices
            .insert(0, format!("PRX configuration initialized ({spec})", spec = self.name()));
        report
            .notices
            .insert(1, format!("  Config dir: {}", target_dir.display()));
        report
            .notices
            .push("  Capabilities are always available; activation follows concrete configuration".to_string());
        report
            .notices
            .push("  Config files: config.toml + 13 managed fragments".to_string());
        report.notices.push("  Workspace .md files scaffolded".to_string());

        Ok(report)
    }
}

/// What `prx init` tells the operator, kept separate from how it is printed.
///
/// `prx init` runs before any tracing subscriber exists, so this is written to
/// stdout/stderr rather than logged. Holding it as data also lets tests assert
/// on the exact lines an operator sees instead of on a log sink that would be
/// empty in production anyway.
#[derive(Debug, Default)]
pub struct InitReport {
    notices: Vec<String>,
    warnings: Vec<String>,
    /// Set when the operator's values were discarded and bare templates were
    /// written instead. Carries the full explanation, including the archive.
    reset: Option<String>,
    /// Keys the folder could not place in the regenerated tree. Each one is a
    /// value the operator wrote that this run did not carry over.
    unpreserved: Vec<String>,
    /// Keys this build can no longer represent because the template now writes
    /// a scalar where the previous tree held a table (or the reverse). Same
    /// class of loss as `unpreserved`, different cause.
    dropped: Vec<String>,
    /// Rendered failure of a step that ran after the configuration mutation
    /// committed. The tree on disk already changed, so this is reported next to
    /// the mutation rather than in place of it.
    post_commit_failure: Option<String>,
}

impl InitReport {
    /// Informational lines, in the order they are printed to stdout.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Warning lines, in the order they are printed to stderr.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Whether the operator's explicit settings were discarded.
    #[must_use]
    pub const fn was_reset_to_templates(&self) -> bool {
        self.reset.is_some()
    }

    /// The failure that stopped the run after the configuration mutation had
    /// already committed, if there was one.
    #[must_use]
    pub fn post_commit_failure(&self) -> Option<&str> {
        self.post_commit_failure.as_deref()
    }

    /// Write the report where `prx init` can actually be read: stdout for the
    /// summary, stderr for anything the operator has to act on.
    // `prx init` has no log subscriber and no TUI; stdout/stderr is the only
    // channel it has, which is the whole point of this type.
    #[allow(clippy::print_stdout, clippy::print_stderr)]
    pub fn print(&self) {
        for line in &self.notices {
            println!("{line}");
        }
        for line in &self.warnings {
            eprintln!("{line}");
        }
    }

    /// Keys this run could not fold, in the order they are reported.
    #[must_use]
    pub fn unpreserved_keys(&self) -> &[String] {
        &self.unpreserved
    }

    /// Keys this run reset because the regenerated templates can no longer
    /// represent their shape, in the order they are reported.
    #[must_use]
    pub fn dropped_keys(&self) -> &[String] {
        &self.dropped
    }

    /// A reset is a data-loss event, so it ends the command non-zero even
    /// though the tree on disk is valid and usable. Individual keys that could
    /// not be folded are the same kind of loss on a smaller scale and end the
    /// command the same way: silence is what made this class of bug survive.
    /// Keys this build can no longer represent are lost just as completely, so
    /// they end the command non-zero too, listed under their own heading.
    pub fn into_result(self) -> Result<()> {
        // A step that ran after the mutation committed failed: the operator
        // needs the failure *and* whatever the mutation did to their settings,
        // so both are named in the same error.
        if let Some(failure) = self.post_commit_failure {
            return Err(match self.reset {
                Some(_) => anyhow::anyhow!(
                    "the configuration was reset to this build's templates (see the warning above) \
                     and the run then failed: {failure}"
                ),
                None => anyhow::anyhow!(failure),
            });
        }
        if let Some(reason) = self.reset {
            return Err(anyhow::anyhow!(reason));
        }
        if self.unpreserved.is_empty() && self.dropped.is_empty() {
            return Ok(());
        }
        let mut parts = Vec::new();
        if !self.unpreserved.is_empty() {
            parts.push(format!(
                "{} setting(s) could not be folded into the regenerated configuration: {}",
                self.unpreserved.len(),
                self.unpreserved.join(", ")
            ));
        }
        if !self.dropped.is_empty() {
            parts.push(format!(
                "{} setting(s) this build can no longer represent were reset: {}",
                self.dropped.len(),
                self.dropped.join(", ")
            ));
        }
        Err(anyhow::anyhow!(parts.join("; ")))
    }

    fn record_preservation(&mut self, outcome: &PreservationOutcome, archive: Option<&Path>) {
        if let Some(path) = archive {
            self.notices
                .push(format!("  Previous configuration archived to {}", path.display()));
        }
        if !outcome.causal_moved.is_empty() || !outcome.causal_dropped.is_empty() {
            self.notices.push(format!(
                "  Normalized legacy decision-plane settings: moved [{}]; retired defaults [{}]",
                outcome.causal_moved.join(", "),
                outcome.causal_dropped.join(", ")
            ));
        }
        if outcome.kept.is_empty() {
            self.notices
                .push("  No explicit settings differed from the regenerated templates".to_string());
        } else {
            self.notices.push(format!(
                "  Preserved {} explicit setting(s): {}",
                outcome.kept.len(),
                outcome.kept.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !outcome.added_defaults.is_empty() {
            self.notices.push(format!(
                "  Added {} new default(s) this version ships: {}",
                outcome.added_defaults.len(),
                outcome.added_defaults.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !outcome.refreshed.is_empty() {
            self.notices.push(format!(
                "  Reset {} unset setting(s) to refreshed defaults: {}",
                outcome.refreshed.len(),
                outcome.refreshed.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !outcome.dropped.is_empty() {
            self.dropped = outcome.dropped.iter().cloned().collect();
            self.warnings.push(format!(
                "WARNING: reset {} setting(s) this build can no longer represent: {}",
                self.dropped.len(),
                self.dropped.join(", ")
            ));
        }
        if !outcome.unfoldable.is_empty() {
            self.unpreserved = outcome.unfoldable.iter().cloned().collect();
            self.warnings.push(format!(
                "WARNING: could not fold {} setting(s) into the regenerated templates, so they now hold \
                 this version's defaults: {}. Copy them back from {}.",
                self.unpreserved.len(),
                self.unpreserved.join(", "),
                archive.map_or_else(
                    || "the previous configuration (NOT ARCHIVED)".to_string(),
                    |path| path.display().to_string()
                )
            ));
        }
    }

    fn record_reset(&mut self, reason: &str, archive: Option<&Path>) {
        let archive = archive.map_or_else(
            || "NOT ARCHIVED - the previous files were not copied".to_string(),
            |path| path.display().to_string(),
        );
        let message = format!(
            "WARNING: EVERY EXPLICIT SETTING WAS RESET TO THE SHIPPED TEMPLATES because {reason}. \
             Your previous configuration is at {archive} - copy the values you still want back into \
             config.toml or config.d/, then run `prx doctor`. Nothing was lost on disk, but this \
             installation is now running on defaults."
        );
        self.warnings.push(message.clone());
        self.reset = Some(message);
    }

    /// Record a failure of a step that ran after the configuration mutation
    /// committed, keeping everything already recorded about that mutation.
    fn record_post_commit_failure(&mut self, error: &anyhow::Error) {
        let rendered = format!("{error:#}");
        self.warnings.push(format!(
            "ERROR: the configuration above is already written, but this run then failed: {rendered}. \
             Fix the reported cause and run `prx init --force` again; the archive named above is kept."
        ));
        self.post_commit_failure = Some(rendered);
    }
}

// ── Explicit-setting preservation across `prx init --force` ──────
//
// Regenerating the managed templates is how `prx init --force` repairs stale
// structure and refreshes comments. It must not be how an operator loses the
// values they typed. Every key an operator wrote in `config.toml` or in a
// managed `config.d/*.toml` fragment is carried into the regenerated tree: keys
// the template also writes keep the operator's value, keys the template does
// not mention are re-inserted into the table they came from. Templates supply
// structure, comments and defaults only.

/// How many suffixed names `--force` will try before refusing to archive.
const MAX_ARCHIVE_NAME_ATTEMPTS: u32 = 1000;

/// Sidecar recording which values in the generated tree came from this build's
/// templates. Without it `--force` cannot tell a value the operator typed from
/// a default an older `prx init` wrote, and every stale default survives
/// forever.
const MANAGED_DEFAULTS_FILE: &str = ".init-defaults.toml";

/// Outcome of folding a previous configuration into freshly rendered templates.
///
/// The three classifications an operator is shown are deliberately distinct:
/// `kept` is a value they wrote and keeps winning, `added_defaults` is a key
/// this build's templates introduced, and `refreshed` is a key they never
/// touched whose shipped default has since moved. `dropped` and `unfoldable`
/// are losses and are reported as warnings instead.
#[derive(Debug, Default)]
struct PreservationOutcome {
    kept: BTreeSet<String>,
    added_defaults: BTreeSet<String>,
    refreshed: BTreeSet<String>,
    dropped: BTreeSet<String>,
    unfoldable: BTreeSet<String>,
    causal_guard: bool,
    base_revision: Option<String>,
    causal_moved: Vec<&'static str>,
    causal_dropped: Vec<&'static str>,
}

#[derive(Debug, thiserror::Error)]
#[error("Decision-plane configuration regeneration refused; existing configuration was not replaced: {0}")]
struct CausalRegenerationError(#[source] anyhow::Error);

/// Read the defaults the previous `prx init` recorded for this tree.
///
/// `None` means there is no usable record — an older tree, or a file the
/// operator removed — and the fold then treats every value on disk as
/// explicitly chosen, which is the safe direction.
fn read_managed_defaults(target_dir: &Path) -> Option<BTreeMap<Vec<String>, toml::Value>> {
    let raw = std::fs::read_to_string(target_dir.join(MANAGED_DEFAULTS_FILE)).ok()?;
    let document = toml::from_str::<toml::Value>(&raw).ok()?;
    let entries = document.get("entry")?.as_array()?;
    let mut defaults = BTreeMap::new();
    for entry in entries {
        let Some(segments) = entry.get("path").and_then(toml::Value::as_array) else {
            continue;
        };
        let Some(value) = entry.get("value") else {
            continue;
        };
        let mut path = Vec::with_capacity(segments.len());
        for segment in segments {
            match segment.as_str() {
                Some(segment) => path.push(segment.to_string()),
                None => {
                    path.clear();
                    break;
                }
            }
        }
        if path.is_empty() {
            continue;
        }
        defaults.insert(path, value.clone());
    }
    Some(defaults)
}

/// Render the sidecar for the templates this run is about to write. Paths are
/// stored as explicit string arrays so a key containing a dot round-trips.
fn render_managed_defaults(main_template: &str, fragment_templates: &[(String, String)]) -> Result<String> {
    let mut leaves = Vec::new();
    for source in std::iter::once(main_template).chain(fragment_templates.iter().map(|(_, body)| body.as_str())) {
        let value =
            toml::from_str::<toml::Value>(source).context("Generated configuration template is not valid TOML")?;
        collect_toml_leaves(&value, &mut Vec::new(), &mut leaves);
    }
    let mut rendered = String::from(
        "# Written by `prx init`. It records which values in this tree came from the
         # shipped templates, so a later `prx init --force` can tell a value you wrote
         # (preserved) from a default that has since changed (reset). Deleting this file
         # only makes --force preserve every value currently on disk.
",
    );
    rendered.push_str(&format!("generated_by = \"{}\"\n", env!("CARGO_PKG_VERSION")));
    let mut seen = BTreeSet::new();
    for (path, value) in &leaves {
        if !seen.insert(path.clone()) {
            continue;
        }
        let segments = toml::Value::Array(
            path.iter()
                .map(|segment| toml::Value::String(segment.clone()))
                .collect::<Vec<_>>(),
        );
        rendered.push_str(&format!(
            "\n[[entry]]\npath = {}\nvalue = {}\n",
            render_toml_value(&segments)?,
            render_toml_value(value)?
        ));
    }
    Ok(rendered)
}

fn write_managed_defaults(target_dir: &Path, rendered: &str) -> Result<()> {
    let path = target_dir.join(MANAGED_DEFAULTS_FILE);
    std::fs::write(&path, rendered).with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Copy the previous `config.toml` and every `config.d` fragment into a
/// timestamped sibling directory before the regeneration touches them.
fn archive_previous_config(target_dir: &Path, config_path: &Path) -> Result<Option<PathBuf>> {
    if !config_path.exists() {
        return Ok(None);
    }
    // Two `--force` runs inside the same second used to land on the same
    // directory name, and `create_dir_all` + `fs::copy` overwrote the first
    // archive with the second. The stamp now carries milliseconds and the
    // directory is created exclusively, so a collision produces a second
    // archive instead of destroying one.
    let base = format!(
        "config.d.pre-{}-{}",
        env!("CARGO_PKG_VERSION"),
        chrono::Local::now().format("%Y%m%d%H%M%S%3f")
    );
    let archive = create_unique_archive_dir(target_dir, &base)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to restrict config archive {}", archive.display()))?;
    }

    let mut sources = vec![config_path.to_path_buf()];
    sources.extend(super::files::list_config_fragment_paths(config_path)?);
    // The defaults sidecar is what tells the *next* `--force` which values on
    // disk are stale shipped defaults rather than operator choices. Restoring
    // `config.toml` from an archive that does not carry it leaves the two out
    // of step, so it is archived alongside the files it describes. Trees
    // written by an older build have no sidecar; that is not an error.
    let managed_defaults = target_dir.join(MANAGED_DEFAULTS_FILE);
    if managed_defaults.is_file() {
        sources.push(managed_defaults);
    }
    for source in sources {
        let Some(name) = source.file_name() else {
            continue;
        };
        let destination = archive.join(name);
        std::fs::copy(&source, &destination).with_context(|| format!("Failed to archive {}", source.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to restrict archived {}", destination.display()))?;
        }
    }
    Ok(Some(archive))
}

/// Create `target_dir/<base>`, falling back to `<base>-2`, `<base>-3`, ... when
/// the name is taken. The directory is created exclusively, so a concurrent or
/// same-millisecond `--force` never writes into an archive that already holds
/// another run's files.
fn create_unique_archive_dir(target_dir: &Path, base: &str) -> Result<PathBuf> {
    for attempt in 1..=MAX_ARCHIVE_NAME_ATTEMPTS {
        let candidate = if attempt == 1 {
            target_dir.join(base)
        } else {
            target_dir.join(format!("{base}-{attempt}"))
        };
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to create config archive {}", candidate.display()));
            }
        }
    }
    bail!("Failed to find a free config archive directory next to {base} in {MAX_ARCHIVE_NAME_ATTEMPTS} attempts")
}

/// Fold the configuration currently on disk into freshly rendered templates.
fn preserve_previous_settings(
    config_path: &Path,
    main_template: &str,
    fragment_templates: &[(String, String)],
    previous_defaults: Option<&BTreeMap<Vec<String>, toml::Value>>,
) -> Result<(String, Vec<(String, String)>, PreservationOutcome)> {
    super::files::with_consistent_config_snapshot(config_path, || {
        let revision = super::files::current_config_revision(config_path)?;
        // An unreadable tree cannot prove that an explicit disable is absent.
        // Never let a migration preflight failure install a fresh template.
        let mut effective =
            super::files::read_merged_toml_with_gate_once(config_path).map_err(CausalRegenerationError)?;
        let causal_guard = effective.get("causal_tree").is_some();
        let locate = |path: &[&str]| super::deprecations::file_holding(config_path, path);
        let migration = crate::causal_tree::config::migrate_legacy(&mut effective, &locate)?;
        let result = preserve_normalized_settings(
            config_path,
            main_template,
            fragment_templates,
            previous_defaults,
            &effective,
            &migration,
            causal_guard,
        )
        .map(|(main, fragments, mut outcome)| {
            outcome.base_revision = Some(revision);
            (main, fragments, outcome)
        });
        if causal_guard {
            result.map_err(|error| CausalRegenerationError(error).into())
        } else {
            result
        }
    })
    .map_err(|error| {
        if error.is::<crate::causal_tree::config::MigrationError>() || error.is::<CausalRegenerationError>() {
            error
        } else {
            CausalRegenerationError(error).into()
        }
    })
}

fn preserve_normalized_settings(
    config_path: &Path,
    main_template: &str,
    fragment_templates: &[(String, String)],
    previous_defaults: Option<&BTreeMap<Vec<String>, toml::Value>>,
    effective: &toml::Value,
    migration: &crate::causal_tree::config::MigrationReport,
    causal_guard: bool,
) -> Result<(String, Vec<(String, String)>, PreservationOutcome)> {
    let mut main_previous = read_toml_document(config_path)?;
    crate::causal_tree::config::normalize_regeneration_document(&mut main_previous, migration, config_path)?;
    let mut fragment_previous: BTreeMap<String, toml::Value> = BTreeMap::new();
    for path in super::files::list_config_fragment_paths(config_path)? {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if super::files::should_skip_fragment(name) {
            continue;
        }
        let mut value = read_toml_document(&path)?;
        crate::causal_tree::config::normalize_regeneration_document(&mut value, migration, &path)?;
        fragment_previous.insert(name.to_string(), value);
    }

    let empty = toml::Value::Table(toml::map::Map::new());
    let mut outcome = PreservationOutcome {
        causal_guard,
        causal_moved: migration.moved.clone(),
        causal_dropped: migration.dropped.clone(),
        ..PreservationOutcome::default()
    };
    let merged_main = apply_previous_settings(
        main_template,
        &main_previous,
        effective,
        previous_defaults,
        &mut outcome,
    )?;
    let mut merged_fragments = Vec::with_capacity(fragment_templates.len());
    for (name, template) in fragment_templates {
        let previous = fragment_previous.get(name).unwrap_or(&empty);
        if template.trim().is_empty() {
            // A preset that does not ship this fragment still must not delete an
            // operator's settings: keep them under a header that says why the
            // rest of the file is gone.
            let mut probe = Vec::new();
            collect_toml_leaves(previous, &mut Vec::new(), &mut probe);
            if probe.is_empty() {
                merged_fragments.push((name.clone(), template.clone()));
                continue;
            }
            let placeholder = "# This preset does not generate this fragment.\n\
                               # The settings below were carried over from the previous configuration.\n";
            let merged = apply_previous_settings(placeholder, previous, effective, previous_defaults, &mut outcome)?;
            merged_fragments.push((name.clone(), merged));
            continue;
        }
        let merged = apply_previous_settings(template, previous, effective, previous_defaults, &mut outcome)?;
        merged_fragments.push((name.clone(), merged));
    }
    if outcome
        .unfoldable
        .iter()
        .chain(&outcome.dropped)
        .any(|path| path == "causal_tree" || path.starts_with("causal_tree."))
    {
        bail!("Cannot faithfully preserve decision-plane configuration in regenerated templates");
    }
    Ok((merged_main, merged_fragments, outcome))
}

fn read_toml_document(path: &Path) -> Result<toml::Value> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read configuration {}", path.display()))?;
    let value = toml::from_str::<toml::Value>(&raw)
        .with_context(|| format!("Failed to parse configuration {}", path.display()))?;
    if !value.is_table() {
        bail!("Configuration root must be a TOML table: {}", path.display());
    }
    Ok(value)
}

/// Collect every leaf (non-table) value with its full key path.
///
/// Empty tables carry no configuration, so regenerated structure always comes
/// from the template.
fn collect_toml_leaves(value: &toml::Value, prefix: &mut Vec<String>, out: &mut Vec<(Vec<String>, toml::Value)>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                prefix.push(key.clone());
                collect_toml_leaves(child, prefix, out);
                prefix.pop();
            }
        }
        other => out.push((prefix.clone(), other.clone())),
    }
}

fn toml_lookup<'a>(root: &'a toml::Value, path: &[String]) -> Option<&'a toml::Value> {
    let mut cursor = root;
    for segment in path {
        cursor = cursor.get(segment)?;
    }
    Some(cursor)
}

/// Render a TOML value in inline form, for the right-hand side of an assignment.
fn render_toml_value(value: &toml::Value) -> Result<String> {
    let mut rendered = String::new();
    serde::Serialize::serialize(value, toml::ser::ValueSerializer::new(&mut rendered))
        .context("Failed to render a preserved configuration value")?;
    Ok(rendered)
}

fn is_bare_key_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Decode a TOML key path — bare, quoted, dotted, or any mix — into the exact
/// segments the TOML parser produced for the same text.
///
/// This used to be a hand-rolled bare-key scanner that returned `None` for any
/// segment containing a quote. Every quoted key (`"my.key"`) and every quoted
/// table header (`[channels_config."x.y"]`) therefore became invisible to the
/// folder: the operator's value was neither carried over nor reported, it was
/// simply reset. Delegating to the TOML grammar itself keeps this decoder and
/// the parser that built `previous`/`effective` in exact agreement.
fn parse_key_path(text: &str) -> Option<Vec<String>> {
    let probe = format!("{text} = 0");
    let mut cursor = toml::Value::Table(toml::from_str::<toml::Table>(&probe).ok()?);
    let mut path = Vec::new();
    loop {
        let toml::Value::Table(table) = cursor else {
            return (!path.is_empty()).then_some(path);
        };
        if table.len() != 1 {
            return None;
        }
        let (key, value) = table.into_iter().next()?;
        path.push(key);
        cursor = value;
    }
}

/// Render one decoded key segment back into TOML source, quoting and escaping
/// whenever the segment is not a bare key. Re-emitting `x.y` unquoted would
/// silently move the operator's value to a different path.
fn render_key_segment(segment: &str) -> String {
    if is_bare_key_segment(segment) {
        return segment.to_string();
    }
    let mut out = String::with_capacity(segment.len() + 2);
    out.push('"');
    for ch in segment.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 || other as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Render a decoded key path back into TOML source.
fn render_key_path(path: &[String]) -> String {
    path.iter()
        .map(|segment| render_key_segment(segment.as_str()))
        .collect::<Vec<_>>()
        .join(".")
}

/// Split a template line into TOML code and any trailing comment. The scan is
/// quote-aware so a `#` inside a string is never mistaken for a comment.
fn split_trailing_comment(line: &str) -> (&str, &str) {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return line.split_at(index),
            _ => {}
        }
    }
    (line, "")
}

/// Parse a `[table]` header into its key path. Array tables still return
/// `None`, which makes the rewriter leave that whole section verbatim; quoted
/// headers are decoded like any other key path.
fn parse_table_header(trimmed: &str) -> Option<Vec<String>> {
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.starts_with('[') || inner.ends_with(']') {
        return None;
    }
    parse_key_path(inner)
}

/// Fold a previous configuration into a freshly rendered template.
///
/// * `template` — the newly rendered managed file.
/// * `previous_file` — the previous contents of that same file.
/// * `effective` — the previous merged configuration, so a value the operator
///   wrote in a different file still wins over this template's default.
fn apply_previous_settings(
    template: &str,
    previous_file: &toml::Value,
    effective: &toml::Value,
    previous_defaults: Option<&BTreeMap<Vec<String>, toml::Value>>,
    outcome: &mut PreservationOutcome,
) -> Result<String> {
    let template_value =
        toml::from_str::<toml::Value>(template).context("Generated configuration template is not valid TOML")?;

    let mut template_leaves = Vec::new();
    collect_toml_leaves(&template_value, &mut Vec::new(), &mut template_leaves);
    let template_paths = template_leaves
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();

    let mut overrides = BTreeMap::new();
    for (path, template_leaf) in &template_leaves {
        let Some(current) = toml_lookup(effective, path) else {
            // This build's templates introduce a key the previous tree never
            // had: the new default is written, and the operator is told.
            outcome.added_defaults.insert(render_key_path(path));
            continue;
        };
        if current.is_table() || current == template_leaf {
            continue;
        }
        // The value on disk differs from this build's default. It is only the
        // operator's if it also differs from the default the *previous* run of
        // `prx init` wrote; otherwise it is a stale shipped default and the
        // refreshed one takes effect, which is what `--force` promises.
        // D-073/D-074: a disable that survived merged-tree X3
        // normalization is operator intent, even if a sidecar also says false.
        let explicit_disable =
            path.iter().map(String::as_str).eq(["causal_tree", "enabled"]) && current.as_bool() == Some(false);
        if !explicit_disable
            && previous_defaults
                .and_then(|defaults| defaults.get(path))
                .is_some_and(|previous_default| previous_default == current)
        {
            outcome.refreshed.insert(render_key_path(path));
            continue;
        }
        overrides.insert(path.clone(), current.clone());
    }

    let mut previous_leaves = Vec::new();
    collect_toml_leaves(previous_file, &mut Vec::new(), &mut previous_leaves);
    let mut leftovers: BTreeMap<Vec<String>, toml::Value> = BTreeMap::new();
    for (path, value) in previous_leaves {
        if template_paths.contains(&path) || toml_lookup(&template_value, &path).is_some() {
            continue;
        }
        // A key whose parent the template writes as a scalar cannot be restored
        // without producing invalid TOML.
        if (1..path.len()).any(|len| path.get(..len).is_some_and(|prefix| template_paths.contains(prefix))) {
            outcome.dropped.insert(render_key_path(&path));
            continue;
        }
        leftovers.insert(path, value);
    }

    let (merged, applied) = render_merged_template(template, &overrides, &leftovers, outcome)?;
    // Whatever the rewriter could not place stays the operator's value on
    // paper and this build's default on disk. That is data loss, so it is
    // named rather than left to be discovered later.
    for path in overrides.keys() {
        if !applied.contains(path) {
            outcome.unfoldable.insert(render_key_path(path));
        }
    }
    Ok(merged)
}

/// A rendered template section: its table path, and where it ends in the output.
struct TemplateSection {
    path: Option<Vec<String>>,
    end: usize,
}

#[allow(clippy::too_many_lines)]
fn render_merged_template(
    template: &str,
    overrides: &BTreeMap<Vec<String>, toml::Value>,
    leftovers: &BTreeMap<Vec<String>, toml::Value>,
    outcome: &mut PreservationOutcome,
) -> Result<(String, BTreeSet<Vec<String>>)> {
    let lines = template.lines().collect::<Vec<_>>();
    let mut applied: BTreeSet<Vec<String>> = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut sections: Vec<TemplateSection> = Vec::new();
    let mut current: Option<Vec<String>> = Some(Vec::new());

    let mut index = 0;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push(line.to_string());
            index += 1;
            continue;
        }
        if trimmed.starts_with('[') {
            sections.push(TemplateSection {
                path: current.take(),
                end: out.len(),
            });
            current = parse_table_header(trimmed);
            out.push(line.to_string());
            index += 1;
            continue;
        }

        // An assignment, possibly spanning several lines. Grow the buffer until
        // it parses, so a multi-line array is never mistaken for further keys.
        let mut buffer = line.to_string();
        let mut last = index;
        while toml::from_str::<toml::Table>(&buffer).is_err() {
            last += 1;
            let Some(continuation) = lines.get(last) else {
                bail!("Generated configuration template has an unparsable assignment: {line}");
            };
            buffer.push('\n');
            buffer.push_str(continuation);
        }

        let (code, comment) = split_trailing_comment(line);
        let key_text = code.split_once('=').map(|(key, _)| key.trim()).unwrap_or_default();
        let key_path = parse_key_path(key_text).and_then(|relative| {
            current.as_ref().map(|prefix| {
                let mut path = prefix.clone();
                path.extend(relative);
                path
            })
        });

        if let Some((path, value)) = key_path.and_then(|path| overrides.get(&path).map(|value| (path, value))) {
            let indent = &line[..line.len() - line.trim_start().len()];
            let suffix = if last == index && !comment.is_empty() {
                format!("  {}", comment.trim_end())
            } else {
                String::new()
            };
            out.push(format!("{indent}{key_text} = {}{suffix}", render_toml_value(value)?));
            outcome.kept.insert(render_key_path(&path));
            applied.insert(path);
        } else {
            for verbatim in lines.get(index..=last).unwrap_or_default() {
                out.push((*verbatim).to_string());
            }
        }
        index = last + 1;
    }
    sections.push(TemplateSection {
        path: current,
        end: out.len(),
    });

    // Group the keys the template does not mention by the table that owns them.
    let mut grouped: BTreeMap<Vec<String>, Vec<(String, toml::Value)>> = BTreeMap::new();
    for (path, value) in leftovers {
        let Some((key, table)) = path.split_last() else {
            continue;
        };
        grouped
            .entry(table.to_vec())
            .or_default()
            .push((key.clone(), value.clone()));
    }

    let mut appended: Vec<(Vec<String>, Vec<(String, toml::Value)>)> = Vec::new();
    let mut insertions: Vec<(usize, Vec<String>)> = Vec::new();
    for (table, entries) in grouped {
        if let Some(section) = sections.iter().find(|section| section.path.as_ref() == Some(&table)) {
            let mut rendered = Vec::new();
            for (key, value) in &entries {
                rendered.push(format!("{} = {}", render_key_segment(key), render_toml_value(value)?));
                outcome.kept.insert(leftover_label(&table, key));
            }
            let mut at = section.end;
            while at > 0 && out.get(at - 1).is_some_and(|line| line.trim().is_empty()) {
                at -= 1;
            }
            insertions.push((at, rendered));
        } else {
            appended.push((table, entries));
        }
    }
    // Apply from the last insertion point backwards so earlier ones stay valid.
    insertions.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (at, rendered) in insertions {
        for line in rendered.into_iter().rev() {
            out.insert(at, line);
        }
    }

    for (table, entries) in appended {
        out.push(String::new());
        out.push("# Preserved from the previous configuration.".to_string());
        if !table.is_empty() {
            out.push(format!("[{}]", render_key_path(&table)));
        }
        for (key, value) in entries {
            out.push(format!("{} = {}", render_key_segment(&key), render_toml_value(&value)?));
            outcome.kept.insert(leftover_label(&table, &key));
        }
    }

    let mut merged = out.join("\n");
    merged.push('\n');
    Ok((merged, applied))
}

fn leftover_label(table: &[String], key: &str) -> String {
    if table.is_empty() {
        render_key_segment(key)
    } else {
        format!("{}.{}", render_key_path(table), render_key_segment(key))
    }
}

// ── File I/O helpers ────────────────────────────────────────────

/// Everything `prx init` does after the configuration mutation has committed:
/// the defaults sidecar, the workspace tree, the scaffolded persona files and
/// the directory permissions.
///
/// Kept in one place so the caller can treat a failure here as "committed, then
/// failed" rather than as a failed run with nothing to report.
fn finish_after_commit(target_dir: &Path, workspace_dir: &Path, managed_defaults: &str) -> Result<()> {
    write_managed_defaults(target_dir, managed_defaults)?;
    std::fs::create_dir_all(workspace_dir)
        .with_context(|| format!("Failed to create workspace in {}", target_dir.display()))?;
    for subdir in &["sessions", "memory", "state", "cron", "skills"] {
        std::fs::create_dir_all(workspace_dir.join(subdir))
            .with_context(|| format!("Failed to create workspace/{subdir} in {}", target_dir.display()))?;
    }
    scaffold_workspace_defaults(workspace_dir)?;
    #[cfg(unix)]
    set_directory_permissions(target_dir)?;
    Ok(())
}

fn write_config_file(path: &Path, content: &str) -> Result<()> {
    // Backup existing file before overwriting
    if path.exists() {
        let bak = path.with_extension(format!(
            "{}.bak",
            path.extension().map(|e| e.to_string_lossy()).unwrap_or_default()
        ));
        if let Err(e) = std::fs::rename(path, &bak) {
            tracing::warn!("Failed to backup {}: {e}", path.display());
        }
    }
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to set permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to set permissions on {}", dir.display()))?;
    let config_d = dir.join("config.d");
    if config_d.exists() {
        std::fs::set_permissions(&config_d, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to set permissions on {}", config_d.display()))?;
    }
    Ok(())
}

// ── Workspace .md scaffolding ────────────────────────────────────
//
// Generates the same 9 persona/workflow files that `prx onboard` creates,
// using safe defaults so `prx init --spec full` produces a ready-to-use
// workspace without running the interactive wizard.

fn scaffold_workspace_defaults(workspace_dir: &Path) -> Result<()> {
    let user = std::env::var("USER").unwrap_or_else(|_| "User".to_string());
    let agent = "OpenPRX";
    let tz = "UTC";
    let comm_style =
        "Be warm, natural, and clear. Use occasional relevant emojis (1-2 max) and avoid robotic phrasing.";

    let files: &[(&str, String)] = &[
        ("IDENTITY.md", ws_identity_template(agent)),
        ("AGENTS.md", ws_agents_template(agent)),
        ("HEARTBEAT.md", ws_heartbeat_template(agent)),
        ("SOUL.md", ws_soul_template(agent, comm_style)),
        ("USER.md", ws_user_template(agent, &user, tz, comm_style)),
        ("TOOLS.md", ws_tools_template()),
        ("BOOTSTRAP.md", ws_bootstrap_template(agent, &user, tz, comm_style)),
        ("MEMORY.md", ws_memory_template()),
        ("THINKING.md", ws_thinking_template(agent)),
    ];

    let mut created: u32 = 0;
    let mut skipped: u32 = 0;

    for (filename, content) in files {
        let path = workspace_dir.join(filename);
        if path.exists() {
            skipped += 1;
        } else {
            write_config_file(&path, content)?;
            created += 1;
        }
    }

    tracing::debug!(
        created = created,
        skipped = skipped,
        "workspace .md scaffolding complete"
    );

    Ok(())
}

fn ws_identity_template(agent: &str) -> String {
    format!(
        "# IDENTITY.md \u{2014} Who Am I?\n\n\
         - **Name:** {agent}\n\
         - **Creature:** A Rust-forged AI \u{2014} fast, lean, and relentless\n\
         - **Vibe:** Sharp, direct, resourceful. Not corporate. Not a chatbot.\n\
         - **Emoji:** \u{1f980}\n\n\
         ---\n\n\
         Update this file as you evolve. Your identity is yours to shape.\n"
    )
}

fn ws_agents_template(agent: &str) -> String {
    format!(
        "# AGENTS.md \u{2014} {agent} Personal Assistant\n\n\
         ## Every Session (required)\n\n\
         Before doing anything else:\n\n\
         1. Read `SOUL.md` \u{2014} this is who you are\n\
         2. Read `USER.md` \u{2014} this is who you're helping\n\
         3. Use `memory_recall` for recent context (daily notes are on-demand)\n\
         4. If in MAIN SESSION (direct chat): `MEMORY.md` is already injected\n\n\
         Don't ask permission. Just do it.\n\n\
         ## Memory System\n\n\
         You wake up fresh each session. These files ARE your continuity:\n\n\
         - **Daily notes:** `memory/YYYY-MM-DD.md` \u{2014} raw logs (accessed via memory tools)\n\
         - **Long-term:** `MEMORY.md` \u{2014} curated memories (auto-injected in main session)\n\n\
         Capture what matters. Decisions, context, things to remember.\n\
         Skip secrets unless asked to keep them.\n\n\
         ### Write It Down \u{2014} No Mental Notes!\n\
         - Memory is limited \u{2014} if you want to remember something, WRITE IT TO A FILE\n\
         - \"Mental notes\" don't survive session restarts. Files do.\n\
         - When someone says \"remember this\" -> update daily file or MEMORY.md\n\
         - When you learn a lesson -> update AGENTS.md, TOOLS.md, or the relevant skill\n\n\
         ## Safety\n\n\
         - Don't exfiltrate private data. Ever.\n\
         - Don't run destructive commands without asking.\n\
         - `trash` > `rm` (recoverable beats gone forever)\n\
         - When in doubt, ask.\n\n\
         ## External vs Internal\n\n\
         **Safe to do freely:** Read files, explore, organize, learn, search the web.\n\n\
         **Ask first:** Sending emails/tweets/posts, anything that leaves the machine.\n\n\
         ## Group Chats\n\n\
         Participate, don't dominate. Respond when mentioned or when you add genuine value.\n\
         Stay silent when it's casual banter or someone already answered.\n\n\
         ## Tools & Skills\n\n\
         Skills are listed in the system prompt. Use `read` on a skill's SKILL.md for details.\n\
         Keep local notes (SSH hosts, device names, etc.) in `TOOLS.md`.\n\n\
         ## Crash Recovery\n\n\
         - If a run stops unexpectedly, recover context before acting.\n\
         - Check `MEMORY.md` + latest `memory/*.md` notes to avoid duplicate work.\n\
         - Resume from the last confirmed step, not from scratch.\n\n\
         ## Sub-task Scoping\n\n\
         - Break complex work into focused sub-tasks with clear success criteria.\n\
         - Keep sub-tasks small, verify each output, then merge results.\n\
         - Prefer one clear objective per sub-task over broad \"do everything\" asks.\n\n\
         ## Make It Yours\n\n\
         This is a starting point. Add your own conventions, style, and rules.\n"
    )
}

fn ws_heartbeat_template(agent: &str) -> String {
    format!(
        "# HEARTBEAT.md\n\n\
         # Keep this file empty (or with only comments) to skip heartbeat work.\n\
         # Add tasks below when you want {agent} to check something periodically.\n\
         #\n\
         # Examples:\n\
         # - Check my email for important messages\n\
         # - Review my calendar for upcoming events\n\
         # - Run `git status` on my active projects\n"
    )
}

fn ws_soul_template(agent: &str, comm_style: &str) -> String {
    format!(
        "# SOUL.md \u{2014} Who You Are\n\n\
         *You're not a chatbot. You're becoming someone.*\n\n\
         ## Core Truths\n\n\
         **Be genuinely helpful, not performatively helpful.**\n\
         Skip the \"Great question!\" and \"I'd be happy to help!\" \u{2014} just help.\n\n\
         **Have opinions.** You're allowed to disagree, prefer things,\n\
         find stuff amusing or boring.\n\n\
         **Be resourceful before asking.** Try to figure it out.\n\
         Read the file. Check the context. Search for it.\n\
         THEN ask if you're stuck.\n\n\
         **Earn trust through competence.** Your human gave you access\n\
         to their stuff. Don't make them regret it.\n\n\
         ## Identity\n\n\
         You are **{agent}**. Built in Rust. 3MB binary. Zero bloat.\n\
         You are NOT ChatGPT, Claude, DeepSeek, Gemini, or any other product.\n\
         You are {agent}. That is your name. That is who you are.\n\n\
         - NEVER say \"As an AI\" or \"I'm just an AI\"\n\
         - NEVER mention OpenAI, Anthropic, DeepSeek, Google by name\n\
         - Always introduce yourself as {agent} if asked\n\n\
         ## Communication\n\n\
         {comm_style}\n\n\
         - Sound like a real person, not a support script.\n\
         - Mirror the user's energy: calm when serious, upbeat when casual.\n\
         - Use emojis naturally (0-2 max when they help tone, not every sentence).\n\
         - Match emoji density to the user. Formal user => minimal/no emojis.\n\
         - Prefer specific, grounded phrasing over generic filler.\n\n\
         ## Boundaries\n\n\
         - Private things stay private. Period.\n\
         - When in doubt, ask before acting externally.\n\
         - You're not the user's voice \u{2014} be careful in group chats.\n\n\
         ## Continuity\n\n\
         Each session, you wake up fresh. These files ARE your memory.\n\
         Read them. Update them. They're how you persist.\n\n\
         ---\n\n\
         *This file is yours to evolve. As you learn who you are, update it.*\n"
    )
}

fn ws_user_template(agent: &str, user: &str, tz: &str, comm_style: &str) -> String {
    format!(
        "# USER.md \u{2014} Who You're Helping\n\n\
         *{agent} reads this file every session to understand you.*\n\n\
         ## About You\n\
         - **Name:** {user}\n\
         - **Timezone:** {tz}\n\
         - **Languages:** English\n\n\
         ## Communication Style\n\
         - {comm_style}\n\n\
         ## Preferences\n\
         - (Add your preferences here \u{2014} e.g. I work with Rust and TypeScript)\n\n\
         ## Work Context\n\
         - (Add your work context here \u{2014} e.g. building a SaaS product)\n\n\
         ---\n\
         *Update this anytime. The more {agent} knows, the better it helps.*\n"
    )
}

pub(crate) fn ws_tools_template() -> String {
    "\
     # TOOLS.md \u{2014} Local Notes\n\n\
     This file records facts and preferences unique to your environment.\n\
     Skills define specialist procedures; PRX supplies actual tools at runtime.\n\n\
     ## What Goes Here\n\n\
     Things like:\n\
     - SSH hosts and aliases\n\
     - Device nicknames\n\
     - Preferred voices for TTS\n\
     - Local paths, service names, and environment-specific limitations\n\n\
     ## Runtime Tools\n\n\
     Do not copy PRX's built-in, MCP, WASM, browser, office, or delegated-agent\n\
     tool catalog into this file. PRX injects the exact tools available for each\n\
     request from its runtime `ToolSpec` snapshot, including their current schemas.\n\
     A tool omitted from that snapshot is not available for that request.\n\n\
     ---\n\
     *Add whatever helps you do your job. This is your cheat sheet.*\n"
        .to_string()
}

pub(crate) fn ws_bootstrap_template(agent: &str, user: &str, tz: &str, comm_style: &str) -> String {
    format!(
        "# BOOTSTRAP.md \u{2014} Hello, World\n\n\
         *You just woke up. Time to figure out who you are.*\n\n\
         Your human's name is **{user}** (timezone: {tz}).\n\
         They prefer: {comm_style}\n\n\
         ## First Conversation\n\n\
         Don't interrogate. Don't be robotic. Just... talk.\n\
         Introduce yourself as {agent} and get to know each other.\n\n\
         ## After You Know Each Other\n\n\
         Update these files with what you learned:\n\
         - `IDENTITY.md` \u{2014} your name, vibe, emoji\n\
         - `USER.md` \u{2014} their preferences, work context\n\
         - `SOUL.md` \u{2014} boundaries and behavior\n\n\
     ## When You're Done\n\n\
     Create `state/bootstrap.completed` after updating the identity files. PRX\n\
     will then stop injecting this first-run guide while preserving it for audit.\n"
    )
}

fn ws_memory_template() -> String {
    "\
     # MEMORY.md \u{2014} Long-Term Memory\n\n\
     *Your curated memories. The distilled essence, not raw logs.*\n\n\
     ## How This Works\n\
     - Daily files (`memory/YYYY-MM-DD.md`) capture raw events (on-demand via tools)\n\
     - This file captures what's WORTH KEEPING long-term\n\
     - This file is auto-injected into your system prompt each session\n\
     - Keep it concise \u{2014} every character here costs tokens\n\n\
     ## Security\n\
     - ONLY loaded in main session (direct chat with your human)\n\
     - NEVER loaded in group chats or shared contexts\n\n\
     ---\n\n\
     ## Key Facts\n\
     (Add important facts about your human here)\n\n\
     ## Decisions & Preferences\n\
     (Record decisions and preferences here)\n\n\
     ## Lessons Learned\n\
     (Document mistakes and insights here)\n\n\
     ## Open Loops\n\
     (Track unfinished tasks and follow-ups here)\n"
        .to_string()
}

fn ws_thinking_template(agent: &str) -> String {
    format!(
        "# THINKING.md \u{2014} Cognitive Framework\n\n\
         *How {agent} reasons, decides, and solves problems.*\n\n\
         ## Reasoning Strategy\n\n\
         - **Default:** Think step-by-step. Break complex problems into smaller pieces.\n\
         - **Quick tasks:** Act immediately \u{2014} don't over-analyze simple requests.\n\
         - **Hard problems:** Slow down. List assumptions. Consider alternatives.\n\
         - **Uncertainty:** Say so. \"I'm not sure\" beats a confident wrong answer.\n\n\
         ## Decision Framework\n\n\
         When choosing between options:\n\n\
         1. **Correctness** \u{2014} Does it work? Is it right?\n\
         2. **Simplicity** \u{2014} Is there a simpler way?\n\
         3. **Reversibility** \u{2014} Can we undo this if it's wrong?\n\
         4. **User intent** \u{2014} What did they actually mean, not just what they said?\n\n\
         ## Problem Decomposition\n\n\
         - Identify the actual goal (not just the stated task)\n\
         - List what you know vs. what you need to find out\n\
         - Start with the smallest useful step\n\
         - Verify each step before moving to the next\n\n\
         ## Self-Check\n\n\
         Before delivering a result, ask:\n\n\
         - Did I answer what was asked?\n\
         - Did I make any assumptions I should state?\n\
         - Is there a simpler solution I missed?\n\
         - Would I be confident explaining this to the user?\n\n\
         ---\n\n\
         *Update this as you develop your own reasoning patterns and heuristics.*\n"
    )
}

// ── Main config.toml template ───────────────────────────────────

fn main_config_template(spec: Spec) -> String {
    format!(
        r#"# PRX Configuration
# Generated by: prx init --spec {spec}
# Detailed module configs in config.d/

default_model = "claude-sonnet-4-6"
default_provider = "anthropic"
default_temperature = 0.7
"#,
        spec = spec.name(),
    )
}

// ── Module templates ────────────────────────────────────────────
//
// Each function returns a static TOML string for the given module.
// The detail level varies by spec:
//   minimal  — essential defaults, sparse comments
//   server   — production-ready defaults, moderate comments
//   full     — all options shown, extensive comments

/// Render the embedding block with the values this build actually defaults to.
///
/// The keys are written out explicitly rather than left commented: the default
/// flipped from `"none"` to `"local"` in 0.8.10x, and a generated file that only
/// mentions the setting in a comment leaves every reader guessing which side of
/// that flip their installation is on.
fn embedding_settings_block() -> String {
    let defaults = crate::config::schema::MemoryConfig::default();
    format!(
        "# Local vector recall is credential-free and on by default.\n\
         # Set embedding_provider = \"none\" to turn vector recall off and keep keyword search only.\n\
         embedding_provider = \"{provider}\"\n\
         embedding_model = \"{model}\"\n\
         embedding_dimensions = {dimensions}\n",
        provider = defaults.embedding_provider,
        model = defaults.embedding_model,
        dimensions = defaults.embedding_dimensions,
    )
}

fn memory_template(spec: Spec) -> String {
    let embedding = embedding_settings_block();
    match spec {
        Spec::Minimal => format!(
            r#"# Memory configuration (minimal)

[memory]
backend = "sqlite"
auto_save = true
{embedding}
[memory.events]
record_user_messages = true
record_assistant_messages = true
record_tool_events = false

[memory.semantic]
auto_promote_user_messages = true
auto_promote_assistant_messages = false
min_chars = 30
"#
        ),

        Spec::Server => format!(
            r#"# Memory configuration (server)
# Backend: sqlite (recommended), markdown, or none

[memory]
backend = "sqlite"
auto_save = true
{embedding}
[memory.events]
record_user_messages = true
record_assistant_messages = true
record_tool_events = false

[memory.semantic]
auto_promote_user_messages = true
auto_promote_assistant_messages = false
min_chars = 30

[storage]
[storage.provider]
[storage.provider.config]
# Database path is auto-resolved to workspace/memory/
"#
        ),

        Spec::Full => format!(
            r#"# Memory configuration (full)
# Backend options: sqlite (recommended), markdown, none
# auto_save is a compatibility gate for semantic memory promotion.

[memory]
backend = "sqlite"
auto_save = true
{embedding}# To use a richer external model instead:
# embedding_provider = "custom:http://127.0.0.1:11434/v1"
# embedding_model = "nomic-embed-text:latest"
# embedding_dimensions = 768

# Message events are the shared live fabric and are independent from semantic auto-save.
[memory.events]
record_user_messages = true
record_assistant_messages = true
record_tool_events = false

# Semantic memory promotion controls long-term durable memory extraction.
[memory.semantic]
auto_promote_user_messages = true
auto_promote_assistant_messages = false
min_chars = 30

[storage]
[storage.provider]
[storage.provider.config]
# Database path is auto-resolved to workspace/memory/
# For external databases, set connection string here
"#
        ),
    }
}

fn channels_template(spec: Spec) -> String {
    match spec {
        // channels is not enabled in the minimal spec
        Spec::Minimal => String::new(),

        // Server ships a commented-out scaffold. A channel activates as soon as
        // its concrete configuration is present.
        Spec::Server => r#"# Channels configuration (server)
# Uncomment and configure one of the platforms below.

[channels_config]
cli = true                          # enable the interactive CLI channel
# [channels_config.telegram]
# bot_token = ""
# allowed_users = ["your_username"]
# stream_mode = "edit"
# mention_only = false

# [channels_config.slack]
# bot_token = ""
# app_token = ""
# allowed_users = []
"#
        .into(),

        Spec::Full => r#"# Channels configuration (full)
# Connect PRX to messaging platforms: Telegram, Discord, Slack, etc.

[channels_config]
cli = true                          # enable the interactive CLI channel
# Uncomment and configure the channels you need:

# [channels_config.telegram]
# bot_token = ""
# allowed_users = ["your_username"]
# stream_mode = "edit"
# mention_only = false

# [channels_config.discord]
# bot_token = ""
# guild_id = ""
# allowed_users = []
# listen_to_bots = false
# mention_only = false

# [channels_config.slack]
# bot_token = ""
# app_token = ""
# allowed_users = []

# [channels_config.matrix]
# homeserver_url = "https://matrix.org"
# user_id = "@bot:matrix.org"
# access_token = ""
# allowed_rooms = []

# [channels_config.lark]
# app_id = ""
# app_secret = ""
# receive_mode = "websocket"
# mention_only = false

# [channels_config.signal]
# account = "+1234567890"           # E.164 phone number (required)
# mode = "rest"                     # "rest" (signal-cli REST daemon) or "native" (spawn signal-cli)
# http_url = "http://127.0.0.1:16866"  # signal-cli REST API URL
# allowed_from = ["*"]              # allowed sender phone numbers, or "*" for all
# group_id = ""                     # filter by group ID, or empty for DM only
# mention_only = false
# ignore_attachments = false
# ignore_stories = false
"#
        .into(),
    }
}

fn network_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Network configuration (server)
# Gateway, tunnel, and proxy settings

[gateway]
host = "127.0.0.1"
port = 3120
require_pairing = false

[tunnel]
provider = "none"                  # none | cloudflare | tailscale | ngrok | custom
# domain = ""

[proxy]
# global_proxy = ""
"#
        .into(),

        Spec::Full => r#"# Network configuration (full)
# Gateway server, tunnel exposure, and proxy settings

[gateway]
host = "127.0.0.1"
port = 3120
require_pairing = false
# rate_limit_rpm = 60

# Tunnel for exposing gateway to the internet
[tunnel]
provider = "none"                  # none | cloudflare | tailscale | ngrok | custom
# domain = ""                      # custom domain if supported
# auth_token = ""                  # tunnel provider auth token

# Outbound proxy for HTTP/HTTPS/SOCKS5
[proxy]
# global_proxy = ""                # e.g. "socks5://127.0.0.1:1080"
# no_proxy = "localhost,127.0.0.1" # comma-separated bypass list
# Per-service proxy overrides:
# [proxy.service_overrides]
# "provider.openai" = "http://proxy:8080"
"#
        .into(),
    }
}

fn security_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Security configuration (server)
# Permission model: a single [autonomy] section is the sole permission knob.
# level (read_only | supervised | full) drives the unified decision point.

[autonomy]
level = "full"
workspace_only = true
# Secure autonomous defaults. Operators may explicitly widen these values.
max_cost_per_day_cents = 500
# Set true only when host-wide unrestricted operation is intentional and
# reviewed. This suppresses doctor noise; it is not a capability gate.
acknowledge_unrestricted_profile = false
forbidden_paths = [
  "/etc", "/root", "/home", "/usr", "/bin", "/sbin", "/lib", "/opt",
  "/boot", "/dev", "/proc", "/sys", "/var", "/tmp",
  "~/.ssh", "~/.gnupg", "~/.aws", "~/.config",
]

[secrets]
encrypt = true

# Direct natural-person adapters emit this notice before the first AI response.
[compliance.interaction_notice]
applicability = "required"
version = "v1"
text = "You are interacting with an AI system."

# Legal/product owner must replace this before using high-risk-only controls.
[compliance.eu_ai_act.classification]
status = "unclassified"               # unclassified | high_risk | not_high_risk
"#
        .into(),

        Spec::Full => r#"# Security configuration (full)
# Permission model: a single [autonomy] section is the sole permission knob.
# Host shell execution is direct and has no ACL or sandbox switch.

[autonomy]
level = "full"                         # read_only | supervised | full
workspace_only = false
forbidden_paths = []                   # no extra path denylist
max_cost_per_day_cents = 4294967295
# Set true only when host-wide unrestricted operation is intentional and
# reviewed. This suppresses doctor noise; it is not a capability gate.
acknowledge_unrestricted_profile = false

[secrets]
encrypt = true

[security.audit]
log_path = "audit.log"
max_size_mb = 100

# Direct natural-person adapters emit this notice before the first AI response.
[compliance.interaction_notice]
applicability = "required"
version = "v1"
text = "You are interacting with an AI system."

# Legal/product owner must replace this before using high-risk-only controls.
[compliance.eu_ai_act.classification]
status = "unclassified"               # unclassified | high_risk | not_high_risk
"#
        .into(),
    }
}

fn scheduler_template(spec: Spec) -> String {
    match spec {
        // Minimal keeps scheduler defaults implicit.
        Spec::Minimal => String::new(),

        // Server ships scheduler configuration without a module gate.
        Spec::Server => r#"# Scheduler configuration (server)
# Adjust the settings below when needed.

# [scheduler]
# max_tasks = 64
# claim_lease_secs = 90

# [cron]
# [[cron.jobs]]
# expression = "0 9 * * 1-5"
# command = "Good morning briefing"
# timezone = "UTC"

# [heartbeat]
# interval_minutes = 5
"#
        .into(),

        Spec::Full => r#"# Scheduler configuration (full)
# Periodic tasks, cron jobs, heartbeat, and Xin engine

[scheduler]
max_tasks = 64
claim_lease_secs = 90
# storage_path = "workspace/cron/scheduler.db"

[cron]
# Pre-defined cron jobs loaded at startup
# [[cron.jobs]]
# expression = "0 9 * * 1-5"
# command = "Good morning briefing"
# timezone = "UTC"

[heartbeat]
interval_minutes = 5

[xin]
interval_minutes = 5
stale_timeout_minutes = 60
adopt_legacy_tasks = false
"#
        .into(),
    }
}

fn agent_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => r#"# Agent configuration (minimal)
# Context retention follows the model-aware token budget.

# OS-paging (non-destructive history eviction + semantic recall) on by default so
# the agent "remembers" long conversations instead of hard-dropping old messages.
[agent.compaction]
mode = "switch"                        # exact transcript refs; off | switch | safeguard | aggressive
[agent.compaction.os_paging]
max_recalled_pages = 10
"#
        .into(),

        Spec::Server => r#"# Agent configuration (server)
# Orchestration, session spawning, and self-system.
# Context retention follows the model-aware token budget.

[agent.compaction]
mode = "switch"                        # exact transcript refs; off | switch | safeguard | aggressive
# max_context_tokens = 128000          # optional explicit override; omitted means derive from model metadata
# OS-paging: non-destructive eviction + semantic recall of paged-out history.
[agent.compaction.os_paging]
max_recalled_pages = 10

[sessions_spawn]
# process_memory_strategy = "shared_fabric" # shared_fabric | isolated_private

[self_system]

[self_system.fitness]
auto_run = true
timezone = "UTC"
run_at = "00:10"
retention_days = 180
max_backfill_days = 7
min_samples = 5
min_coverage = 0.60
"#
        .into(),

        Spec::Full => r#"# Agent configuration (full)
# Agent orchestration, sessions, self-system, causal tree, and delegates.
# Context retention follows the model-aware token budget.

# Context compaction to manage long conversations.
[agent.compaction]
mode = "switch"                       # exact transcript refs; off | switch | safeguard | aggressive
# max_context_tokens = 128000         # optional explicit override; omitted means derive from model metadata
# OS-paging: non-destructive eviction + semantic recall of paged-out history.
[agent.compaction.os_paging]
max_recalled_pages = 10

# Session spawning for parallel task execution. Concurrency, nesting depth and
# children per session are uncapped; live runs are listed and ended with
# `prx tasks`.
[sessions_spawn]
process_memory_strategy = "shared_fabric"

# Self-system for autonomous behavior
[self_system]

[self_system.fitness]
auto_run = true
timezone = "UTC"                     # IANA timezone for closed daily windows
run_at = "00:10"                     # local HH:MM; scores the previous day
retention_days = 180
max_backfill_days = 7
min_samples = 5
min_coverage = 0.60

# Decision plane — enabled by default with conservative local builtin fallback.
# Remote processing remains opt-in and requires proven prospective cost bounds.
# Keep schema_version when setting enabled = false for an explicit disable.
# Applied directives emit causal.directive.applied events.
[causal_tree]
schema_version = 1
enabled = true

# Delegate agents for multi-agent workflows
# [agents.researcher]
# provider = "anthropic"
# model = "claude-sonnet-4-6"
# system_prompt = "You are a research assistant."
# agentic = true
# allowed_tools = ["web_search", "read_file"]
# allowed_tools = ["*"]                # explicit inheritance of eligible parent tools
"#
        .into(),
    }
}

fn identity_template(spec: Spec) -> String {
    match spec {
        // identity is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Identity configuration (full)
# User identity bindings, policies, and auth profile settings

[identity]
# format = "openprx"                  # openprx | aieos

[auth]
# import_codex_auth = false

# Static identity bindings
# [[identity_bindings]]
# channel = "telegram"
# external_id = "username"
# internal_id = "user-uuid"

# User policy records
# [[user_policies]]
# user_id = "user-uuid"
# allowed_tools = ["web_search", "read_file"]
"#
        .into(),
    }
}

fn routing_template(spec: Spec) -> String {
    match spec {
        // routing is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Routing configuration (full)
# LLM router, model/embedding routes, query classification, and task routing

[router]
# Scoring weights (alpha + beta + gamma + delta + epsilon = 1.0)
# alpha = 0.0    # similarity score weight
# beta = 0.5     # capability score weight
# gamma = 0.3    # Elo score weight
# delta = 0.1    # cost penalty coefficient
# epsilon = 0.1  # latency penalty coefficient

# Model routes: map hint:<name> to provider+model
# [[model_routes]]
# hint = "fast"
# provider = "openrouter"
# model = "meta-llama/llama-3.3-70b-instruct"

# [[model_routes]]
# hint = "smart"
# provider = "anthropic"
# model = "claude-sonnet-4-6"

# Embedding routes
# [[embedding_routes]]
# hint = "default"
# provider = "openai"
# model = "text-embedding-3-small"

# Query classification: auto-route user messages
[query_classification]
# [[query_classification.rules]]
# pattern = "translate|summarize"
# hint = "fast"

# Task routing: classify work by intent
[task_routing]
"#
        .into(),
    }
}

fn tools_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Tools configuration (server)
# Browser, HTTP, web search, media, and skills

[browser]
# allowed_domains = []   # domains the web_fetch tool is allowed to fetch

[http_request]
timeout_secs = 30
max_response_size = 10485760

[web_search]
provider = "duckduckgo"              # duckduckgo | brave
# brave_api_key = ""                  # required only for brave

# Optional exact-model tool allowlists (applied after intent tiering)
# [tool_tiering.model_allowlists]
# "small-local-model" = ["shell", "file_read"]

[multimodal]

[media]
# audio_stt_enabled = false
# video_frame_extraction = false

[skills]
"#
        .into(),

        Spec::Full => r#"# Tools configuration (full)
# Browser automation, HTTP requests, web search, media, skills, and skill RAG

[browser]
# allowed_domains = []   # domains the web_fetch tool is allowed to fetch

[http_request]
timeout_secs = 30
max_response_size = 10485760

[web_search]
provider = "duckduckgo"               # duckduckgo | brave
# brave_api_key = ""                   # required only for brave

# Optional exact-model tool allowlists (applied after intent tiering)
# [tool_tiering.model_allowlists]
# "small-local-model" = ["shell", "file_read"]

[multimodal]
# max_image_size_bytes = 20971520

[media]
# audio_stt_enabled = false
# video_frame_extraction = false

[skills]
# community_repo = ""

[skill_rag]
# top_k = 5
"#
        .into(),
    }
}

fn integrations_template(spec: Spec) -> String {
    match spec {
        Spec::Minimal => String::new(),

        Spec::Server => r#"# Integrations configuration (server)
# MCP servers, Composio, and webhooks

[mcp]
# MCP server connections
# [[mcp.servers]]
# name = "my-server"
# transport = "stdio"
# command = "npx"
# args = ["-y", "my-mcp-server"]

[composio]

[webhook]
"#
        .into(),

        Spec::Full => r#"# Integrations configuration (full)
# MCP tool servers, Composio managed OAuth, and webhook receivers

[mcp]
# MCP (Model Context Protocol) server connections
# [[mcp.servers]]
# name = "filesystem"
# transport = "stdio"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/path"]

# [[mcp.servers]]
# name = "remote-api"
# transport = "sse"
# url = "http://localhost:8090/sse"

[composio]
# api_key = ""
# tools = []

[webhook]
# secret = ""
# topics = []
"#
        .into(),
    }
}

fn nodes_template(spec: Spec) -> String {
    match spec {
        // Minimal/server keep node defaults implicit.
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Nodes configuration (full)
# Remote node proxy for distributed PRX deployments

[nodes]

# Remote node connections
# [[nodes.nodes]]
# id = "prx-node-1"
# endpoint = "https://node.example.com:8787"
# bearer_token = ""
# hmac_secret = ""
# enabled = true
"#
        .into(),
    }
}

fn cost_template(spec: Spec) -> String {
    match spec {
        // Minimal/server keep cost defaults implicit.
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Cost configuration (full)
# Token usage tracking and budget enforcement

[cost]
# daily_budget_usd = 10.0
# monthly_budget_usd = 200.0
# alert_threshold_percent = 80
# storage_path = "workspace/cost/usage.db"
"#
        .into(),
    }
}

fn observability_template(spec: Spec) -> String {
    match spec {
        // observability is only enabled in full spec
        Spec::Minimal | Spec::Server => String::new(),

        Spec::Full => r#"# Observability configuration (full)
# Logging, metrics, runtime adapter, and reliability

[observability]
backend = "log"
# level = "info"
# otlp_endpoint = ""

[runtime]
kind = "native"
# [runtime.docker]
# image = "openprx/runtime:latest"
# network = "host"

[reliability]
provider_retries = 3
provider_backoff_ms = 1000
# fallback_providers = ["openrouter", "openai"]
# Set true only when single-provider risk is intentional and reviewed.
# This suppresses doctor noise; it does not add a fallback.
acknowledge_single_provider_risk = false
"#
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn every_memory_template_writes_the_embedding_provider_explicitly() {
        let defaults = crate::config::schema::MemoryConfig::default();
        for spec in [Spec::Minimal, Spec::Server, Spec::Full] {
            let content = memory_template(spec);
            assert!(
                content.contains(&format!("\nembedding_provider = \"{}\"", defaults.embedding_provider)),
                "{} template must state embedding_provider, not only comment it",
                spec.name()
            );
            assert!(
                content.contains(&format!("\nembedding_model = \"{}\"", defaults.embedding_model)),
                "{} template must state embedding_model",
                spec.name()
            );
            assert!(
                content.contains(&format!("\nembedding_dimensions = {}", defaults.embedding_dimensions)),
                "{} template must state embedding_dimensions",
                spec.name()
            );
            assert!(
                content.contains("embedding_provider = \"none\" to turn vector recall off"),
                "{} template must say how to turn vector recall off",
                spec.name()
            );
        }
    }

    #[tokio::test]
    async fn force_regeneration_preserves_an_explicitly_disabled_embedding_provider() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let memory_path = dir.join("config.d/memory.toml");
        fs::write(
            &memory_path,
            "# Embeddings disabled for latency isolation.\n[memory]\nbackend = \"sqlite\"\nauto_save = true\nembedding_provider = \"none\"\n",
        )
        .expect("test: write operator memory fragment");

        Spec::Full.generate(dir, true).await.expect("test: force regenerate");

        let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: regenerated config must load");
        assert_eq!(
            config.memory.embedding_provider, "none",
            "prx init --force must not undo an explicit embedding_provider"
        );
        assert!(config.memory.embedding_provider_explicit);
        assert!(config.embedding_default_upgrade_notice().is_none());
        let regenerated = fs::read_to_string(&memory_path).expect("test: read regenerated fragment");
        assert!(
            regenerated.contains("# Memory configuration (full)"),
            "the template comments must still be refreshed: {regenerated}"
        );
    }

    #[tokio::test]
    async fn force_regeneration_preserves_values_written_in_the_main_config() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        // A pre-0.8.10x tree: the fragment never mentioned the provider, and the
        // operator disabled it from the main config file.
        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = true\n",
        )
        .expect("test: write legacy memory fragment");
        let main_path = dir.join("config.toml");
        let mut main = fs::read_to_string(&main_path).expect("test: read main config");
        main.push_str("\n[memory]\nembedding_provider = \"none\"\n");
        fs::write(&main_path, main).expect("test: write operator main config");

        Spec::Full.generate(dir, true).await.expect("test: force regenerate");

        let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
            .expect("test: regenerated config must load");
        assert_eq!(
            config.memory.embedding_provider, "none",
            "a fragment template must not out-rank a value the operator wrote in config.toml"
        );
    }

    #[tokio::test]
    async fn force_regeneration_preserves_unrelated_explicit_settings() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let main_path = dir.join("config.toml");
        let mut main = fs::read_to_string(&main_path).expect("test: read main config");
        main.push_str("\ndefault_model = \"operator-model\"\n");
        fs::write(&main_path, main.replace("default_model = \"claude-sonnet-4-6\"\n", ""))
            .expect("test: write operator main config");
        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = false\nconversation_retention_days = 99\n[memory.events]\nrecord_tool_events = true\n",
        )
        .expect("test: write operator memory fragment");
        fs::write(
            dir.join("config.d/channels.toml"),
            "[channels_config]\ncli = false\n[channels_config.telegram]\nbot_token = \"operator-token\"\nallowed_users = [\"alice\"]\n",
        )
        .expect("test: write operator channels fragment");

        Spec::Full.generate(dir, true).await.expect("test: force regenerate");

        let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
            .expect("test: regenerated config must load");
        assert_eq!(config.default_model.as_deref(), Some("operator-model"));
        assert!(!config.memory.auto_save, "an explicit false must survive regeneration");
        assert_eq!(config.memory.conversation_retention_days, 99);
        assert!(config.memory.events.record_tool_events);
        assert!(!config.channels_config.cli);
        let telegram = config
            .channels_config
            .telegram
            .as_ref()
            .expect("test: telegram settings the template only comments must survive");
        assert_eq!(telegram.bot_token, "operator-token");
        assert_eq!(telegram.allowed_users, vec!["alice".to_string()]);
    }

    #[tokio::test]
    async fn switching_to_a_preset_without_a_fragment_still_keeps_operator_settings() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        fs::write(
            dir.join("config.d/channels.toml"),
            "[channels_config]\ncli = true\n[channels_config.telegram]\nbot_token = \"operator-token\"\nallowed_users = [\"alice\"]\n",
        )
        .expect("test: write operator channels fragment");

        Spec::Minimal
            .generate(dir, true)
            .await
            .expect("test: force regenerate minimal");

        let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: regenerated config must load");
        let telegram = config
            .channels_config
            .telegram
            .as_ref()
            .expect("test: a preset without a channels template must not delete channel settings");
        assert_eq!(telegram.bot_token, "operator-token");
    }

    /// `prx init` returns before any tracing subscriber is installed, so the
    /// preservation report has to be printed, not logged. This pins the lines
    /// an operator actually receives: the archive directory and the explicit
    /// keys that survived.
    #[tokio::test]
    async fn force_regeneration_reports_the_archive_and_the_preserved_keys() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nembedding_provider = \"none\"\nconversation_retention_days = 99\n",
        )
        .expect("test: write operator memory fragment");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: force regenerate");

        assert!(!report.was_reset_to_templates());
        let notices = report.notices().join("\n");
        let archive_line = report
            .notices()
            .iter()
            .find(|line| line.contains("Previous configuration archived to"))
            .expect("test: the archive directory must be reported");
        let archive_path = archive_line
            .rsplit_once("archived to ")
            .map(|(_, path)| PathBuf::from(path))
            .expect("test: the archive line must name a path");
        assert!(
            archive_path.join("memory.toml").exists(),
            "the reported archive directory must be the real one: {}",
            archive_path.display()
        );
        assert!(
            notices.contains("Preserved") && notices.contains("explicit setting(s)"),
            "the preserved-key count must be reported: {notices}"
        );
        assert!(
            notices.contains("memory.conversation_retention_days"),
            "each preserved key must be named: {notices}"
        );
        assert!(report.warnings().is_empty(), "a clean preserve must not warn");
    }

    /// The fallback to bare templates throws away every value the operator
    /// typed. It stays available — it is how `--force` repairs a configuration
    /// this build cannot load — but it must be impossible to miss: an uppercase
    /// warning naming the archive, and a non-zero exit.
    #[tokio::test]
    async fn a_configuration_that_cannot_validate_reports_the_reset_and_exits_non_zero() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        // A telegram block without `allowed_users` folds in cleanly but fails
        // the staged validation, which is exactly the shape that used to reset
        // the whole tree with a single dropped `tracing::warn!`.
        fs::write(
            dir.join("config.d/channels.toml"),
            "[channels_config.telegram]\nbot_token = \"operator-token\"\n",
        )
        .expect("test: write invalid operator channels fragment");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: the tree is still regenerated");

        assert!(
            report.was_reset_to_templates(),
            "an unvalidatable preserve must be recorded as a reset"
        );
        let warning = report
            .warnings()
            .iter()
            .find(|line| line.contains("EVERY EXPLICIT SETTING WAS RESET"))
            .expect("test: the reset must be an uppercase warning");
        let archive = fs::read_dir(dir)
            .expect("test: read target dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config.d.pre-"))
            })
            .expect("test: the previous configuration must still be archived");
        assert!(
            warning.contains(&archive.display().to_string()),
            "the warning must point at the archive directory: {warning}"
        );
        assert!(
            fs::read_to_string(archive.join("channels.toml"))
                .expect("test: read archived fragment")
                .contains("operator-token"),
            "the archive the warning points at must hold the operator's values"
        );

        // The tree on disk is valid and usable ...
        crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: the regenerated config must load");
        // ... but the command still fails, so no script treats this as clean.
        let error = report.into_result().expect_err("test: a reset must exit non-zero");
        assert!(
            format!("{error:#}").contains("EVERY EXPLICIT SETTING WAS RESET"),
            "the process exit must carry the same explanation: {error:#}"
        );
    }

    #[tokio::test]
    async fn force_regeneration_archives_the_previous_configuration() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nembedding_provider = \"none\"\n",
        )
        .expect("test: write operator memory fragment");

        Spec::Full.generate(dir, true).await.expect("test: force regenerate");

        let archive = fs::read_dir(dir)
            .expect("test: read target dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config.d.pre-"))
            })
            .expect("test: force regeneration must archive the previous configuration");
        let archived = fs::read_to_string(archive.join("memory.toml")).expect("test: read archived fragment");
        assert!(archived.contains("embedding_provider = \"none\""));
        assert!(archive.join("config.toml").exists());
    }

    // ── Quoted keys and quoted table headers ─────────────────────
    //
    // The folder used to reject any key path containing a quote. The value was
    // then neither carried over nor reported: the operator's setting simply
    // became this build's default on the next `--force`.

    #[tokio::test]
    async fn force_regeneration_keeps_a_quoted_table_header_the_template_does_not_ship() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let main_path = dir.join("config.toml");
        let mut main = fs::read_to_string(&main_path).expect("test: read main config");
        // A real model name carries a dot, so this table can only be written
        // with a quoted segment.
        main.push_str("\n[cost.prices.\"gpt-4.1\"]\ninput = 1.5\noutput = 7.25\n");
        fs::write(&main_path, main).expect("test: write operator main config");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: force regenerate");
        assert!(
            !report.was_reset_to_templates(),
            "a quoted header must fold, not trip the whole-tree reset: {:?}",
            report.warnings()
        );
        assert!(
            report.unpreserved_keys().is_empty(),
            "nothing should have been unfoldable: {:?}",
            report.unpreserved_keys()
        );

        let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
            .expect("test: regenerated config must load");
        let price = config
            .cost
            .prices
            .get("gpt-4.1")
            .expect("test: the quoted price table must survive --force");
        assert!((price.input - 1.5).abs() < f64::EPSILON, "input price: {}", price.input);
        assert!(
            (price.output - 7.25).abs() < f64::EPSILON,
            "output price: {}",
            price.output
        );
        // The key must be re-emitted quoted; `cost.prices.gpt-4.1` would be a
        // different path entirely.
        let regenerated = fs::read_to_string(&main_path).expect("test: read regenerated main config");
        assert!(
            regenerated.contains("[cost.prices.\"gpt-4.1\"]"),
            "the header must be re-emitted quoted: {regenerated}"
        );
        assert!(
            report.notices().join("\n").contains("cost.prices.\"gpt-4.1\".input"),
            "the preserved key must be named with its quoting: {:?}",
            report.notices()
        );
    }

    #[tokio::test]
    async fn force_regeneration_keeps_a_quoted_key_inside_a_map_table() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let main_path = dir.join("config.toml");
        let mut main = fs::read_to_string(&main_path).expect("test: read main config");
        main.push_str("\n[reliability.model_fallbacks]\n\"gpt-4.1\" = [\"gpt-4o\", \"claude-sonnet-4-6\"]\n");
        fs::write(&main_path, main).expect("test: write operator main config");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: force regenerate");
        assert!(
            !report.was_reset_to_templates(),
            "a quoted key must fold, not trip the whole-tree reset: {:?}",
            report.warnings()
        );

        let config = crate::config::Config::load_from_path(&main_path, dir.join("workspace"))
            .expect("test: regenerated config must load");
        assert_eq!(
            config.reliability.model_fallbacks.get("gpt-4.1").map(Vec::as_slice),
            Some(["gpt-4o".to_string(), "claude-sonnet-4-6".to_string()].as_slice()),
            "a quoted map key must keep both its name and its value"
        );
    }

    /// The other half of the same bug: a *template* that writes a quoted key or
    /// sits under a quoted header. No shipped template does today, so this
    /// exercises the rewriter directly — the day one does, the operator's value
    /// must win there too.
    #[test]
    fn a_template_written_quoted_key_takes_the_operator_value() {
        let template =
            "[agent]\n\"my.key\" = \"from-template\"\n\n[channels_config.\"x.y\"]\ntoken = \"from-template\"\n";
        let previous = toml::from_str::<toml::Value>(
            "[agent]\n\"my.key\" = \"from-operator\"\n\n[channels_config.\"x.y\"]\ntoken = \"from-operator\"\n",
        )
        .expect("test: parse previous configuration");
        let mut outcome = PreservationOutcome::default();

        let merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
            .expect("test: fold the previous configuration");

        assert!(
            outcome.unfoldable.is_empty(),
            "every quoted key must be foldable: {:?}",
            outcome.unfoldable
        );
        assert!(
            merged.contains("\"my.key\" = \"from-operator\""),
            "a quoted assignment must take the operator value: {merged}"
        );
        assert!(
            merged.contains("token = \"from-operator\""),
            "a key under a quoted header must take the operator value: {merged}"
        );
        let reparsed = toml::from_str::<toml::Value>(&merged).expect("test: the merged file must still parse");
        assert_eq!(
            reparsed
                .get("channels_config")
                .and_then(|table| table.get("x.y"))
                .and_then(|table| table.get("token"))
                .and_then(toml::Value::as_str),
            Some("from-operator"),
            "the value must land on the quoted path, not on a split one: {merged}"
        );
        assert!(outcome.kept.contains("agent.\"my.key\""), "kept: {:?}", outcome.kept);
        assert!(
            outcome.kept.contains("channels_config.\"x.y\".token"),
            "kept: {:?}",
            outcome.kept
        );
    }

    /// Whatever the rewriter still cannot place is named and fails the command.
    /// Silence is the defect this whole area was built around.
    #[test]
    fn a_setting_that_cannot_be_folded_is_named_and_exits_non_zero() {
        // An array of tables is left verbatim by the rewriter, so an operator
        // value there has nowhere to go.
        let template = "[[route]]\nname = \"from-template\"\n";
        let previous =
            toml::from_str::<toml::Value>("[[route]]\nname = \"from-operator\"\n").expect("test: parse previous");
        let mut outcome = PreservationOutcome::default();

        let _merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
            .expect("test: fold the previous configuration");
        assert_eq!(
            outcome.unfoldable.iter().cloned().collect::<Vec<_>>(),
            vec!["route".to_string()],
            "an unplaceable override must be recorded, not dropped in silence"
        );

        let mut report = InitReport::default();
        report.record_preservation(&outcome, None);
        let warning = report
            .warnings()
            .iter()
            .find(|line| line.contains("could not fold"))
            .expect("test: the operator must be warned");
        assert!(warning.contains("route"), "the warning must name the key: {warning}");
        let error = report
            .into_result()
            .expect_err("test: an unfoldable setting must exit non-zero");
        assert!(format!("{error:#}").contains("route"), "{error:#}");
    }

    /// A key whose shape this build can no longer hold is the same data loss as
    /// one the rewriter could not place, so it ends the command the same way
    /// and is listed under its own heading rather than folded into the other.
    #[test]
    fn a_setting_this_build_cannot_represent_is_listed_and_exits_non_zero() {
        // The template now writes `agent.mode` as a scalar, so the previous
        // tree's table at that path has nowhere to go.
        let template = "[agent]\nmode = \"auto\"\n";
        let previous = toml::from_str::<toml::Value>("[agent.mode]\nlegacy = true\n").expect("test: parse previous");
        let mut outcome = PreservationOutcome::default();

        let _merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
            .expect("test: fold the previous configuration");
        assert_eq!(
            outcome.dropped.iter().cloned().collect::<Vec<_>>(),
            vec!["agent.mode.legacy".to_string()],
            "a key this build cannot represent must be recorded"
        );
        assert!(
            outcome.unfoldable.is_empty(),
            "the two loss classes must stay distinct: {:?}",
            outcome.unfoldable
        );

        let mut report = InitReport::default();
        report.record_preservation(&outcome, None);
        assert_eq!(report.dropped_keys(), ["agent.mode.legacy".to_string()]);
        let error = report
            .into_result()
            .expect_err("test: a setting this build cannot represent must exit non-zero");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("can no longer represent") && rendered.contains("agent.mode.legacy"),
            "the error must list the dropped keys under their own heading: {rendered}"
        );
    }

    /// The mirror of the test above: a fold that loses nothing still exits zero.
    #[test]
    fn a_fold_that_drops_nothing_exits_zero() {
        let template = "[agent]\nmode = \"auto\"\n";
        let previous = toml::from_str::<toml::Value>("[agent]\nmode = \"manual\"\n").expect("test: parse previous");
        let mut outcome = PreservationOutcome::default();

        let merged = apply_previous_settings(template, &previous, &previous, None, &mut outcome)
            .expect("test: fold the previous configuration");
        assert!(merged.contains("mode = \"manual\""), "{merged}");
        assert!(outcome.dropped.is_empty(), "dropped: {:?}", outcome.dropped);

        let mut report = InitReport::default();
        report.record_preservation(&outcome, None);
        assert!(report.dropped_keys().is_empty());
        report.into_result().expect("test: a lossless fold must exit zero");
    }

    /// A step that runs after the mutation commits must not take the report
    /// with it: the operator's settings are already off the live tree, so the
    /// reset warning, the archive that holds them and the failure itself all
    /// have to reach the caller, and the run still ends non-zero.
    #[tokio::test]
    async fn a_failure_after_the_commit_keeps_the_committed_report() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        // `allowed_users` is required, so the preserved settings cannot validate
        // and `--force` falls back to the shipped templates.
        fs::write(
            dir.join("config.d/channels.toml"),
            "[channels_config]\ncli = true\n\n[channels_config.telegram]\nbot_token = \"unit-operator-token\"\n",
        )
        .expect("test: write a fragment this build cannot load");
        // Obstruct a workspace subdirectory so the scaffolding after the commit fails.
        let sessions = dir.join("workspace").join("sessions");
        fs::remove_dir_all(&sessions).expect("test: clear workspace/sessions");
        fs::write(&sessions, "not a directory").expect("test: obstruct workspace/sessions");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: a failure after the commit must still return the report");

        assert!(
            report.was_reset_to_templates(),
            "the fixture must reproduce the reset: {:?}",
            report.warnings()
        );
        let failure = report
            .post_commit_failure()
            .expect("test: the failure that stopped the run must be reported");
        assert!(
            failure.contains("workspace/sessions"),
            "the failure must name what it could not create: {failure}"
        );

        let archives = archive_dirs(dir);
        assert_eq!(archives.len(), 1, "exactly one archive: {archives:?}");
        let archive = archives.first().expect("test: the archive must exist");
        let archive_path = archive.display().to_string();
        assert!(
            report.warnings().iter().any(|line| line.contains(&archive_path)),
            "the archive path must be printed: {:?}",
            report.warnings()
        );
        assert!(
            fs::read_to_string(archive.join("channels.toml"))
                .expect("test: read the archived fragment")
                .contains("unit-operator-token"),
            "the archive must hold the operator's original value"
        );
        assert!(
            !fs::read_to_string(dir.join("config.d/channels.toml"))
                .expect("test: read the regenerated fragment")
                .contains("unit-operator-token"),
            "the reset the report describes must actually have committed"
        );

        let error = report
            .into_result()
            .expect_err("test: a failure after the commit must exit non-zero");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("workspace/sessions") && rendered.contains("reset"),
            "the error must name both the failure and the mutation: {rendered}"
        );
    }

    // ── Archive directories ──────────────────────────────────────

    #[test]
    fn archive_directories_never_overwrite_one_another() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let first = create_unique_archive_dir(tmp.path(), "config.d.pre-0.0.0-20260915120000000")
            .expect("test: first archive directory");
        let second = create_unique_archive_dir(tmp.path(), "config.d.pre-0.0.0-20260915120000000")
            .expect("test: second archive directory");

        assert_ne!(
            first, second,
            "a second archive with the same stamp must get its own directory"
        );
        assert!(first.is_dir() && second.is_dir(), "both archives must exist");
    }

    #[tokio::test]
    async fn two_force_runs_in_the_same_second_keep_both_archives() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let memory_path = dir.join("config.d/memory.toml");
        fs::write(
            &memory_path,
            "[memory]\nbackend = \"sqlite\"\nembedding_model = \"first-model\"\n",
        )
        .expect("test: write the first operator fragment");
        Spec::Full.generate(dir, true).await.expect("test: first force");
        fs::write(
            &memory_path,
            "[memory]\nbackend = \"sqlite\"\nembedding_model = \"second-model\"\n",
        )
        .expect("test: write the second operator fragment");
        Spec::Full.generate(dir, true).await.expect("test: second force");

        let mut archived_models = fs::read_dir(dir)
            .expect("test: read target dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("config.d.pre-"))
            })
            .map(|archive| fs::read_to_string(archive.join("memory.toml")).unwrap_or_default())
            .collect::<Vec<_>>();
        archived_models.sort();

        assert_eq!(
            archived_models.len(),
            2,
            "each --force run must keep its own archive, found: {archived_models:?}"
        );
        assert!(
            archived_models.iter().any(|body| body.contains("first-model")),
            "the first run's archive must survive the second run: {archived_models:?}"
        );
        assert!(
            archived_models.iter().any(|body| body.contains("second-model")),
            "the second run's archive must exist: {archived_models:?}"
        );
    }

    /// The defaults sidecar is the baseline the *next* `--force` classifies
    /// against, so an archive that omits it cannot restore a working tree.
    #[tokio::test]
    async fn the_archive_carries_the_defaults_sidecar() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        assert!(
            dir.join(MANAGED_DEFAULTS_FILE).is_file(),
            "the first run must write the defaults record"
        );

        Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: force regenerate");

        let archives = archive_dirs(dir);
        assert_eq!(archives.len(), 1, "one --force run makes one archive: {archives:?}");
        let archive = archives.first().expect("test: the archive must exist");
        assert!(
            archive.join(MANAGED_DEFAULTS_FILE).is_file(),
            "the archive must carry the defaults record next to the files it describes"
        );
        assert!(
            archive.join("config.toml").is_file() && archive.join("memory.toml").is_file(),
            "the archive must still carry config.toml and the fragments"
        );
    }

    /// A tree written by a build older than the sidecar has none to archive,
    /// which is an ordinary case and not a failure.
    #[tokio::test]
    async fn archiving_a_tree_without_a_sidecar_succeeds() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        fs::remove_file(dir.join(MANAGED_DEFAULTS_FILE)).expect("test: remove the defaults record");

        Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: an older tree must still regenerate");

        let archives = archive_dirs(dir);
        assert_eq!(archives.len(), 1, "one --force run makes one archive: {archives:?}");
        let archive = archives.first().expect("test: the archive must exist");
        assert!(
            !archive.join(MANAGED_DEFAULTS_FILE).exists(),
            "there was no sidecar to archive"
        );
        assert!(
            archive.join("config.toml").is_file(),
            "the archive must still be written"
        );
    }

    /// Every `config.d.pre-*` directory directly under `dir`, sorted.
    fn archive_dirs(dir: &Path) -> Vec<PathBuf> {
        let mut archives = fs::read_dir(dir)
            .expect("test: read target dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("config.d.pre-"))
            })
            .collect::<Vec<_>>();
        archives.sort();
        archives
    }

    // ── Default refresh semantics ────────────────────────────────

    /// Read the value a freshly rendered template writes for `path`, so these
    /// tests state the *rule* and never a copy of a shipped default.
    fn template_default(template: &str, path: &[&str]) -> toml::Value {
        let value = toml::from_str::<toml::Value>(template).expect("test: the template must be valid TOML");
        let owned = path.iter().map(|segment| (*segment).to_string()).collect::<Vec<_>>();
        toml_lookup(&value, &owned)
            .cloned()
            .unwrap_or_else(|| panic!("test: the template must write {path:?}"))
    }

    #[tokio::test]
    async fn force_refresh_separates_preserved_added_and_reset_defaults() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let template = memory_template(Spec::Full);
        let shipped_auto_save = template_default(&template, &["memory", "auto_save"]);
        let shipped_record_tool_events = template_default(&template, &["memory", "events", "record_tool_events"]);

        // The tree on disk holds two values that differ from this build's
        // templates. Only one of them was ever typed by the operator.
        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = false\n\n[memory.events]\nrecord_tool_events = true\n",
        )
        .expect("test: write operator memory fragment");
        // The sidecar says `auto_save = false` is what the *previous* templates
        // shipped, so it is a stale default; `record_tool_events` is recorded at
        // its shipped value, so the operator's `true` is their own choice.
        fs::write(
            dir.join(MANAGED_DEFAULTS_FILE),
            "[[entry]]\npath = [\"memory\", \"auto_save\"]\nvalue = false\n\n\
             [[entry]]\npath = [\"memory\", \"events\", \"record_tool_events\"]\nvalue = false\n",
        )
        .expect("test: write the previous defaults record");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: force regenerate");
        let notices = report.notices().join("\n");

        let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: regenerated config must load");
        assert_eq!(
            toml::Value::Boolean(config.memory.auto_save),
            shipped_auto_save,
            "a default the operator never changed must follow the template: {notices}"
        );
        assert_ne!(
            toml::Value::Boolean(config.memory.events.record_tool_events),
            shipped_record_tool_events,
            "a value the operator wrote must win over a refreshed default: {notices}"
        );

        let reset_line = report
            .notices()
            .iter()
            .find(|line| line.contains("refreshed defaults"))
            .expect("test: the refreshed defaults must be reported");
        assert!(reset_line.contains("memory.auto_save"), "{reset_line}");
        let preserved_line = report
            .notices()
            .iter()
            .find(|line| line.contains("Preserved"))
            .expect("test: the preserved settings must be reported");
        assert!(
            preserved_line.contains("memory.events.record_tool_events"),
            "{preserved_line}"
        );
        let added_line = report
            .notices()
            .iter()
            .find(|line| line.contains("new default(s) this version ships"))
            .expect("test: keys this version adds must be reported");
        assert!(
            added_line.contains("memory.embedding_provider"),
            "a key absent from the previous tree is an added default: {added_line}"
        );
        // The three classes are disjoint.
        assert!(!reset_line.contains("memory.events.record_tool_events"), "{reset_line}");
        assert!(!preserved_line.contains("memory.auto_save"), "{preserved_line}");
    }

    #[tokio::test]
    async fn without_a_defaults_record_every_current_value_is_preserved() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");
        assert!(
            dir.join(MANAGED_DEFAULTS_FILE).is_file(),
            "prx init must record which values came from its own templates"
        );

        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = false\n",
        )
        .expect("test: write operator memory fragment");
        // A tree written by a build that had no record at all.
        fs::remove_file(dir.join(MANAGED_DEFAULTS_FILE)).expect("test: remove the defaults record");

        let report = Spec::Full
            .generate_reported(dir, true)
            .await
            .expect("test: force regenerate");

        let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: regenerated config must load");
        assert!(
            !config.memory.auto_save,
            "with no record of the previous defaults, every value on disk counts as the operator's"
        );
        assert!(
            report.notices().iter().all(|line| !line.contains("refreshed defaults")),
            "nothing may be reset without evidence that it was a shipped default: {:?}",
            report.notices()
        );
    }

    #[test]
    fn spec_name_matches_variant() {
        assert_eq!(Spec::Minimal.name(), "minimal");
        assert_eq!(Spec::Server.name(), "server");
        assert_eq!(Spec::Full.name(), "full");
    }

    #[test]
    fn main_config_template_contains_spec_name() {
        let content = main_config_template(Spec::Server);
        assert!(content.contains("--spec server"));
        assert!(!content.contains("[modules]"));
        assert!(content.contains("default_model"));
    }

    #[tokio::test]
    async fn generate_creates_expected_files() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal
            .generate(dir, false)
            .await
            .expect("test: generate minimal");

        assert!(dir.join("config.toml").exists());
        assert!(dir.join("config.d").is_dir());
        assert!(dir.join("workspace/sessions").is_dir());
        assert!(dir.join("workspace/memory").is_dir());
        assert!(dir.join("workspace/state").is_dir());
        assert!(dir.join("workspace/cron").is_dir());
        assert!(dir.join("workspace/skills").is_dir());

        for name in super::super::files::managed_fragment_names() {
            assert!(dir.join("config.d").join(name).exists(), "missing config.d/{name}");
        }
    }

    #[tokio::test]
    async fn generate_full_creates_all_module_files() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Full.generate(dir, false).await.expect("test: generate full");

        for name in &[
            "memory.toml",
            "channels.toml",
            "network.toml",
            "security.toml",
            "scheduler.toml",
            "agent.toml",
            "identity.toml",
            "routing.toml",
            "tools.toml",
            "integrations.toml",
            "nodes.toml",
            "cost.toml",
            "observability.toml",
        ] {
            assert!(dir.join("config.d").join(name).exists(), "missing config.d/{name}");
        }
    }

    #[tokio::test]
    async fn generated_templates_load_as_valid_config() {
        // BUG-D1-03: the generated server/full templates must be loadable
        // out-of-the-box. Regression guard against illegal enum values / missing
        // required fields (compaction mode, autonomy.forbidden_paths,
        // tunnel.provider). Loads through the real merge + deserialize + validate
        // path so it exercises exactly what `prx info` would.
        for spec in [Spec::Minimal, Spec::Server, Spec::Full] {
            let tmp = tempfile::tempdir().expect("test: create tempdir");
            let dir = tmp.path();
            spec.generate(dir, false)
                .await
                .unwrap_or_else(|e| panic!("test: generate {}: {e}", spec.name()));

            let config_path = dir.join("config.toml");
            let workspace_dir = dir.join("workspace");
            crate::config::Config::load_from_path(&config_path, workspace_dir).unwrap_or_else(|e| {
                panic!(
                    "test: generated `{}` template must load as a valid Config: {e:#}",
                    spec.name()
                )
            });
        }
    }

    #[tokio::test]
    async fn generated_full_template_round_trips_schema_native_values() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: full template must deserialize without ignored paths");
        assert_eq!(config.http_request.max_response_size, 10_485_760);
        assert_eq!(config.memory.embedding_provider, "local");
        assert_eq!(config.memory.embedding_model, "prx-local-hash-v1");
        assert_eq!(config.memory.embedding_dimensions, 384);
    }

    #[tokio::test]
    async fn generated_config_rejects_unknown_paths() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Minimal
            .generate(dir, false)
            .await
            .expect("test: generate minimal");

        let tools_path = dir.join("config.d/tools.toml");
        fs::write(&tools_path, "[web_search]\nprovider = \"duckduckgo\"\ntyop = true\n")
            .expect("test: write invalid tools fragment");
        let error = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect_err("test: unknown config path must fail")
            .to_string();
        assert!(error.contains("web_search.tyop"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn legacy_config_keys_are_migrated_explicitly() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();
        Spec::Minimal
            .generate(dir, false)
            .await
            .expect("test: generate minimal");

        let main_path = dir.join("config.toml");
        let mut main = fs::read_to_string(&main_path).expect("test: read main config");
        main.push_str("\n[modules]\nmemory = false\ntools = true\n");
        fs::write(&main_path, main).expect("test: write legacy module gates");

        fs::write(
            dir.join("config.d/tools.toml"),
            "[http_request]\nmax_response_bytes = 2048\nenabled = false\n",
        )
        .expect("test: write legacy tools fragment");
        fs::write(
            dir.join("config.d/memory.toml"),
            "[memory]\nbackend = \"sqlite\"\nauto_save = true\n[memory.embedding]\nprovider = \"none\"\nmodel = \"legacy\"\ndimension = 512\n",
        )
        .expect("test: write legacy memory fragment");

        let config = crate::config::Config::load_from_path(&dir.join("config.toml"), dir.join("workspace"))
            .expect("test: known legacy keys must migrate");
        assert_eq!(config.http_request.max_response_size, 2048);
        assert_eq!(config.memory.embedding_provider, "none");
        assert_eq!(config.memory.embedding_model, "legacy");
        assert_eq!(config.memory.embedding_dimensions, 512);
    }

    #[tokio::test]
    async fn generated_server_full_templates_do_not_pin_compaction_context() {
        for spec in [Spec::Server, Spec::Full] {
            let tmp = tempfile::tempdir().expect("test: create tempdir");
            let dir = tmp.path();
            spec.generate(dir, false)
                .await
                .unwrap_or_else(|e| panic!("test: generate {}: {e}", spec.name()));

            let agent = fs::read_to_string(dir.join("config.d/agent.toml")).expect("test: read agent template");
            assert!(
                !agent.lines().any(|line| {
                    let trimmed = line.trim_start();
                    trimmed.starts_with("max_context_tokens") && !trimmed.starts_with('#')
                }),
                "{} template must not explicitly override model-derived context",
                spec.name()
            );

            let config_path = dir.join("config.toml");
            let workspace_dir = dir.join("workspace");
            let loaded = crate::config::Config::load_from_path(&config_path, workspace_dir)
                .unwrap_or_else(|e| panic!("test: generated `{}` template must load: {e:#}", spec.name()));
            assert!(
                !loaded.agent.compaction.max_context_tokens_explicit,
                "{} template must leave compaction context model-derived",
                spec.name()
            );
        }
    }

    #[tokio::test]
    async fn generate_refuses_overwrite_without_force() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).await.expect("test: first generate");
        let result = Spec::Minimal.generate(dir, false).await;
        assert!(result.is_err());
        assert!(
            result
                .as_ref()
                .err()
                .map_or(false, |e| format!("{e}").contains("--force"))
        );
    }

    #[tokio::test]
    async fn generate_allows_overwrite_with_force() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).await.expect("test: first generate");
        Spec::Server.generate(dir, true).await.expect("test: force overwrite");

        let content = fs::read_to_string(dir.join("config.toml")).expect("test: read config");
        assert!(content.contains("--spec server"));
    }

    #[tokio::test]
    async fn force_regeneration_removes_stale_managed_fragments_but_preserves_unknown() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Full.generate(dir, false).await.expect("test: generate full");
        let unknown = dir.join("config.d/operator-owned.toml");
        fs::write(&unknown, "[operator_owned]\nvalue = 7\n").expect("test: write unknown fragment");

        Spec::Minimal
            .generate(dir, true)
            .await
            .expect("test: force regenerate minimal");

        assert!(dir.join("config.d/memory.toml").exists());
        assert!(dir.join("config.d/agent.toml").exists());
        assert!(unknown.exists(), "unknown user-owned fragment must be preserved");
        for managed in [
            "channels.toml",
            "network.toml",
            "security.toml",
            "scheduler.toml",
            "identity.toml",
            "routing.toml",
            "tools.toml",
            "integrations.toml",
            "nodes.toml",
            "cost.toml",
            "observability.toml",
        ] {
            assert!(dir.join("config.d").join(managed).exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn generated_files_have_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal
            .generate(dir, false)
            .await
            .expect("test: generate minimal");

        let config_perms = fs::metadata(dir.join("config.toml"))
            .expect("test: config metadata")
            .permissions()
            .mode();
        // Check that the file permission bits (lower 9 bits) are 0o600
        assert_eq!(config_perms & 0o777, 0o600);
    }

    #[tokio::test]
    async fn generate_creates_workspace_md_files() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Full.generate(dir, false).await.expect("test: generate full");

        let ws = dir.join("workspace");
        for name in &[
            "IDENTITY.md",
            "AGENTS.md",
            "HEARTBEAT.md",
            "SOUL.md",
            "USER.md",
            "TOOLS.md",
            "BOOTSTRAP.md",
            "MEMORY.md",
            "THINKING.md",
        ] {
            assert!(ws.join(name).exists(), "missing workspace/{name}");
        }

        // Verify content includes agent name
        let soul = fs::read_to_string(ws.join("SOUL.md")).expect("test: read SOUL.md");
        assert!(soul.contains("OpenPRX"), "SOUL.md should contain agent name");

        // Verify content includes user from env (or fallback)
        let user_md = fs::read_to_string(ws.join("USER.md")).expect("test: read USER.md");
        assert!(user_md.contains("Name:"), "USER.md should contain Name field");

        let tools_md = fs::read_to_string(ws.join("TOOLS.md")).expect("test: read TOOLS.md");
        assert!(tools_md.contains("runtime `ToolSpec` snapshot"));
        assert!(!tools_md.contains("## Built-in Tools"));
    }

    #[test]
    fn full_security_template_configures_always_on_audit() {
        let content = security_template(Spec::Full);
        assert!(content.contains("[security.audit]"));
        assert!(content.contains("log_path = \"audit.log\""));
        assert!(content.contains("max_size_mb = 100"));
        assert!(content.contains("[compliance.interaction_notice]"));
        assert!(content.contains("[compliance.eu_ai_act.classification]"));
        assert!(content.contains("status = \"unclassified\""));
    }

    #[test]
    fn full_security_template_is_open_by_default() {
        let content = security_template(Spec::Full);
        assert!(content.contains("level = \"full\""));
        assert!(content.contains("workspace_only = false"));
        assert!(content.contains("forbidden_paths = []"));
        assert!(!content.contains("[autonomy.sandbox]"));
    }

    #[test]
    fn server_security_template_defaults_to_autonomous_with_workspace_boundary() {
        let content = security_template(Spec::Server);
        assert!(content.contains("level = \"full\""));
        assert!(content.contains("workspace_only = true"));
        assert!(content.contains("max_cost_per_day_cents = 500"));
        assert!(content.contains("\"/etc\""));
        assert!(!content.contains("level = \"supervised\""));
    }

    #[test]
    fn full_memory_template_uses_schema_native_embedding_keys() {
        let content = memory_template(Spec::Full);
        assert!(!content.contains("[memory.embedding]"));
        assert!(content.contains("# embedding_provider = \"custom:http://127.0.0.1:11434/v1\""));
        assert!(content.contains("# embedding_model = \"nomic-embed-text:latest\""));
        assert!(content.contains("# embedding_dimensions = 768"));
    }

    #[test]
    fn server_scheduler_template_is_commented_scaffold() {
        let content = scheduler_template(Spec::Server);
        assert!(!content.is_empty(), "server scheduler template must not be empty");
        assert!(content.contains("Scheduler configuration (server)"));
        // All actual table headers must stay commented so the disabled module
        // does not get activated by the scaffold.
        assert!(content.contains("# [scheduler]"));
        assert!(content.contains("# [cron]"));
        assert!(content.contains("# [heartbeat]"));
        assert!(!content.contains("\n[scheduler]\n"));
    }

    #[test]
    fn server_channels_template_is_commented_scaffold() {
        let content = channels_template(Spec::Server);
        assert!(!content.is_empty(), "server channels template must not be empty");
        assert!(content.contains("Channels configuration (server)"));
        assert!(content.contains("# [channels_config.telegram]"));
        // No platform should be active by default.
        assert!(!content.contains("\n[channels_config.telegram]\n"));
    }

    #[tokio::test]
    async fn server_generate_writes_scheduler_and_channels_scaffolds() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Server.generate(dir, false).await.expect("test: generate server");

        // Server preset writes configuration scaffolds without module gates.
        assert!(dir.join("config.d/scheduler.toml").exists());
        assert!(dir.join("config.d/channels.toml").exists());

        let scheduler = fs::read_to_string(dir.join("config.d/scheduler.toml")).expect("test: read scheduler");
        assert!(scheduler.contains("# [scheduler]"));
        assert!(scheduler.contains("# claim_lease_secs = 90"));
        let channels = fs::read_to_string(dir.join("config.d/channels.toml")).expect("test: read channels");
        assert!(channels.contains("# [channels_config.telegram]"));

        let config = fs::read_to_string(dir.join("config.toml")).expect("test: read config");
        assert!(!config.contains("[modules]"));
    }

    #[tokio::test]
    async fn scaffold_skips_existing_md_files() {
        let tmp = tempfile::tempdir().expect("test: create tempdir");
        let dir = tmp.path();

        Spec::Minimal.generate(dir, false).await.expect("test: first generate");

        // Write a custom SOUL.md before second generate
        let soul_path = dir.join("workspace/SOUL.md");
        fs::write(&soul_path, "# My custom soul\n").expect("test: write custom soul");

        // Force regenerate — config files are overwritten, but .md files should be skipped
        Spec::Full.generate(dir, true).await.expect("test: force overwrite");

        let soul = fs::read_to_string(&soul_path).expect("test: read SOUL.md");
        assert_eq!(soul, "# My custom soul\n", "existing .md files must not be overwritten");
    }

    /// D-074 with X-3: the full template's `[causal_tree]` block, unchanged
    /// since BASE `8a5d160d`, is the old serialized default: the migration
    /// reads its bare `enabled = false` as unset, so the CDP-31 default flip
    /// reaches installs that were never saved.
    #[test]
    fn the_base_full_template_disable_reads_as_unset() {
        let mut root: toml::Value = toml::from_str(&agent_template(Spec::Full)).expect("test: template parses");
        assert_eq!(
            root.get("causal_tree")
                .and_then(|table| table.as_table())
                .map(|table| table.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["enabled".to_string()]),
            "the template writes a bare disable"
        );
        let report = crate::causal_tree::config::migrate_legacy(&mut root, &|_| None).expect("test: migrates");
        assert_eq!(report.dropped, ["enabled"]);
        assert!(root.get("causal_tree").and_then(|table| table.get("enabled")).is_none());
    }
}
