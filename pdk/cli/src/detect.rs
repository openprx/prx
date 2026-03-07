use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    Go,
}

impl Language {
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::Go => "go",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Detect the plugin language from files present in `dir`.
/// Priority: Cargo.toml > go.mod > package.json > pyproject.toml/setup.py
pub fn detect_language(dir: &Path) -> Option<Language> {
    if dir.join("Cargo.toml").exists() {
        return Some(Language::Rust);
    }
    if dir.join("go.mod").exists() {
        return Some(Language::Go);
    }
    if dir.join("package.json").exists() {
        return Some(Language::JavaScript);
    }
    if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        return Some(Language::Python);
    }
    None
}
