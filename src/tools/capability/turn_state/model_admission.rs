// Runtime-owned immutable model policy. As with the accounting contract, the
// seal permits reviewed crate producers, never external/backend payloads.
pub(crate) mod model_admission_sealed {
    pub(crate) trait Producer {}
}

#[derive(Clone, Copy)]
pub(crate) struct ModelAdmissionEntry<'a> {
    pub(crate) public_name: &'a str,
    pub(crate) root: &'a str,
    pub(crate) generation: [u8; 32],
    pub(crate) layout: &'a [u8; 32],
    pub(crate) public_slot: Option<usize>,
    pub(crate) root_slot: Option<usize>,
}

pub(crate) trait ModelAdmissionView: model_admission_sealed::Producer + Send + Sync {
    fn denied_words(&self) -> &[u64];
    fn generation(&self) -> [u8; 32];
    fn layout(&self) -> &[u8; 32];
    fn digest(&self) -> &[u8; 32];

    fn matches_layout(&self, generation: [u8; 32], layout: &[u8; 32]) -> bool {
        self.generation() == generation && self.layout() == layout
    }

    fn excludes(&self, entry: ModelAdmissionEntry<'_>) -> bool {
        if !self.matches_layout(entry.generation, entry.layout) {
            return true;
        }
        [entry.public_slot, entry.root_slot].into_iter().flatten().any(|slot| {
            self.denied_words()
                .get(slot / 64)
                .is_none_or(|word| word & (1_u64 << (slot % 64)) != 0)
        })
    }
}

// Preserve the previous concrete value equality, not digest-only equality.
impl PartialEq for dyn ModelAdmissionView + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.denied_words() == other.denied_words()
            && self.generation() == other.generation()
            && self.layout() == other.layout()
            && self.digest() == other.digest()
    }
}
impl Eq for dyn ModelAdmissionView + '_ {}
