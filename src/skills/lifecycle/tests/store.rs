use super::{basic_entries, entry, md, owner};
use crate::skills::lifecycle::{
    Bundle, BundleEntry, LifecycleError, NewRevision, OwnerScope, RevisionId, SkillId, SkillLifecycleStore,
    SkillRevision, SourceKind,
};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const OWNER_A: &str = "owner:ws:cli:alice";
const OWNER_B: &str = "owner:ws:cli:bob";

fn open(dir: &Path, who: &str) -> SkillLifecycleStore {
    SkillLifecycleStore::open(dir, owner(who)).expect("test: store opens")
}

fn skill() -> SkillId {
    SkillId::parse("deploy-helper").expect("test: valid skill id")
}

fn publish(store: &SkillLifecycleStore, entries: Vec<BundleEntry>) -> (RevisionId, PathBuf) {
    let bundle = Bundle::from_entries(entries).expect("test: bundle");
    let outcome = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect("test: write");
    let id = outcome.revision.revision_id;
    let dir = store.root().join("revisions").join(skill().as_str()).join(id.as_str());
    (id, dir)
}

fn setup() -> (TempDir, SkillLifecycleStore) {
    let tmp = TempDir::new().expect("test: tempdir");
    let store = open(tmp.path(), OWNER_A);
    (tmp, store)
}

/// Adds a stray file with owner-only permissions, so the refusal under test is
/// the structural one and not the permission check.
fn plant(path: &Path) {
    fs::write(path, b"x").expect("test: add");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("test: chmod");
    }
}

fn private_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("test: chmod");
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn read_err(store: &SkillLifecycleStore, id: &RevisionId) -> LifecycleError {
    store
        .read_revision(&skill(), id, None)
        .expect_err("test: read must fail")
}

#[test]
fn written_revision_reads_back_verified() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    let verified = store.read_revision(&skill(), &id, None).expect("test: read");
    assert_eq!(verified.bundle.revision_id(), &id);
    assert_eq!(verified.revision.owner_scope, owner(OWNER_A));
    assert_eq!(verified.revision.display_name, "Deploy helper");
    assert_eq!(verified.revision.record_id, id.to_string());
    assert_eq!(verified.revision.bundle_manifest_hash, id.digest_hex());
    assert!(dir.join("bundle/references/guide.md").is_file());
    // Markdown bytes are stored untouched.
    let stored = fs::read(dir.join("bundle/references/guide.md")).expect("test: read file");
    assert_eq!(stored, b"# Guide\r\nkeep CRLF\r\n");
    let leftovers = fs::read_dir(store.root().join("staging"))
        .expect("test: staging")
        .count();
    assert_eq!(leftovers, 0, "staging must be clean after a write");
}

#[test]
fn rewriting_an_existing_revision_is_idempotent_only_for_identical_provenance() {
    let (_tmp, store) = setup();
    let bundle = Bundle::from_entries(basic_entries()).expect("test: bundle");
    let new = NewRevision::new(skill(), SourceKind::Operator);
    let first = store.write_revision(&new, &bundle).expect("test: first write");
    assert!(first.created);
    let record = store
        .root()
        .join("revisions")
        .join(skill().as_str())
        .join(first.revision.revision_id.as_str())
        .join("revision.json");
    let before = fs::read(&record).expect("test: record");

    let second = store.write_revision(&new, &bundle).expect("test: identical rewrite");
    assert!(!second.created);
    assert_eq!(second.revision, first.revision);
    assert_eq!(second.record_digest, first.record_digest);

    let mut variants = Vec::new();
    let mut v = new.clone();
    v.source_kind = SourceKind::Learned;
    variants.push((v, "source_kind"));
    let mut v = new.clone();
    v.proposal_id = Some("p-1".to_string());
    variants.push((v, "proposal_id"));
    let mut v = new.clone();
    v.review_receipt_ids = vec!["rev-1".to_string()];
    variants.push((v, "review_receipt_ids"));
    let mut v = new;
    v.parent_revision_id = Some(first.revision.revision_id);
    variants.push((v, "parent_revision_id"));
    for (variant, field) in variants {
        let err = store
            .write_revision(&variant, &bundle)
            .expect_err("test: differing provenance");
        assert!(
            matches!(err, LifecycleError::RevisionConflict { field: f, .. } if f == field),
            "{field}: {err}"
        );
    }
    assert_eq!(fs::read(&record).expect("test: record"), before);
}

