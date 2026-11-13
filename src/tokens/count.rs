//! Token measurement shared by the agent loop and the decision engine.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenMeasurementSource {
    /// OpenAI cl100k is a stable local proxy, not the active model's tokenizer.
    Cl100kProxy,
    Heuristic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TokenMeasurement {
    pub tokens: usize,
    pub source: TokenMeasurementSource,
}

impl TokenMeasurement {
    /// A measurement of nothing. Attributed to the proxy tokenizer so an absent
    /// request component never drags the whole estimate onto the heuristic
    /// budget multiplier.
    pub(crate) const NONE: Self = Self {
        tokens: 0,
        source: TokenMeasurementSource::Cl100kProxy,
    };
}

/// Count the number of tokens in `text`.
///
/// This uses the `cl100k_base`
/// encoding (the tokenizer used by GPT-3.5/4 and a close proxy for most modern
/// chat models), giving an accurate count. The encoder is expensive to build,
/// so it is constructed once and cached in a [`std::sync::LazyLock`].
///
/// This compatibility helper is used by tests. Production callers measure
/// through `count_tokens_with_source`, which falls back to the `chars / 4`
/// heuristic (rounded up) only if the encoder fails to initialize.
#[cfg(test)]
pub fn count_tokens(text: &str) -> usize {
    count_tokens_with_source(text).tokens
}

#[cfg(test)]
pub(crate) mod token_timing;

pub(crate) fn count_tokens_with_source(text: &str) -> TokenMeasurement {
    #[cfg(test)]
    let _whole_timing = token_timing::Span::start(token_timing::Kind::Whole);
    {
        use std::sync::LazyLock;
        use tiktoken_rs::CoreBPE;

        static ENCODER: LazyLock<Option<CoreBPE>> = LazyLock::new(|| {
            #[cfg(test)]
            let _init_timing = token_timing::Span::start(token_timing::Kind::Init);
            match tiktoken_rs::cl100k_base() {
                Ok(bpe) => Some(bpe),
                Err(err) => {
                    tracing::warn!("tiktoken cl100k_base init failed, falling back to char heuristic: {err}");
                    None
                }
            }
        });

        #[cfg(test)]
        let lookup_timing = token_timing::Span::start(token_timing::Kind::Lookup);
        let encoder = ENCODER.as_ref();
        #[cfg(test)]
        drop(lookup_timing);
        if let Some(bpe) = encoder {
            return TokenMeasurement {
                tokens: {
                    #[cfg(test)]
                    let _encode_timing = token_timing::Span::start(token_timing::Kind::Encode);
                    bpe.encode_with_special_tokens(text).len()
                },
                source: TokenMeasurementSource::Cl100kProxy,
            };
        }
    }

    // Heuristic fallback: ~4 characters per token, rounding up so non-empty
    // text never reports zero tokens.
    TokenMeasurement {
        tokens: text.chars().count().div_ceil(4),
        source: TokenMeasurementSource::Heuristic,
    }
}
