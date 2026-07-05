#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashProviderModelCatalog {
    pub provider: String,
    pub models: Vec<SlashModelCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashModelCandidate {
    pub name: String,
    pub description: String,
}
