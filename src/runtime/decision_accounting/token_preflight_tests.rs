#![cfg(test)]

use super::{auxiliary_input_upper_bound, input_token_preflight};
use crate::agent::context_budget::{
    TokenMeasurement, TokenMeasurementSource, count_tokens_with_source, estimate_request_tokens,
};

fn estimate(text: &str) -> usize {
    estimate_request_tokens(count_tokens_with_source(text), TokenMeasurement::NONE)
}

#[test]
fn auxiliary_early_pass_preserves_actual_proxy_and_heuristic_admission() -> anyhow::Result<()> {
    let repeated = "the ".repeat(256);
    for text in [
        "",
        "a",
        "abcd",
        "abcde",
        "\u{03bb}\u{4e2d}\u{1f600}",
        "<|endoftext|>",
        r#"{"input":"project text","image":"AAAA=="}"#,
        &repeated,
    ] {
        let actual = estimate(text);
        let heuristic = estimate_request_tokens(
            TokenMeasurement {
                tokens: text.chars().count().div_ceil(4),
                source: TokenMeasurementSource::Heuristic,
            },
            TokenMeasurement::NONE,
        );
        let upper = auxiliary_input_upper_bound(text);
        assert!(actual <= upper && heuristic <= upper);
        for measured in [actual, heuristic] {
            for allowance in [
                0,
                1,
                2,
                measured.saturating_sub(1),
                measured,
                measured.saturating_add(1),
                upper,
                upper.saturating_add(1),
            ] {
                let allowance = u64::try_from(allowance)?;
                assert_eq!(
                    input_token_preflight(text, allowance, true, || measured)?,
                    u64::try_from(measured)? < allowance
                );
            }
        }
    }
    Ok(())
}

#[test]
fn auxiliary_sufficient_bound_skips_only_proxy_work_and_equality_falls_back() -> anyhow::Result<()> {
    let calls = std::cell::Cell::new(0);
    let measured = || {
        calls.set(calls.get() + 1);
        1
    };
    assert_eq!(auxiliary_input_upper_bound("a"), 2);
    assert!(input_token_preflight("a", 3, true, measured)?);
    assert_eq!(calls.get(), 0, "strict sufficient bound skips the estimator");
    assert!(input_token_preflight("a", 2, true, measured)?);
    assert_eq!(calls.get(), 1, "equal upper bound is inconclusive, not rejection");
    assert!(!input_token_preflight("a", 1, true, measured)?);
    assert_eq!(calls.get(), 2, "original strict predicate still rejects equality");
    assert!(input_token_preflight("a", 3, false, measured)?);
    assert_eq!(calls.get(), 3, "ordinary decision calls retain original estimator");
    assert!(!input_token_preflight("", 0, true, || 0)?);
    Ok(())
}

#[test]
fn auxiliary_large_compressible_body_keeps_original_estimator_acceptance() -> anyhow::Result<()> {
    let text = "the ".repeat(256);
    let measured = estimate(&text);
    let upper = auxiliary_input_upper_bound(&text);
    let allowance = u64::try_from(
        measured
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("fixture overflow"))?,
    )?;
    assert!(
        u64::try_from(upper)? >= allowance,
        "fixture requires the inconclusive branch"
    );
    let calls = std::cell::Cell::new(0);
    assert!(input_token_preflight(&text, allowance, true, || {
        calls.set(calls.get() + 1);
        estimate(&text)
    })?);
    assert_eq!(calls.get(), 1);
    Ok(())
}
