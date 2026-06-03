//! Unified inbound side-effect gate for channel ingress.
//!
//! Wraps [`SideEffectGate`] so the three channel authorization points
//! (inbound / autosave / outbound) share one construction path and one
//! operation-naming convention, without altering deny semantics. The gate is a
//! thin wrapper that only returns `Result`: the deny-driven control flow
//! (inbound = whole-message return, autosave = skip-only, outbound = suppress
//! reply) stays at the callsite so the three distinct semantics are never
//! flattened here.
//!
//! The authorization backend is abstracted behind [`InboundAuthorizer`] so
//! tests can inject an operation-selective deny. The production
//! [`SideEffectGate`] only supports autonomy + rate gating, never
//! per-operation-name allow/deny (see
//! [`SecurityPolicy::enforce_tool_operation`]), so the trait seam is the only
//! place an op-selective deny can be expressed.

use crate::security::SideEffectGate;
use crate::security::policy::ResourceRiskLevel;
use crate::security::policy::SecurityPolicy;

/// Authorization backend for inbound side effects. The single method mirrors the
/// underlying [`SideEffectGate::authorize_resource_operation`] call so a test
/// double can deny a *specific* operation (e.g. autosave only) — something the
/// real [`SecurityPolicy`] cannot express (it gates by autonomy + rate only).
pub trait InboundAuthorizer {
    /// Authorize a single inbound resource operation. Returns `Ok(())` on allow
    /// and `Err(reason)` on deny.
    fn authorize(&self, tool_name: &str, operation_name: &str, risk: ResourceRiskLevel) -> Result<(), String>;
}

/// Blanket impl so a borrowed authorizer (e.g. `&dyn InboundAuthorizer` behind
/// an `Arc`) can drive an `InboundGate` without taking ownership. This lets the
/// channel test seam construct `InboundGate::new(arc.as_ref())`.
impl<T: InboundAuthorizer + ?Sized> InboundAuthorizer for &T {
    fn authorize(&self, tool_name: &str, operation_name: &str, risk: ResourceRiskLevel) -> Result<(), String> {
        (**self).authorize(tool_name, operation_name, risk)
    }
}

/// Production backend: delegates to the real [`SideEffectGate`], preserving
/// audit / re-entrancy / M5-replay behavior unchanged.
pub struct PolicyAuthorizer<'a> {
    security: &'a SecurityPolicy,
}

impl<'a> PolicyAuthorizer<'a> {
    #[must_use]
    pub const fn new(security: &'a SecurityPolicy) -> Self {
        Self { security }
    }
}

impl InboundAuthorizer for PolicyAuthorizer<'_> {
    fn authorize(&self, tool_name: &str, operation_name: &str, risk: ResourceRiskLevel) -> Result<(), String> {
        SideEffectGate::new(self.security)
            .authorize_resource_operation(tool_name, operation_name, risk, None)
            .map(|_| ())
    }
}

/// Unified inbound gate. Generic over the authorizer so production uses
/// [`PolicyAuthorizer`] (static dispatch via [`InboundGate::for_policy`]) and
/// tests can supply an operation-selective deny double.
pub struct InboundGate<A: InboundAuthorizer> {
    authorizer: A,
}

impl<A: InboundAuthorizer> InboundGate<A> {
    #[must_use]
    pub const fn new(authorizer: A) -> Self {
        Self { authorizer }
    }

    /// Authorize the inbound conversation-turn persistence.
    ///
    /// Operation name MUST stay `channel:{channel}:inbound:{sender}`.
    pub fn authorize_inbound(&self, channel: &str, sender: &str) -> Result<(), String> {
        self.authorizer.authorize(
            "channel",
            &format!("channel:{channel}:inbound:{sender}"),
            ResourceRiskLevel::Low,
        )
    }

    /// Authorize the per-message memory autosave side effect.
    ///
    /// Operation name MUST stay `channel:{channel}:autosave`.
    pub fn authorize_autosave(&self, channel: &str) -> Result<(), String> {
        self.authorizer.authorize(
            "channel",
            &format!("channel:{channel}:autosave"),
            ResourceRiskLevel::Low,
        )
    }

