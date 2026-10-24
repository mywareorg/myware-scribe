use std::fmt;

/// Speaker-count picker options: auto-detect or a fixed 1..=5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpeakerOpt {
    Auto,
    One,
    Two,
    Three,
    Four,
    Five,
}

impl SpeakerOpt {
    pub(crate) const ALL: [SpeakerOpt; 6] = [
        SpeakerOpt::Auto,
        SpeakerOpt::One,
        SpeakerOpt::Two,
        SpeakerOpt::Three,
        SpeakerOpt::Four,
        SpeakerOpt::Five,
    ];

    /// The pipeline value: `None` asks the diarizer to detect the count.
    pub(crate) fn value(self) -> Option<i32> {
        match self {
            SpeakerOpt::Auto => None,
            SpeakerOpt::One => Some(1),
            SpeakerOpt::Two => Some(2),
            SpeakerOpt::Three => Some(3),
            SpeakerOpt::Four => Some(4),
            SpeakerOpt::Five => Some(5),
        }
    }

    pub(crate) fn from_value(value: Option<i32>) -> Self {
        match value {
            None | Some(0) => SpeakerOpt::Auto,
            Some(1) => SpeakerOpt::One,
            Some(3) => SpeakerOpt::Three,
            Some(4) => SpeakerOpt::Four,
            Some(5) => SpeakerOpt::Five,
            _ => SpeakerOpt::Two,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SpeakerOpt::Auto => "Auto",
            SpeakerOpt::One => "1",
            SpeakerOpt::Two => "2",
            SpeakerOpt::Three => "3",
            SpeakerOpt::Four => "4",
            SpeakerOpt::Five => "5",
        }
    }
}

impl fmt::Display for SpeakerOpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Spoken-language options. `Auto` lets Whisper detect the language; the
/// English-only speed model always runs as English.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LanguageOpt {
    Auto,
    English,
    Spanish,
    French,
    German,
    Italian,
    Portuguese,
    Dutch,
    Russian,
    Japanese,
    Korean,
    Chinese,
    Ukrainian,
    Polish,
    Turkish,
}

impl LanguageOpt {
    pub(crate) const ALL: [LanguageOpt; 15] = [
        LanguageOpt::Auto,
        LanguageOpt::English,
        LanguageOpt::Spanish,
        LanguageOpt::French,
        LanguageOpt::German,
        LanguageOpt::Italian,
        LanguageOpt::Portuguese,
        LanguageOpt::Dutch,
        LanguageOpt::Russian,
        LanguageOpt::Japanese,
        LanguageOpt::Korean,
        LanguageOpt::Chinese,
        LanguageOpt::Ukrainian,
        LanguageOpt::Polish,
        LanguageOpt::Turkish,
    ];

    pub(crate) fn code(self) -> &'static str {
        match self {
            LanguageOpt::Auto => "auto",
            LanguageOpt::English => "en",
            LanguageOpt::Spanish => "es",
            LanguageOpt::French => "fr",
            LanguageOpt::German => "de",
            LanguageOpt::Italian => "it",
            LanguageOpt::Portuguese => "pt",
            LanguageOpt::Dutch => "nl",
            LanguageOpt::Russian => "ru",
            LanguageOpt::Japanese => "ja",
            LanguageOpt::Korean => "ko",
            LanguageOpt::Chinese => "zh",
            LanguageOpt::Ukrainian => "uk",
            LanguageOpt::Polish => "pl",
            LanguageOpt::Turkish => "tr",
        }
    }

    pub(crate) fn from_code(code: Option<&str>) -> Self {
        match code.unwrap_or("en") {
            "auto" => LanguageOpt::Auto,
            "es" => LanguageOpt::Spanish,
            "fr" => LanguageOpt::French,
            "de" => LanguageOpt::German,
            "it" => LanguageOpt::Italian,
            "pt" => LanguageOpt::Portuguese,
            "nl" => LanguageOpt::Dutch,
            "ru" => LanguageOpt::Russian,
            "ja" => LanguageOpt::Japanese,
            "ko" => LanguageOpt::Korean,
            "zh" => LanguageOpt::Chinese,
            "uk" => LanguageOpt::Ukrainian,
            "pl" => LanguageOpt::Polish,
            "tr" => LanguageOpt::Turkish,
            _ => LanguageOpt::English,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            LanguageOpt::Auto => "Auto-detect",
            LanguageOpt::English => "English",
            LanguageOpt::Spanish => "Spanish",
            LanguageOpt::French => "French",
            LanguageOpt::German => "German",
            LanguageOpt::Italian => "Italian",
            LanguageOpt::Portuguese => "Portuguese",
            LanguageOpt::Dutch => "Dutch",
            LanguageOpt::Russian => "Russian",
            LanguageOpt::Japanese => "Japanese",
            LanguageOpt::Korean => "Korean",
            LanguageOpt::Chinese => "Chinese",
            LanguageOpt::Ukrainian => "Ukrainian",
            LanguageOpt::Polish => "Polish",
            LanguageOpt::Turkish => "Turkish",
        }
    }
}

