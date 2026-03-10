use anyhow::Result;
use skillratings::elo::{elo, EloConfig, EloRating};
use skillratings::Outcomes;

use crate::memory::{Memory, MemoryCategory};
use crate::self_system::SELF_SYSTEM_SESSION_ID;

fn elo_key(model_id: &str) -> String {
    format!("router/elo/{model_id}")
}

pub fn update_elo(winner_elo: f32, loser_elo: f32) -> (f32, f32) {
    let config = EloConfig::new();
    let winner = EloRating {
        rating: f64::from(winner_elo),
    };
    let loser = EloRating {
        rating: f64::from(loser_elo),
    };
    let (winner, loser) = elo(&winner, &loser, &Outcomes::WIN, &config);
    (winner.rating as f32, loser.rating as f32)
}

pub async fn persist_elo(model_id: &str, elo: f32, memory: &dyn Memory) -> Result<()> {
    memory
        .store(
            &elo_key(model_id),
            &elo.to_string(),
            MemoryCategory::Custom("router".to_string()),
            Some(SELF_SYSTEM_SESSION_ID),
        )
        .await
}

pub async fn load_elo(model_id: &str, memory: &dyn Memory) -> f32 {
    let key = elo_key(model_id);
    match memory.get(&key).await {
        Ok(Some(entry)) => {
            if let Ok(elo) = entry.content.parse::<f32>() {
                elo
            } else {
                serde_json::from_str::<serde_json::Value>(&entry.content)
                    .ok()
                    .and_then(|value| value.get("dynamic_elo").and_then(serde_json::Value::as_f64))
                    .map_or(1_000.0, |elo| elo as f32)
            }
        }
        Ok(None) | Err(_) => 1_000.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_elo_update() {
        let (winner, loser) = update_elo(1_000.0, 1_000.0);
        assert!(winner > 1_000.0);
        assert!(loser < 1_000.0);
    }
}
