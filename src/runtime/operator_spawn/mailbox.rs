//! The mailbox source shares child lifecycle mechanics, never operator provenance.
use super::{Arc, AtomicBool, CancellationToken, ChildLaunchSource, OwnedTurnSourceProof};
impl ChildLaunchSource {
    pub(crate) async fn from_mailbox(
        proof: &crate::runtime::mailbox_child::MailboxChildProof,
        publication: Arc<crate::runtime::turn_publication::TurnRuntimeService>,
        provider: Arc<dyn crate::providers::Provider>,
        resolved: crate::providers::catalog::ResolvedModel,
        route: Option<crate::providers::catalog::RouteTarget>,
        record: crate::memory::child_sessions::ChildSessionRecord,
        input: crate::messaging::runtime::MessageInput,
        live_turn: Option<crate::agent::live_frames::LiveTurnRef>,
    ) -> anyhow::Result<Self> {
        let generation = Arc::clone(&publication.generation);
        let fabric = crate::memory::MemoryFabric::new(Arc::clone(&publication.memory), &record.workspace)
            .with_event_recording(generation.effective.memory.event_recording_config());
        let security = Arc::clone(&publication.security);
        let publication = crate::runtime::child_source::publication::IndependentChildPublication::capture(
            Arc::clone(&publication),
            Arc::clone(publication.runtime_holder()),
            proof,
            &generation,
            &fabric.memory(),
            &security,
        )?;
        let source = Self {
            live_turn,
            message_event: None,
            message_permit: None,
            message_input: None,
            continuation: None,
            proof: OwnedTurnSourceProof::capture(proof),
            catalog: crate::providers::selection::catalog_for(&generation.effective),
            generation,
            fabric,
            provider,
            resolved,
            route,
            security,
            publication,
            task: input.text.clone(),
            claimed: AtomicBool::new(false),
            child_issued: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        };
        source.for_message(record, &input).await
    }
}