#[test]
fn pinned_record_digest_authenticates_non_derived_fields() {
    let (_tmp, store) = setup();
    let bundle = Bundle::from_entries(basic_entries()).expect("test: bundle");
    let mut new = NewRevision::new(skill(), SourceKind::Learned);
    new.review_receipt_ids = vec!["receipt-1".to_string()];
    let outcome = store.write_revision(&new, &bundle).expect("test: write");
    let id = outcome.revision.revision_id.clone();
    let digest = outcome.record_digest;
    let ok = store
        .read_revision(&skill(), &id, Some(&digest))
        .expect("test: pinned read");
    assert_eq!(ok.record_digest, digest);
    assert_eq!(
        crate::skills::lifecycle::record_digest(&ok.revision).expect("test: digest"),
        digest
    );

    let path = store
        .root()
        .join("revisions")
        .join(skill().as_str())
        .join(id.as_str())
        .join("revision.json");
    let text = fs::read_to_string(&path).expect("test: record");
    fs::write(&path, text.replace("receipt-1", "receipt-2")).expect("test: edit receipt");
    // Without a pinned digest the edit is undetectable; with one it is refused.
    store
        .read_revision(&skill(), &id, None)
        .expect("test: unpinned read accepts");
    let err = store
        .read_revision(&skill(), &id, Some(&digest))
        .expect_err("test: pinned read must fail");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
}

#[test]
fn losing_the_publication_race_verifies_and_compares_the_winner() {
    let (_tmp, store) = setup();
    let bundle = Bundle::from_entries(basic_entries()).expect("test: bundle");
    let info = crate::skills::lifecycle::validate_manifest(&bundle).expect("test: manifest");
    let new = NewRevision::new(skill(), SourceKind::Operator);
    let winner = store.write_revision(&new, &bundle).expect("test: winner");
    let final_dir = store
        .root()
        .join("revisions")
        .join(skill().as_str())
        .join(winner.revision.revision_id.as_str());
    let staged = || store.root().join("staging").join(uuid::Uuid::new_v4().to_string());

    // The final directory exists when the rename happens: identical provenance
    // returns the winner, differing provenance conflicts.
    let outcome = store
        .stage_and_publish(&new, &bundle, &info, bundle.revision_id(), &staged(), &final_dir)
        .expect("test: lost race, same provenance");
    assert!(!outcome.created);
    assert_eq!(outcome.record_digest, winner.record_digest);
    let mut other = new.clone();
    other.proposal_id = Some("p".to_string());
    let err = store
        .stage_and_publish(&other, &bundle, &info, bundle.revision_id(), &staged(), &final_dir)
        .expect_err("test: lost race, different provenance");
    assert!(matches!(err, LifecycleError::RevisionConflict { .. }), "{err}");

    // A tampered winner is reported, not trusted.
    fs::write(final_dir.join("bundle/references/guide.md"), b"changed").expect("test: tamper");
    let err = store
        .stage_and_publish(&new, &bundle, &info, bundle.revision_id(), &staged(), &final_dir)
        .expect_err("test: lost race, tampered winner");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
}

#[test]
fn changed_bundle_creates_a_distinct_revision() {
    let (_tmp, store) = setup();
    let (first, _) = publish(&store, basic_entries());
    let mut changed = basic_entries();
    changed.push(entry("references/new.md", b"new"));
    let (second, _) = publish(&store, changed);
    assert_ne!(first, second);
    store
        .read_revision(&skill(), &first, None)
        .expect("test: first still verifies");
}