    /// Authorize driving the LLM loop + sending the reply (outbound).
    ///
    /// Operation name MUST stay `channel:{channel}:outbound`.
    pub fn authorize_outbound(&self, channel: &str) -> Result<(), String> {
        self.authorizer.authorize(
            "channel",
            &format!("channel:{channel}:outbound"),
            ResourceRiskLevel::Low,
        )
    }
}

impl<'a> InboundGate<PolicyAuthorizer<'a>> {
    /// Production constructor: static-dispatch gate backed by the real
    /// [`SideEffectGate`]. Callsites stay terse:
    /// `InboundGate::for_policy(&policy).authorize_inbound(..)`.
    #[must_use]
    pub const fn for_policy(security: &'a SecurityPolicy) -> Self {
        Self::new(PolicyAuthorizer::new(security))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    /// Test double that denies exactly the operation names containing the given
    /// needle and allows everything else, so the unit matrix can assert each of
    /// the three gate methods independently (op-selective deny — not expressible
    /// by the real policy).
    struct SelectiveDeny {
        deny_needle: &'static str,
    }

    impl InboundAuthorizer for SelectiveDeny {
        fn authorize(&self, _tool_name: &str, operation_name: &str, _risk: ResourceRiskLevel) -> Result<(), String> {
            if operation_name.contains(self.deny_needle) {
                Err(format!("denied: {operation_name}"))
            } else {
                Ok(())
            }
        }
    }

    /// Authorizer that records the exact operation names it was asked to
    /// authorize, so we can assert the op-name literals stay intact.
    struct RecordingAuthorizer {
        seen: parking_lot::Mutex<Vec<String>>,
    }

    impl InboundAuthorizer for RecordingAuthorizer {
        fn authorize(&self, _tool_name: &str, operation_name: &str, _risk: ResourceRiskLevel) -> Result<(), String> {
            self.seen.lock().push(operation_name.to_string());
            Ok(())
        }
    }

    #[test]
    fn deny_all_rejects_every_method() {
        let gate = InboundGate::new(SelectiveDeny {
            deny_needle: "channel:",
        });
        assert!(gate.authorize_inbound("tg", "alice").is_err());
        assert!(gate.authorize_autosave("tg").is_err());
        assert!(gate.authorize_outbound("tg").is_err());
    }

    #[test]
    fn deny_only_autosave() {
        let gate = InboundGate::new(SelectiveDeny {
            deny_needle: ":autosave",
        });
        assert!(
            gate.authorize_inbound("tg", "alice").is_ok(),
            "inbound must stay allowed"
        );
        assert!(gate.authorize_autosave("tg").is_err(), "autosave must be denied");
        assert!(gate.authorize_outbound("tg").is_ok(), "outbound must stay allowed");
    }

    #[test]
    fn deny_only_outbound() {
        let gate = InboundGate::new(SelectiveDeny {
            deny_needle: ":outbound",
        });
        assert!(
            gate.authorize_inbound("tg", "alice").is_ok(),
            "inbound must stay allowed"
        );
        assert!(gate.authorize_autosave("tg").is_ok(), "autosave must stay allowed");
        assert!(gate.authorize_outbound("tg").is_err(), "outbound must be denied");
    }

    #[test]
    fn operation_names_are_literal() {
        let gate = InboundGate::new(RecordingAuthorizer {
            seen: parking_lot::Mutex::new(Vec::new()),
        });
        let _ = gate.authorize_inbound("telegram", "bob");
        let _ = gate.authorize_autosave("telegram");
        let _ = gate.authorize_outbound("telegram");
        let seen = gate.authorizer.seen.lock().clone();
        assert_eq!(
            seen,
            vec![
                "channel:telegram:inbound:bob".to_string(),
                "channel:telegram:autosave".to_string(),
                "channel:telegram:outbound".to_string(),
            ]
        );
    }

    #[test]
    fn for_policy_allows_under_supervised() {
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let gate = InboundGate::for_policy(&policy);
        assert!(gate.authorize_inbound("tg", "alice").is_ok());
        assert!(gate.authorize_autosave("tg").is_ok());
        assert!(gate.authorize_outbound("tg").is_ok());
    }

    #[test]
    fn for_policy_denies_under_readonly() {
        let policy = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        let gate = InboundGate::for_policy(&policy);
        assert!(gate.authorize_inbound("tg", "alice").is_err());
        assert!(gate.authorize_autosave("tg").is_err());
        assert!(gate.authorize_outbound("tg").is_err());
    }
}
