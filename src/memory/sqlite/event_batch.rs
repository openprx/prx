//! Bounded event batches with per-input isolation and one durable commit.
use super::{MessageEvent, MessageEventInput, SqliteMemory};
use crate::memory::traits::MESSAGE_EVENT_BATCH_LIMIT;

// Bound background write amplification across independent workspaces without
// putting selected reads, scalar/Applied writes or accounting behind this gate.
static BACKGROUND_BATCHES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

impl SqliteMemory {
    pub(super) async fn append_event_batch(&self, inputs: Vec<MessageEventInput>) -> Vec<anyhow::Result<MessageEvent>> {
        let count = inputs.len();
        if count == 0 {
            return Vec::new();
        }
        if count > MESSAGE_EVENT_BATCH_LIMIT {
            return inputs
                .iter()
                .map(|_| Err(anyhow::anyhow!("message event batch exceeds capacity")))
                .collect();
        }
        let permit = match BACKGROUND_BATCHES.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                return (0..count)
                    .map(|_| Err(anyhow::anyhow!("message event batch admission failed: {error}")))
                    .collect();
            }
        };
        let pool = self.pool.clone();
        let result =
            crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<anyhow::Result<MessageEvent>>> {
                // The blocking owner retains admission through the real
                // commit even if its async caller is cancelled.
                let _permit = permit;
                let connection = pool.write();
                let tx = connection.unchecked_transaction()?;
                let mut results = Vec::with_capacity(count);
                for input in inputs {
                    if let Err(error) = input.validate() {
                        results.push(Err(error));
                        continue;
                    }
                    tx.execute_batch("SAVEPOINT message_event_batch_item;")?;
                    match Self::append_event_row(&tx, input, false) {
                        Ok((event, _)) => {
                            tx.execute_batch("RELEASE message_event_batch_item;")?;
                            results.push(Ok(event));
                        }
                        Err(error) => {
                            tx.execute_batch(
                                "ROLLBACK TO message_event_batch_item; RELEASE message_event_batch_item;",
                            )?;
                            results.push(Err(error));
                        }
                    }
                }
                tx.commit()?;
                Ok(results)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(std::convert::identity);
        match result {
            Ok(results) => results,
            Err(error) => (0..count)
                .map(|_| Err(anyhow::anyhow!("message event batch storage failed: {error}")))
                .collect(),
        }
    }
}