#[test]
fn invalid_manifest_is_not_published() {
    let (_tmp, store) = setup();
    let bundle = Bundle::from_entries(vec![entry("SKILL.md", b"no frontmatter")]).expect("test: bundle");
    let err = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect_err("test: manifest rejected");
    assert!(matches!(err, LifecycleError::Manifest(_)), "{err}");
    let skill_dir = store.root().join("revisions").join(skill().as_str());
    assert!(!skill_dir.exists());
}

#[test]
fn missing_revision_is_reported() {
    let (_tmp, store) = setup();
    let id = RevisionId::parse(&format!("r1-{}", "0".repeat(64))).expect("test: id");
    assert!(matches!(read_err(&store, &id), LifecycleError::RevisionNotFound(_)));
}

#[test]
fn tampered_bundle_bytes_are_detected_on_read_and_never_repaired_by_rewrite() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    fs::write(dir.join("bundle/references/guide.md"), b"# Guide\nchanged\n").expect("test: tamper");
    assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
    let bundle = Bundle::from_entries(basic_entries()).expect("test: bundle");
    let err = store
        .write_revision(&NewRevision::new(skill(), SourceKind::Operator), &bundle)
        .expect_err("test: rewrite must report tampering");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
    assert_eq!(
        fs::read(dir.join("bundle/references/guide.md")).expect("test: read"),
        b"# Guide\nchanged\n",
        "a tampered revision must not be overwritten silently"
    );
}

#[test]
fn added_removed_and_unexpected_entries_are_detected() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    plant(&dir.join("bundle/extra.md"));
    assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
    fs::remove_file(dir.join("bundle/extra.md")).expect("test: rm");
    store.read_revision(&skill(), &id, None).expect("test: restored");

    plant(&dir.join("notes.txt"));
    assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
    fs::remove_file(dir.join("notes.txt")).expect("test: rm");

    fs::remove_file(dir.join("bundle/scripts/run.sh")).expect("test: rm");
    assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
}

#[test]
fn record_metadata_tampering_is_detected() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    let path = dir.join("revision.json");
    let original = fs::read_to_string(&path).expect("test: record");
    fs::write(&path, original.replace("Helps with deploys", "Does something else")).expect("test: tamper");
    assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
    fs::write(
        &path,
        original.replace(&id.to_string(), &format!("r1-{}", "1".repeat(64))),
    )
    .expect("test: tamper");
    assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
    fs::write(&path, "{ not json").expect("test: tamper");
    assert!(matches!(read_err(&store, &id), LifecycleError::InvalidRecord { .. }));
    fs::remove_file(&path).expect("test: rm");
    fs::create_dir(&path).expect("test: dir as file");
    private_dir(&path);
    assert!(matches!(
        read_err(&store, &id),
        LifecycleError::InvalidRecord { .. } | LifecycleError::Io { .. }
    ));
}

#[test]
fn forged_owner_in_a_record_is_denied() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    let path = dir.join("revision.json");
    let forged = fs::read_to_string(&path)
        .expect("test: record")
        .replace(OWNER_A, OWNER_B);
    fs::write(&path, forged).expect("test: forge");
    assert!(matches!(read_err(&store, &id), LifecycleError::OwnerMismatch { .. }));
}

#[test]
fn a_store_cannot_be_opened_for_another_owner() {
    let tmp = TempDir::new().expect("test: tempdir");
    open(tmp.path(), OWNER_A);
    let err = SkillLifecycleStore::open(tmp.path(), owner(OWNER_B)).expect_err("test: foreign owner");
    assert!(matches!(err, LifecycleError::OwnerMismatch { .. }), "{err}");
}