impl fmt::Display for LanguageOpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Playback speed multiplier options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speed {
    Half,
    One,
    OneHalf,
    Two,
}

impl Speed {
    pub(crate) const ALL: [Speed; 4] = [Speed::Half, Speed::One, Speed::OneHalf, Speed::Two];

    pub(crate) fn factor(self) -> f32 {
        match self {
            Speed::Half => 0.5,
            Speed::One => 1.0,
            Speed::OneHalf => 1.5,
            Speed::Two => 2.0,
        }
    }

    pub(crate) fn from_factor(factor: f32) -> Self {
        if (factor - 0.5).abs() < 0.01 {
            Speed::Half
        } else if (factor - 1.5).abs() < 0.01 {
            Speed::OneHalf
        } else if (factor - 2.0).abs() < 0.01 {
            Speed::Two
        } else {
            Speed::One
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Speed::Half => ".5x",
            Speed::One => "1x",
            Speed::OneHalf => "1.5x",
            Speed::Two => "2x",
        }
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Options in the "Import…" dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportKind {
    Transcription,
    Audio,
}

impl ImportKind {
    pub(crate) const ALL: [ImportKind; 2] = [ImportKind::Transcription, ImportKind::Audio];

    pub(crate) fn label(self) -> &'static str {
        match self {
            ImportKind::Transcription => "Transcript",
            ImportKind::Audio => "Audio",
        }
    }
}

impl fmt::Display for ImportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Options in the "Help…" dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpKind {
    About,
    Shortcuts,
    Console,
}

impl HelpKind {
    pub(crate) const ALL: [HelpKind; 3] = [HelpKind::About, HelpKind::Shortcuts, HelpKind::Console];

    pub(crate) fn label(self) -> &'static str {
        match self {
            HelpKind::About => "About",
            HelpKind::Shortcuts => "Shortcuts",
            HelpKind::Console => "Console",
        }
    }
}

impl fmt::Display for HelpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Preset {
    #[default]
    Speed,
    Balanced,
    Accuracy,
}

impl Preset {
    pub(crate) const ALL: [Preset; 3] = [Preset::Speed, Preset::Balanced, Preset::Accuracy];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Preset::Speed => "Speed",
            Preset::Balanced => "Balanced",
            Preset::Accuracy => "Accuracy",
        }
    }

    pub(crate) fn arg(self) -> &'static str {
        match self {
            Preset::Speed => "speed",
            Preset::Balanced => "balanced",
            Preset::Accuracy => "accuracy",
        }
    }

    pub(crate) fn from_arg(value: &str) -> Option<Preset> {
        match value {
            "speed" => Some(Preset::Speed),
            "balanced" => Some(Preset::Balanced),
            "accuracy" => Some(Preset::Accuracy),
            _ => None,
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}
