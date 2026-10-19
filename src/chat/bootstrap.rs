//! Chat startup helpers: dispatcher state, model switch, provider chain and banner.

use super::{session, state};
use crate::config::Config;
use crate::providers;
use crate::providers::Provider;
use anyhow::Result;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Seed the reducer with the exact durable chat session before any turn can be
/// admitted. This must also run for a brand-new session with zero turns: leaving
/// `ChatState::new`'s placeholder id in place lets the first Redux save rename
/// the conversation, which splits message-event provenance across session keys.
pub(super) fn initial_chat_dispatcher_state(
    provider_name: &str,
    model_name: &str,
    shutdown: CancellationToken,
    chat_session: &session::ChatSession,
) -> state::ChatState {
    let mut state = state::ChatState::new(Arc::from(provider_name), Arc::from(model_name), shutdown);
    let _ = state.reduce(crate::chat::action::Action::SessionLoaded(chat_session.clone()));
    state
}

/// Check a `/model <name>` switch on the current provider: resolved for chat
/// through the one selection check, so a model that cannot chat is refused
/// before the next turn uses it; an id-only model switches with a warning.
pub(super) fn chat_model_switch(config: &Config, provider_name: &str, model_name: &str) -> Result<()> {
    providers::selection::check(
        config,
        provider_name,
        model_name,
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    Ok(())
}

/// Build the chat provider chain for `provider_name`.
///
/// Shared by startup and `/provider`. The selection and every model route
/// are resolved for chat first, so a model that cannot chat is refused before
/// any provider exists. The root credential and base URL are released only
/// when the selection is the configured provider's identity (any spelling of
/// it, `providers::configured_connection_for`), and the rotation keys in
/// `reliability.api_keys` follow them, because they are extra credentials for
/// that same root key.
pub(crate) fn chat_provider_chain<'a>(
    config: &'a Config,
    provider_name: &str,
    configured_provider_name: &str,
    model_name: &str,
    options: &providers::ProviderRuntimeOptions,
) -> Result<(providers::ProviderConnection<'a>, Box<dyn Provider>)> {
    let catalog = providers::selection::catalog_for(config);
    let resolved = providers::selection::check_with(
        config,
        &catalog,
        provider_name,
        model_name,
        providers::selection::credential_for(config, provider_name),
        &providers::catalog::Requirement::chat(),
        providers::catalog::UnknownPolicy::Invocation,
    )?;
    let connection = providers::configured_connection_for(
        provider_name,
        configured_provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
    );
    let provider = providers::create_routed_provider_for(&resolved, &catalog, config, options)?;
    Ok((connection, provider))
}

/// The one-line banner every chat session opens with:
/// `prx <version> · provider/model`. A `custom:` URL may carry
/// `user:password@`, so the provider is printed through the one redaction
/// helper.
pub(super) fn startup_banner(provider_name: &str, model_name: &str) -> String {
    format!(
        "prx {} \u{00B7} {}/{model_name}",
        env!("CARGO_PKG_VERSION"),
        providers::redact_endpoint_userinfo(provider_name)
    )
}