#[test]
fn a_foreign_workspace_revision_is_denied() {
    let ws_a = TempDir::new().expect("test: tempdir");
    let ws_b = TempDir::new().expect("test: tempdir");
    let store_a = open(ws_a.path(), OWNER_A);
    let store_b = open(ws_b.path(), OWNER_B);
    let (id, dir_a) = publish(&store_a, basic_entries());
    let skill_dir_b = store_b.root().join("revisions").join(skill().as_str());
    fs::create_dir_all(&skill_dir_b).expect("test: mkdir");
    copy_tree(&dir_a, &skill_dir_b.join(id.as_str()));
    assert!(matches!(read_err(&store_b, &id), LifecycleError::OwnerMismatch { .. }));
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("test: mkdir");
    private_dir(to);
    for item in fs::read_dir(from).expect("test: read_dir") {
        let item = item.expect("test: entry");
        let target = to.join(item.file_name());
        if item.file_type().expect("test: type").is_dir() {
            copy_tree(&item.path(), &target);
        } else {
            fs::copy(item.path(), &target).expect("test: copy");
        }
    }
}

#[test]
fn identity_of_foreign_skills_cannot_be_swapped_by_moving_directories() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    let other = SkillId::parse("other-skill").expect("test: id");
    let other_dir = store.root().join("revisions").join(other.as_str());
    fs::create_dir_all(&other_dir).expect("test: mkdir");
    copy_tree(&dir, &other_dir.join(id.as_str()));
    let err = store.read_revision(&other, &id, None).expect_err("test: moved record");
    assert!(matches!(err, LifecycleError::Tampered { .. }), "{err}");
}

#[test]
fn identifiers_reject_traversal_and_malformed_forms() {
    for bad in ["", "..", "../x", "a/b", "A", "-x", "a.b", "x y"] {
        assert!(SkillId::parse(bad).is_err(), "{bad:?}");
    }
    assert!(SkillId::parse(&"a".repeat(65)).is_err());
    assert!(SkillId::parse(SkillId::generate().as_str()).is_ok());
    for bad in [
        "",
        "r1-",
        "r1-ABC",
        "../r1",
        &format!("r1-{}", "g".repeat(64)),
        &format!("r2-{}", "0".repeat(64)),
    ] {
        assert!(RevisionId::parse(bad).is_err(), "{bad:?}");
    }
    assert!(serde_json::from_str::<SkillId>("\"../x\"").is_err());
    assert!(serde_json::from_str::<RevisionId>("\"nope\"").is_err());
    assert!(OwnerScope::new("").is_err());
    assert!(OwnerScope::new(" padded").is_err());
    assert!(OwnerScope::new("a\nb").is_err());
    assert!(serde_json::from_str::<OwnerScope>("\"\"").is_err());
}

