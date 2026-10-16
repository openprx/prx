#![cfg(test)]

use super::{input_token_preflight, input_token_upper_bound};
use crate::agent::context_budget::{
    TokenMeasurement, TokenMeasurementSource, count_tokens_with_source, estimate_request_tokens,
};

fn estimate(text: &str) -> usize {
    estimate_request_tokens(count_tokens_with_source(text), TokenMeasurement::NONE)
}

#[test]
fn sufficient_bound_preserves_actual_proxy_and_heuristic_admission() -> anyhow::Result<()> {
    let repeated = "the ".repeat(256);
    for text in [
        "",
        "a",
        "abcd",
        "abcde",
        "\u{03bb}\u{20ac}\u{1f600}",
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
        let upper = input_token_upper_bound(text);
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
                    input_token_preflight(text, allowance, || measured)?,
                    u64::try_from(measured)? < allowance
                );
            }
        }
    }
    Ok(())
}

#[test]
fn sufficient_bound_skips_only_proxy_work_and_equality_falls_back() -> anyhow::Result<()> {
    let calls = std::cell::Cell::new(0);
    let measured = || {
        calls.set(calls.get() + 1);
        1
    };
    assert_eq!(input_token_upper_bound("a"), 2);
    assert!(input_token_preflight("a", 3, measured)?);
    assert_eq!(calls.get(), 0, "strict sufficient bound skips the estimator");
    assert!(input_token_preflight("a", 2, measured)?);
    assert_eq!(calls.get(), 1, "equal upper bound is inconclusive, not rejection");
    assert!(!input_token_preflight("a", 1, measured)?);
    assert_eq!(calls.get(), 2, "original strict predicate still rejects equality");
    let ordinary = r#"{"state":"ordinary decision request"}"#;
    let ordinary_allowance = u64::try_from(input_token_upper_bound(ordinary).saturating_add(1))?;
    assert!(input_token_preflight(ordinary, ordinary_allowance, measured)?);
    assert_eq!(
        calls.get(),
        2,
        "a conclusive bound skips the proxy for an ordinary decision request"
    );
    assert!(!input_token_preflight("", 0, || 0)?);
    Ok(())
}

#[test]
fn large_compressible_body_keeps_original_estimator_acceptance() -> anyhow::Result<()> {
    let text = "the ".repeat(256);
    let measured = estimate(&text);
    let upper = input_token_upper_bound(&text);
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
    assert!(input_token_preflight(&text, allowance, || {
        calls.set(calls.get() + 1);
        estimate(&text)
    })?);
    assert_eq!(calls.get(), 1);
    Ok(())
}
