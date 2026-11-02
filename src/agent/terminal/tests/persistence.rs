#![cfg(test)]

use super::*;

#[tokio::test]
async fn float_cost_receipts_verify_through_the_real_store() {
    for (run, cost) in [
        ("prx-float-receipt", 0.002_917_600_000_000_000_2_f64),
        ("prx-stable-receipt", 0.000_1_f64),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let fabric = MemoryFabric::new(memory.clone(), temp.path().to_string_lossy());
        let mut prices = CostConfig::default();
        prices.prices.insert(
            "ollama/prx-local-model".into(),
            crate::config::schema::ModelPricing {
                input: cost * 1_000_000.0 / 10.0,
                output: 0.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let mut commit = completed_commit(run);
        let service = bind_terminal_fixture(temp.path(), memory, &mut commit, &prices).await;
        // The real fixture reports ten prompt tokens and five completion tokens.
        let link = commit.decision.as_ref().unwrap().clone();
        let receipt = finalize_turn(&fabric, commit, &prices, temp.path()).await.unwrap();
        assert_eq!(receipt.usage_settlement.as_ref().unwrap().cost_usd, Some(cost));
        let stored = fabric
            .memory()
            .find_owned_message_event(
                fabric.workspace_id(),
                receipt.terminal_event.owner_id.as_deref().unwrap(),
                &receipt.terminal_event.event_id,
            )
            .await
            .unwrap()
            .unwrap();
        let recorded = link.recorded.as_ref().unwrap();
        let verified = verify_terminal_replay(
            fabric.memory().as_ref(),
            &recorded.reader,
            &recorded.key,
            &recorded.store_workspace_id,
            fabric.workspace_id(),
            stored.owner_id.as_deref().unwrap(),
            run,
            &stored,
        )
        .await
        .unwrap();
        assert_eq!(verified.source().run_id.as_deref(), Some(run));
        let payload: serde_json::Value = serde_json::from_str(stored.raw_payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload
                .pointer("/usage_settlement/cost_usd")
                .and_then(serde_json::Value::as_f64)
                .unwrap()
                .to_bits(),
            cost.to_bits()
        );
        service.shutdown(Instant::now() + Duration::from_secs(5)).await;
    }
}