#[test]
fn revision_record_rejects_unknown_fields() {
    let (_tmp, store) = setup();
    let (id, dir) = publish(&store, basic_entries());
    let path = dir.join("revision.json");
    let text = fs::read_to_string(&path).expect("test: record");
    let parsed: SkillRevision = serde_json::from_str(&text).expect("test: parse");
    assert_eq!(parsed.revision_id, id);
    let injected = text.replacen('{', "{\"trusted\": true,", 1);
    assert!(serde_json::from_str::<SkillRevision>(&injected).is_err());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).expect("test: stat").permissions().mode() & 0o777
    }

    #[test]
    fn store_and_revision_permissions_are_owner_only_and_recorded() {
        let (_tmp, store) = setup();
        let (id, dir) = publish(&store, basic_entries());
        let mut paths = vec![
            store.root().to_path_buf(),
            store.root().join("schema.json"),
            dir.clone(),
            dir.join("revision.json"),
            dir.join("bundle"),
            dir.join("bundle/references"),
            dir.join("bundle/SKILL.md"),
            dir.join("bundle/scripts/run.sh"),
        ];
        paths.push(store.root().join("revisions"));
        for path in paths {
            assert_eq!(mode(&path) & 0o077, 0, "{} must be owner-only", path.display());
        }
        assert_eq!(mode(&dir.join("bundle/scripts/run.sh")) & 0o100, 0o100);
        assert_eq!(mode(&dir.join("bundle/SKILL.md")) & 0o111, 0);
        let record = store.read_revision(&skill(), &id, None).expect("test: read").revision;
        assert_eq!(record.recorded_dir_mode, Some(mode(&dir)));
        assert_eq!(record.recorded_dir_mode, Some(0o700));
    }

    #[test]
    fn loosened_permissions_are_refused() {
        let (tmp, store) = setup();
        let (id, dir) = publish(&store, basic_entries());
        fs::set_permissions(dir.join("bundle/SKILL.md"), fs::Permissions::from_mode(0o644)).expect("test: chmod");
        assert!(matches!(
            read_err(&store, &id),
            LifecycleError::PermissionsTooOpen { .. }
        ));
        fs::set_permissions(dir.join("bundle/SKILL.md"), fs::Permissions::from_mode(0o600)).expect("test: chmod");
        store.read_revision(&skill(), &id, None).expect("test: restored");

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("test: chmod");
        assert!(matches!(
            read_err(&store, &id),
            LifecycleError::PermissionsTooOpen { .. }
        ));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("test: chmod");

        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o755)).expect("test: chmod");
        let err = SkillLifecycleStore::open(tmp.path(), owner(OWNER_A)).expect_err("test: open must refuse");
        assert!(matches!(err, LifecycleError::PermissionsTooOpen { .. }), "{err}");
        fs::set_permissions(store.root(), fs::Permissions::from_mode(0o700)).expect("test: chmod");
    }

    #[test]
    fn symlinked_store_components_are_refused() {
        let tmp = TempDir::new().expect("test: tempdir");
        let victim = TempDir::new().expect("test: tempdir");
        open(victim.path(), OWNER_A);
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).expect("test: mkdir");
        std::os::unix::fs::symlink(victim.path().join("self"), workspace.join("self")).expect("test: symlink");
        let err = SkillLifecycleStore::open(&workspace, owner(OWNER_A)).expect_err("test: symlinked self");
        assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");

        fs::remove_file(workspace.join("self")).expect("test: rm");
        fs::create_dir_all(workspace.join("self")).expect("test: mkdir");
        std::os::unix::fs::symlink(
            victim.path().join("self/skill-lifecycle"),
            workspace.join("self/skill-lifecycle"),
        )
        .expect("test: symlink");
        let err = SkillLifecycleStore::open(&workspace, owner(OWNER_A)).expect_err("test: symlinked store");
        assert!(matches!(err, LifecycleError::UnsafeStorePath { .. }), "{err}");
    }

    #[test]
    fn symlinked_revision_directory_and_stored_symlinks_are_refused() {
        let (_tmp, store) = setup();
        let (id, dir) = publish(&store, basic_entries());
        let moved = dir.with_file_name("moved");
        fs::rename(&dir, &moved).expect("test: rename");
        std::os::unix::fs::symlink(&moved, &dir).expect("test: symlink");
        assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
        fs::remove_file(&dir).expect("test: rm");
        fs::rename(&moved, &dir).expect("test: rename back");
        store.read_revision(&skill(), &id, None).expect("test: restored");

        fs::remove_file(dir.join("bundle/references/guide.md")).expect("test: rm");
        std::os::unix::fs::symlink(dir.join("bundle/SKILL.md"), dir.join("bundle/references/guide.md"))
            .expect("test: symlink");
        assert!(matches!(
            read_err(&store, &id),
            LifecycleError::Tampered { .. } | LifecycleError::UnsafeStorePath { .. }
        ));
    }

    #[test]
    fn flipping_the_exec_bit_of_a_stored_script_is_detected() {
        let (_tmp, store) = setup();
        let (id, dir) = publish(&store, basic_entries());
        fs::set_permissions(dir.join("bundle/scripts/run.sh"), fs::Permissions::from_mode(0o600)).expect("test: chmod");
        assert!(matches!(read_err(&store, &id), LifecycleError::Tampered { .. }));
    }

    #[test]
    fn write_uses_the_owner_workspace_only_and_never_creates_brain_db() {
        let (tmp, store) = setup();
        publish(&store, vec![entry("SKILL.md", md("S", "d").as_bytes())]);
        assert!(
            store
                .root()
                .starts_with(tmp.path().canonicalize().expect("test: canonical"))
        );
        assert!(!tmp.path().join("brain.db").exists());
        assert!(!tmp.path().join("skills").exists());
    }
}
