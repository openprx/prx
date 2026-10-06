//! The hosted TypeSafe System One service (CDP 4.3 item 3; SP-CDP-A 5.9;
//! CDP-00B `DIFFS.md` D-01, D-13, D-15, D-16).
//!
//! TypeSafe serves the System One wire itself, so the adapter is the
//! compatible transport ([`super::compatible_http::CompatibleHttpBackend`])
//! with these differences:
//!
//! - the base URL defaults to [`DEFAULT_BASE`]; `causal_tree.backend.endpoint`
//!   overrides it (COORD-6), and the backend stays remote whatever the URL
//!   says, so `remote_enabled` gates it even against a loopback stub;
//! - the model is pinned: the response must name exactly the configured
//!   model, or `pinned_snapshot` when one is set
//!   (`ModelVersionPolicy::ExactPinned`, D-043 C-8);
//! - the provider request id comes from the [`REQUEST_ID_HEADER`] header;
//! - the credential is an auth-profile reference owned by
//!   `decision:typesafe` (D-040 OQ-A4, D-074), sent as a bearer token.
//!
//! It never goes through a chat provider (AG-03).

/// The adapter's name on the wire envelope.
pub const NAME: &str = "typesafe";

/// The service's base URL; the System One route is appended.
pub const DEFAULT_BASE: &str = "https://api.typesafe.ai";

/// The System One route under the base URL.
pub const ROUTE: &str = "/v1/systemone";

/// The response header that carries TypeSafe's request id (`req_` and 32
/// hex digits).
pub const REQUEST_ID_HEADER: &str = "x-typesafe-request-id";
