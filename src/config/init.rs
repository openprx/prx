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
        self.generate_customized(target_dir, force, None, None).await
    }

    pub(crate) async fn generate_customized(
        self,
        target_dir: &Path,
        force: bool,
        customize: Option<&(dyn Fn(&mut super::Config) -> Result<()> + Send + Sync)>,
        identity: Option<&crate::identity::scaffold::ProjectContext>,
    ) -> Result<InitReport> {
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
        let mut preserved_revision = if regenerating {
            Some(hex::encode(super::files::compute_config_fingerprint_gated(
                &config_path,
            )?))
        } else {
            None
        };
        let (effective_main, effective_fragments) = if regenerating {
            let (merged_main, merged_fragments, outcome) =
                preserve_previous_settings(&config_path, &main_toml, &fragments, previous_defaults.as_ref())?;
            report.record_preservation(&outcome, archive.as_deref());
            causal_guard = outcome.causal_guard;
            preserved_revision = outcome.base_revision.or(preserved_revision);
            (merged_main, merged_fragments)
        } else {
            (main_toml.clone(), fragments.clone())
        };

        // 4. Stage and validate the complete effective configuration before
        // mutation. Unknown/legacy fragment names remain excluded by the loader
        // and are retained unchanged by the existing mutation planner.
        let mut plan =
            match super::files::plan_mutation(&config_path, &workspace_dir, effective_main, effective_fragments) {
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
        if let Some(customize) = customize {
            let mut candidate =
                super::Config::from_stored_tree(plan.effective_stored_tree()?, &config_path, workspace_dir.clone())?;
            customize(&mut candidate)?;
            candidate.validate()?;
            let (main, fragments) = candidate.to_split_toml_strings()?;
            plan = super::files::plan_mutation(&config_path, &workspace_dir, main, fragments)?;
        }
        let committed = super::files::commit_mutation_guarded(plan, preserved_revision.as_deref(), move |before| {
            anyhow::ensure!(
                regenerating
                    || !before
                        .files
                        .iter()
                        .any(|file| file.relative_path == "config.toml" && file.contents.is_some()),
                "Configuration was created concurrently; nothing was overwritten"
            );
            Ok(())
        })
        .await?;
        anyhow::ensure!(
            regenerating || !matches!(committed, super::files::GuardedCommit::Unchanged { .. }),
            "Configuration was created concurrently; nothing was overwritten"
        );

        // 5. The mutation is on disk from here on. A failure in the steps that
        // follow is still a failure, but it must not take the report with it:
        // returning `Err` here used to discard the reset warning and the
        // archive path, leaving the operator with defaults, an exit code and
        // an empty stdout. The failure is carried in the report instead, so
        // `print` still shows what committed and `into_result` still exits
        // non-zero — now naming both the mutation and the error that stopped
        // the run.
        if let Err(error) = finish_after_commit(target_dir, &workspace_dir, &managed_defaults, identity) {
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

        report.notices.push(
            "  Main role: workspace/identities/main (legacy root role files, if any, are retained as backups)".into(),
        );
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
fn finish_after_commit(
    target_dir: &Path,
    workspace_dir: &Path,
    managed_defaults: &str,
    identity: Option<&crate::identity::scaffold::ProjectContext>,
) -> Result<()> {
    write_managed_defaults(target_dir, managed_defaults)?;
    std::fs::create_dir_all(workspace_dir)
        .with_context(|| format!("Failed to create workspace in {}", target_dir.display()))?;
    for subdir in &["sessions", "memory", "state", "cron", "skills"] {
        std::fs::create_dir_all(workspace_dir.join(subdir))
            .with_context(|| format!("Failed to create workspace/{subdir} in {}", target_dir.display()))?;
    }
    match identity {
        Some(ctx) => crate::identity::root::initialize(workspace_dir, ctx)?,
        None => scaffold_workspace_defaults(workspace_dir)?,
    }
    #[cfg(unix)]
    set_directory_permissions(target_dir)?;
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
// Generates the same 9 persona/workflow files that `prx init` creates,
// using safe defaults so `prx init --spec full` produces a ready-to-use
// workspace without running the interactive wizard.

fn scaffold_workspace_defaults(workspace_dir: &Path) -> Result<()> {
    crate::identity::root::initialize(workspace_dir, &crate::identity::scaffold::default_context())
}

// ── Main config.toml template ───────────────────────────────────

mod templates;
use templates::{
    agent_template, channels_template, cost_template, identity_template, integrations_template, main_config_template,
    memory_template, network_template, nodes_template, observability_template, routing_template, scheduler_template,
    security_template, tools_template,
};

#[cfg(test)]
mod tests;
