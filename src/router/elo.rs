use anyhow::Result;
use skillratings::Outcomes;
use skillratings::elo::{EloConfig, EloRating, elo};

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
    use crate::memory::none::NoneMemory;

    #[test]
    fn elo_update_equal_ratings() {
        let (winner, loser) = update_elo(1_000.0, 1_000.0);
        assert!(winner > 1_000.0);
        assert!(loser < 1_000.0);
        // Symmetric: winner gain ≈ loser loss
        let gain = winner - 1_000.0;
        let loss = 1_000.0 - loser;
        assert!((gain - loss).abs() < 1.0);
    }

    #[test]
    fn elo_update_higher_rated_winner_gains_less() {
        let (w1, _) = update_elo(1_500.0, 1_000.0); // strong beats weak
        let (w2, _) = update_elo(1_000.0, 1_500.0); // weak beats strong
        let gain_strong = w1 - 1_500.0;
        let gain_weak = w2 - 1_000.0;
        assert!(gain_weak > gain_strong, "upset should yield larger ELO gain");
    }

    #[test]
    fn elo_update_preserves_total_rating() {
        let initial_sum = 1_200.0 + 800.0;
        let (winner, loser) = update_elo(1_200.0, 800.0);
        let final_sum = winner + loser;
        assert!(
            (final_sum - initial_sum).abs() < 1.0,
            "total rating should be conserved"
        );
    }

    #[test]
    fn elo_update_zero_ratings() {
        // Edge: both at 0
        let (winner, loser) = update_elo(0.0, 0.0);
        assert!(winner > 0.0);
        assert!(loser < 0.0);
    }

    #[test]
    fn elo_update_very_high_ratings() {
        let (winner, loser) = update_elo(3_000.0, 3_000.0);
        assert!(winner > 3_000.0);
        assert!(loser < 3_000.0);
    }

    #[test]
    fn elo_key_format() {
        assert_eq!(elo_key("gpt-4"), "router/elo/gpt-4");
    }

    #[tokio::test]
    async fn load_elo_missing_returns_default() {
        let memory = NoneMemory;
        let rating = load_elo("nonexistent", &memory).await;
        assert!((rating - 1_000.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn persist_elo_does_not_panic() {
        let memory = NoneMemory;
        let result = persist_elo("gpt-4", 1_234.5, &memory).await;
        assert!(result.is_ok());
    }
}
