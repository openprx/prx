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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtPathCandidate {
    pub path: String,
    pub is_dir: bool,
}
