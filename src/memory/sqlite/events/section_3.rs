#[cfg(test)]
fn hold_paths_match(left: &std::path::Path, right: &std::path::Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
pub(super) fn hold_append_snapshot_for_test(
    input: &MessageEventInput,
    db_path: &std::path::Path,
) -> anyhow::Result<()> {
    let mut slot = APPEND_SNAPSHOT_HOLD.lock();
    let matched = slot.as_ref().is_some_and(|hold| {
        input.run_id.as_deref() == Some(hold.run_id.as_str())
            && input.idempotency_key.as_deref() == Some(hold.idempotency_key.as_str())
            && hold_paths_match(&hold.db_path, db_path)
    });
    if !matched {
        return Ok(());
    }
    let hold = slot
        .take()
        .ok_or_else(|| anyhow::anyhow!("test: snapshot hold vanished after identity match"))?;
    drop(slot);
    let _ = hold.entered.send(());
    let _ = hold.release.recv_timeout(std::time::Duration::from_secs(60));
    Ok(())
}
