use std::fmt;

/// Speaker picker options: one voice, or as many as the diarizer finds.
///
/// `Multiple` is the auto-detecting case: it hands no count to the diarizer, so
/// the clustering decides. Asking for a fixed 2 could not be right for a file
/// with three voices, and asking for 5 on a two-person file invented speakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpeakerOpt {
    Single,
    Multiple,
}

impl SpeakerOpt {
    pub(crate) const ALL: [SpeakerOpt; 2] = [SpeakerOpt::Single, SpeakerOpt::Multiple];

    /// The pipeline value. `None` means "detect".
    pub(crate) fn value(self) -> Option<i32> {
        match self {
            SpeakerOpt::Single => Some(1),
            SpeakerOpt::Multiple => None,
        }
    }

    pub(crate) fn from_value(value: Option<i32>) -> Self {
        match value {
            Some(1) => SpeakerOpt::Single,
            // Anything else, including the old fixed 2..=5 settings, is a
            // multi-speaker run: detect rather than keep a stale count.
            _ => SpeakerOpt::Multiple,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SpeakerOpt::Single => "Single",
            SpeakerOpt::Multiple => "Multiple",
        }
    }
}

impl fmt::Display for SpeakerOpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Spoken-language options. `Auto` lets Whisper detect the language; every
/// preset uses a multilingual model, so an explicit code always applies.
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
        match code {
            None | Some("") | Some("auto") => LanguageOpt::Auto,
            Some("en") => LanguageOpt::English,
            Some("es") => LanguageOpt::Spanish,
            Some("fr") => LanguageOpt::French,
            Some("de") => LanguageOpt::German,
            Some("it") => LanguageOpt::Italian,
            Some("pt") => LanguageOpt::Portuguese,
            Some("nl") => LanguageOpt::Dutch,
            Some("ru") => LanguageOpt::Russian,
            Some("ja") => LanguageOpt::Japanese,
            Some("ko") => LanguageOpt::Korean,
            Some("zh") => LanguageOpt::Chinese,
            Some("uk") => LanguageOpt::Ukrainian,
            Some("pl") => LanguageOpt::Polish,
            Some("tr") => LanguageOpt::Turkish,
            _ => LanguageOpt::Auto,
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
    Four,
}

impl Speed {
    pub(crate) const ALL: [Speed; 5] = [
        Speed::Half,
        Speed::One,
        Speed::OneHalf,
        Speed::Two,
        Speed::Four,
    ];

    pub(crate) fn factor(self) -> f32 {
        match self {
            Speed::Half => 0.5,
            Speed::One => 1.0,
            Speed::OneHalf => 1.5,
            Speed::Two => 2.0,
            Speed::Four => 4.0,
        }
    }

    pub(crate) fn from_factor(factor: f32) -> Self {
        if (factor - 0.5).abs() < 0.01 {
            Speed::Half
        } else if (factor - 1.5).abs() < 0.01 {
            Speed::OneHalf
        } else if (factor - 2.0).abs() < 0.01 {
            Speed::Two
        } else if (factor - 4.0).abs() < 0.01 {
            Speed::Four
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
            Speed::Four => "4x",
        }
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Entries in the Help menu that open a reference dialog or toggle a panel.
///
/// The menu itself adds the appearance choices; these are the actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpKind {
    About,
    Shortcuts,
    Console,
    Settings,
}

impl HelpKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            HelpKind::About => "About",
            HelpKind::Shortcuts => "Shortcuts",
            HelpKind::Console => "Console",
            HelpKind::Settings => "Settings",
        }
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
