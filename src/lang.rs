use clap::ValueEnum;

/// Language for agent prompts and user-facing ghost text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lang {
    Ja,
    En,
}

impl Default for Lang {
    fn default() -> Self {
        Self::Ja
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ja => "ja",
            Self::En => "en",
        }
    }

    pub fn say(self, ja: &str, en: &str) -> String {
        match self {
            Self::Ja => ja.to_string(),
            Self::En => en.to_string(),
        }
    }
}
