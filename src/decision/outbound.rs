//! The outbound guard (SP-CDP-A 5.6 step 3; invariant 16.2; CDP-PLAN 1.5;
//! D-079 M-3 and L-8).
//!
//! Before any non-builtin attempt, the canonical System One body the
//! request encodes to is checked:
//!
//! - it fits the configured `max_request_bytes`, or the attempt is refused
//!   as [`RequestRule::RequestTooLarge`];
//! - no secret value of the live configuration and no resolved credential
//!   appears in it, raw or JSON-escaped, or the attempt is refused as
//!   [`PolicyRule::SecretInRequest`].
//!
//! A body that passes becomes a [`GuardedBody`], which only this module
//! builds; a backend sends exactly those bytes and never re-encodes. The
//! refusal names the rule only, never a value.

use super::error::{DecisionError, PolicyRule, RequestRule};

/// Registered secret values shorter than this are not scanned: a value of a
/// few characters matches ordinary text and would refuse every remote
/// attempt. Such a value is reported once per configuration generation.
pub const MIN_SCANNED_SECRET_CHARS: usize = 8;

/// Secret values no request may carry: every value `secret_paths` finds in
/// the live configuration and every resolved decision credential, each in
/// its raw and its JSON-escaped form. Built once per configuration
/// generation by the factory; `Debug` never prints a value.
pub struct OutboundSecrets {
    values: Vec<String>,
}

impl std::fmt::Debug for OutboundSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OutboundSecrets({} values, [REDACTED])", self.values.len())
    }
}

/// An empty set scans nothing, so it exists only for tests (D-082 N-2):
/// production builds its secrets from the live configuration.
#[cfg(test)]
impl Default for OutboundSecrets {
    fn default() -> Self {
        Self { values: Vec::new() }
    }
}

impl OutboundSecrets {
    /// The secret values of `config` (the `secret_paths` registry).
    #[must_use]
    pub fn of_config(config: &crate::config::Config) -> Self {
        match serde_json::to_value(config) {
            Ok(tree) => {
                let mut secrets = Self { values: Vec::new() };
                for (setting, value) in crate::config::secret_paths::scan(&tree).values_with_paths() {
                    secrets.add(setting, value);
                }
                secrets
            }
            // Serializing a loaded configuration cannot fail; if it ever
            // did, no value is known and every remote attempt is refused
            // rather than sent unchecked.
            Err(error) => {
                tracing::error!("decision outbound guard could not read the configuration's secrets: {error}");
                Self {
                    values: vec![String::new()],
                }
            }
        }
    }

    /// Add the secret value of `setting`, in its raw and JSON-escaped
    /// forms. A value shorter than [`MIN_SCANNED_SECRET_CHARS`] is not
    /// scanned, with a warning that names the setting and never the value
    /// (D-082 N-6).
    pub fn add(&mut self, setting: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        if value.chars().count() < MIN_SCANNED_SECRET_CHARS {
            tracing::warn!(
                setting = %setting,
                min_chars = MIN_SCANNED_SECRET_CHARS,
                "a secret is too short to be looked for in decision requests"
            );
            return;
        }
        if !self.values.iter().any(|known| known == value) {
            self.values.push(value.to_string());
        }
        let escaped = serde_json::to_string(value).unwrap_or_default();
        let escaped = escaped
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .unwrap_or_default();
        if !escaped.is_empty() && escaped != value && !self.values.iter().any(|known| known == escaped) {
            self.values.push(escaped.to_string());
        }
    }

    /// Whether `body` carries any of the values. An empty value stands for
    /// "the secrets could not be read" and matches every body.
    fn found_in(&self, body: &[u8]) -> bool {
        self.values
            .iter()
            .any(|value| value.is_empty() || body.windows(value.len()).any(|window| window == value.as_bytes()))
    }
}

/// A request body that passed the outbound guard: the exact bytes a backend
/// sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedBody {
    bytes: Vec<u8>,
}

