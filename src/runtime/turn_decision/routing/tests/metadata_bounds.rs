#![cfg(test)]
//! Damaged persisted rows still enter the genuine selected-read path.
use super::Fixture;
use rusqlite::types::Value;

async fn reject_column_value(column: &str, value: Value, expected: &str) -> anyhow::Result<()> {
    let mut fixture = Fixture::new().await;
    fixture.seed().await;
    let database = fixture.database();
    database.execute_batch("PRAGMA ignore_check_constraints=ON;")?;
    // Column names come only from the literal cases below; data stays bound.
    database.execute(&format!("UPDATE router_history_v1 SET {column}=?1"), [value])?;
    drop(database);
    fixture.assert_read_closed(expected).await;
    Ok(())
}

#[tokio::test]
async fn sqlite_router_guards_each_stored_text_field_before_materialization() -> anyhow::Result<()> {
    for column in [
        "terminal",
        "provider",
        "model",
        "workspace",
        "owner",
        "session",
        "embedding_model",
        "source_event",
    ] {
        reject_column_value(
            column,
            Value::Text("x".repeat(513)),
            "Router history metadata exceeds bounds",
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_router_rejects_damaged_binary_dimensions_versions_and_outcomes() -> anyhow::Result<()> {
    for column in ["policy", "fingerprint"] {
        reject_column_value(
            column,
            Value::Blob(vec![0; 31]),
            "Router history binary value exceeds bounds",
        )
        .await?;
    }
    for (column, value) in [("dimensions", 0), ("embedding_version", 2)] {
        reject_column_value(column, Value::Integer(value), "Router history stored domain conflicts").await?;
    }
    reject_column_value("success", Value::Integer(2), "invalid Router history outcome").await?;
    reject_column_value(
        "embedding",
        Value::Text("prx_bad_vector".to_owned()),
        "Router history binary value exceeds bounds",
    )
    .await?;
    reject_column_value(
        "embedding",
        Value::Blob(vec![0; 3]),
        "Router history vector exceeds its fixed encoding bounds",
    )
    .await?;
    Ok(())
}
