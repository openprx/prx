//! Pure unit tests for identifier validation and small helpers.
use crate::memory::postgres::PostgresMemory;
use crate::memory::postgres::rows::{related_table_name, validate_identifier};
use crate::memory::traits::MemoryCategory;

#[test]
fn valid_identifiers_pass_validation() {
    assert!(validate_identifier("public", "schema").is_ok());
    assert!(validate_identifier("_memories_01", "table").is_ok());
}

#[test]
fn invalid_identifiers_are_rejected() {
    assert!(validate_identifier("", "schema").is_err());
    assert!(validate_identifier("1bad", "schema").is_err());
    assert!(validate_identifier("bad-name", "table").is_err());
}

#[test]
fn identifier_length_limit_enforced() {
    // Exactly 63 chars — valid
    let max_len = "a".repeat(63);
    assert!(validate_identifier(&max_len, "table").is_ok());

    // 64 chars — exceeds PostgreSQL NAMEDATALEN-1 limit
    let too_long = "a".repeat(64);
    assert!(validate_identifier(&too_long, "table").is_err());

    // SQL injection attempts
    assert!(validate_identifier("table; DROP TABLE users", "table").is_err());
    assert!(validate_identifier("table\"--", "table").is_err());
    assert!(validate_identifier("table\x00name", "table").is_err());
}

#[test]
fn related_table_name_adds_suffix_with_identifier_limit() {
    assert_eq!(related_table_name("memories", "_drafts").unwrap(), "memories_drafts");

    let max_len = "a".repeat(63);
    let related = related_table_name(&max_len, "_drafts").unwrap();
    assert_eq!(related.len(), 63);
    assert!(related.ends_with("_drafts"));
}

#[test]
fn parse_category_maps_known_and_custom_values() {
    assert_eq!(PostgresMemory::parse_category("core"), MemoryCategory::Core);
    assert_eq!(PostgresMemory::parse_category("daily"), MemoryCategory::Daily);
    assert_eq!(
        PostgresMemory::parse_category("conversation"),
        MemoryCategory::Conversation
    );
    assert_eq!(
        PostgresMemory::parse_category("custom_notes"),
        MemoryCategory::Custom("custom_notes".into())
    );
}

#[test]
fn pgvector_literal_rejects_empty_or_non_finite_vectors() {
    assert_eq!(
        PostgresMemory::pgvector_literal(&[0.1, 0.2, 0.3]).as_deref(),
        Some("[0.1,0.2,0.3]")
    );
    assert!(PostgresMemory::pgvector_literal(&[]).is_none());
    assert!(PostgresMemory::pgvector_literal(&[f32::NAN]).is_none());
    assert!(PostgresMemory::pgvector_literal(&[f32::INFINITY]).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn new_does_not_panic_inside_tokio_runtime() {
    let outcome = std::panic::catch_unwind(|| {
        PostgresMemory::new(
            "postgres://openprx:password@127.0.0.1:1/openprx",
            "public",
            "memories",
            Some(1),
        )
    });

    assert!(outcome.is_ok(), "PostgresMemory::new should not panic");
    assert!(
        outcome.unwrap().is_err(),
        "PostgresMemory::new should return a connect error for an unreachable endpoint"
    );
}
