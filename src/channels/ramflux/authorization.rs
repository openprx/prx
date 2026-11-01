//! Effective autonomy for the ramflux channel.
//!
//! prx config alone never produces `full`. `follow_rf` and `full` both stay
//! supervised until `bridge.hello` declares `authorization = full`, and a
//! configured `supervised` mode ignores a host declaration of `full`.

use super::protocol::HelloAuthorization;
use crate::config::{RamfluxAuthorizationMode, RamfluxConfig};
use parking_lot::Mutex;
use std::sync::Arc;

/// Autonomy the tool loop actually runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveAutonomy {
    Full,
    Supervised,
}

impl EffectiveAutonomy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Supervised => "supervised",
        }
    }
}

/// Combine the operator's mode with the latest hello.
#[must_use]
pub const fn resolve(mode: RamfluxAuthorizationMode, hello: Option<HelloAuthorization>) -> EffectiveAutonomy {
    match (mode, hello) {
        (RamfluxAuthorizationMode::Supervised, _) => EffectiveAutonomy::Supervised,
        (_, Some(HelloAuthorization::Full)) => EffectiveAutonomy::Full,
        (_, Some(HelloAuthorization::Supervised) | None) => EffectiveAutonomy::Supervised,
    }
}

/// Label shown by `prx doctor`, for example `follow_rf→supervised`.
#[must_use]
pub fn doctor_label(mode: RamfluxAuthorizationMode, hello: Option<HelloAuthorization>) -> String {
    format!("{}→{}", mode.as_str(), resolve(mode, hello).as_str())
}

/// Atomic view of the configured mode and the latest handshake.
#[derive(Debug)]
pub struct Authority {
    mode: RamfluxAuthorizationMode,
    hello: Mutex<Option<HelloAuthorization>>,
}

impl Authority {
    #[must_use]
    pub fn from_config(config: &RamfluxConfig) -> Arc<Self> {
        Arc::new(Self {
            mode: config.authorization_mode,
            hello: Mutex::new(None),
        })
    }

    #[must_use]
    pub const fn mode(&self) -> RamfluxAuthorizationMode {
        self.mode
    }

    pub fn set_hello(&self, hello: HelloAuthorization) {
        *self.hello.lock() = Some(hello);
    }

    /// Forget the host's authorization. Effective mode falls back until the next hello.
    pub fn clear_hello(&self) {
        *self.hello.lock() = None;
    }

    #[must_use]
    pub fn hello(&self) -> Option<HelloAuthorization> {
        *self.hello.lock()
    }

    #[must_use]
    pub fn effective(&self) -> EffectiveAutonomy {
        resolve(self.mode, self.hello())
    }

    #[must_use]
    pub fn label(&self) -> String {
        doctor_label(self.mode(), self.hello())
    }
}