impl GuardedBody {
    /// The bytes to send.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Check the encoded `body` of a request before it is sent.
///
/// # Errors
///
/// [`DecisionError::InvalidRequest`] with [`RequestRule::RequestTooLarge`]
/// when the body exceeds `max_request_bytes`, and
/// [`DecisionError::PolicyRejection`] with [`PolicyRule::SecretInRequest`]
/// when it carries a secret value.
pub(super) fn check_outbound(
    body: Vec<u8>,
    max_request_bytes: u32,
    secrets: &OutboundSecrets,
) -> Result<GuardedBody, DecisionError> {
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > u64::from(max_request_bytes) {
        return Err(DecisionError::InvalidRequest(RequestRule::RequestTooLarge));
    }
    if secrets.found_in(&body) {
        return Err(DecisionError::PolicyRejection(PolicyRule::SecretInRequest));
    }
    Ok(GuardedBody { bytes: body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(values: &[&str]) -> OutboundSecrets {
        let mut secrets = OutboundSecrets::default();
        for value in values {
            secrets.add("test.setting", value);
        }
        secrets
    }

    #[test]
    fn a_body_over_the_limit_or_with_a_secret_is_refused_without_the_value() {
        let secrets = secrets(&["sk-OUTBOUND-SENTINEL", ""]);
        let guarded = check_outbound(b"{\"state\":\"clean\"}".to_vec(), 64, &secrets).expect("test: clean");
        assert_eq!(guarded.bytes(), b"{\"state\":\"clean\"}");
        assert_eq!(
            check_outbound(vec![b'a'; 65], 64, &secrets),
            Err(DecisionError::InvalidRequest(RequestRule::RequestTooLarge))
        );
        let error = check_outbound(b"{\"state\":\"key sk-OUTBOUND-SENTINEL\"}".to_vec(), 1024, &secrets)
            .expect_err("test: a secret is refused");
        assert_eq!(error, DecisionError::PolicyRejection(PolicyRule::SecretInRequest));
        assert!(!error.to_string().contains("SENTINEL"));
        assert!(!format!("{secrets:?}").contains("SENTINEL"));
    }

    /// D-079 L-8: a secret with a quote or a backslash is found in its
    /// JSON-escaped form; a value under the minimum length is not scanned.
    #[test]
    fn escaped_secrets_are_found_and_short_values_are_not_scanned() {
        let secrets = secrets(&["pa\"ss\\word-SENTINEL", "abc"]);
        let body = serde_json::to_vec(&serde_json::json!({"state": "x pa\"ss\\word-SENTINEL x"})).expect("test: json");
        assert_eq!(
            check_outbound(body, 1024, &secrets),
            Err(DecisionError::PolicyRejection(PolicyRule::SecretInRequest))
        );
        assert!(check_outbound(b"{\"state\":\"abc abc\"}".to_vec(), 1024, &secrets).is_ok());
    }

    /// D-082 N-6: the warning for a value too short to scan names the
    /// setting that holds it and never the value.
    #[test]
    fn a_short_secret_is_reported_by_its_setting() {
        #[derive(Clone, Default)]
        struct Sink(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let mut config = crate::config::Config::default();
        config.api_key = Some("Zq7x".to_string());
        tracing::subscriber::with_default(subscriber, || {
            // The callsite interest cache is global: a rebuild racing on
            // another thread can leave this callsite disabled after ours, so
            // rebuild and try again until the warning is captured.
            for _ in 0..20 {
                tracing::callsite::rebuild_interest_cache();
                let _ = OutboundSecrets::of_config(&config);
                if !sink.0.lock().is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        });
        let logs = String::from_utf8_lossy(&sink.0.lock()).to_string();
        assert!(logs.contains("too short") && logs.contains("api_key"), "{logs}");
        assert!(!logs.contains("Zq7x"), "{logs}");
    }

    #[test]
    fn unreadable_secrets_refuse_every_body() {
        let secrets = OutboundSecrets {
            values: vec![String::new()],
        };
        assert!(check_outbound(b"{}".to_vec(), 1024, &secrets).is_err());
    }

    #[test]
    fn the_configuration_secrets_are_collected() {
        let mut config = crate::config::Config::default();
        config.api_key = Some("sk-ROOT-OUTBOUND-SENTINEL".to_string());
        let mut secrets = OutboundSecrets::of_config(&config);
        secrets.add("causal_tree.backend.credential_ref", "decision-profile-sentinel");
        assert!(check_outbound(b"x sk-ROOT-OUTBOUND-SENTINEL x".to_vec(), 1024, &secrets).is_err());
        assert!(check_outbound(b"x decision-profile-sentinel x".to_vec(), 1024, &secrets).is_err());
        assert!(check_outbound(b"x nothing x".to_vec(), 1024, &secrets).is_ok());
    }
}
