#![allow(dead_code)] // The store seam is exercised by the Windows/macOS clients.
//!
//! Free/Pro entitlement: tiers, features, trial, offline grace and the
//! platform store provider seam.
//!
//! Nothing here talks to a store. A `Provider` implementation supplies the
//! purchased state; `State` turns that into an effective tier, honouring a
//! 7-day trial and a 30-day offline grace period. The Windows/Mac store
//! clients plug in behind [`Provider`] without touching the gating code.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Length of the in-app Pro trial.
pub const TRIAL_DAYS: u64 = 7;
/// How long a previously verified Pro entitlement survives offline.
pub const GRACE_DAYS: u64 = 30;
/// How often a Pro entitlement should be re-verified with the store.
pub const RECHECK_AFTER_HOURS: u64 = 24;
/// The single paid product on both stores.
pub const PRODUCT_ID: &str = "pro";

/// Per-user configuration directory shared by the GUI and the CLI.
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var("APPDATA")
            .ok()
            .map(|base| PathBuf::from(base).join("hippo-transcribe"))
    } else {
        let base = std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/.config"))
        })?;
        Some(PathBuf::from(base).join("hippo-transcribe"))
    }
}

/// Path of the persisted license record.
pub fn license_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("license.json"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Free,
    Pro,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Free => "Free",
            Tier::Pro => "Pro",
        }
    }
}

/// Which tier an interface element belongs to.
///
/// `Feature` answers "may this run?"; `Audience` answers "should this be on
/// screen?". Keeping them separate lets a control be shown-but-locked instead
/// of silently missing, and gives every widget an explicit tier tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Present in both tiers.
    Everyone,
    /// Free-tier onboarding and upgrade surfaces; hidden once Pro is active.
    FreeOnly,
    /// Pro controls and status surfaces; hidden on the free tier.
    ProOnly,
}

impl Audience {
    pub fn visible_for(self, tier: Tier) -> bool {
        match self {
            Audience::Everyone => true,
            Audience::FreeOnly => tier == Tier::Free,
            Audience::ProOnly => tier == Tier::Pro,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Audience::Everyone => "both",
            Audience::FreeOnly => "free",
            Audience::ProOnly => "pro",
        }
    }
}

/// Everything the paid tier unlocks. Free keeps balanced-model transcription,
/// copy, save and search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// The `speed` preset.
    SpeedPreset,
    /// The `accuracy` preset.
    AccuracyPreset,
    /// Speaker diarization / speaker labels.
    Diarization,
    /// More than one file per run.
    Batch,
    /// Editing transcript lines and renaming speakers.
    Editing,
    /// Audio playback and playhead seeking.
    Playback,
    /// GPU-accelerated transcription.
    GpuAcceleration,
}

impl Feature {
    pub const ALL: [Feature; 7] = [
        Feature::SpeedPreset,
        Feature::AccuracyPreset,
        Feature::Diarization,
        Feature::Batch,
        Feature::Editing,
        Feature::Playback,
        Feature::GpuAcceleration,
    ];

    /// Human-readable name used by the paywall and toasts.
    pub fn label(self) -> &'static str {
        match self {
            Feature::SpeedPreset => "Speed preset",
            Feature::AccuracyPreset => "Accuracy preset",
            Feature::Diarization => "Speaker labels",
            Feature::Batch => "Batch transcription",
            Feature::Editing => "Transcript editing",
            Feature::Playback => "Audio playback",
            Feature::GpuAcceleration => "GPU acceleration",
        }
    }
}

/// The single source of truth for what each tier can do.
pub fn tier_allows(tier: Tier, feature: Feature) -> bool {
    match feature {
        Feature::SpeedPreset
        | Feature::AccuracyPreset
        | Feature::Diarization
        | Feature::Batch
        | Feature::Editing
        | Feature::Playback
        | Feature::GpuAcceleration => tier == Tier::Pro,
    }
}

/// Where the current tier came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// No entitlement: the free tier.
    None,
    /// A store receipt (or the dev override).
    Store,
    /// The in-app trial.
    Trial,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::None => "Free",
            Source::Store => "purchased",
            Source::Trial => "trial",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Source::None => "none",
            Source::Store => "store",
            Source::Trial => "trial",
        }
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("store") => Source::Store,
            Some("trial") => Source::Trial,
            _ => Source::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PurchaseState {
    #[default]
    Idle,
    Pending,
    Active,
    Cancelled,
    Failed,
    /// No store is available (portable build, unsupported platform, store
    /// integration not built yet).
    Unavailable,
}

impl PurchaseState {
    pub fn label(self) -> &'static str {
        match self {
            PurchaseState::Idle => "Not purchased",
            PurchaseState::Pending => "Waiting for the store…",
            PurchaseState::Active => "Pro is active",
            PurchaseState::Cancelled => "Purchase cancelled",
            PurchaseState::Failed => "Purchase failed",
            PurchaseState::Unavailable => "Not available in this build",
        }
    }
}

/// Effective access for the current moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub tier: Tier,
    pub source: Source,
    /// Whole days left in the trial, when the trial is active.
    pub trial_days_left: Option<u64>,
    /// True when a Pro entitlement is older than `RECHECK_AFTER_HOURS`.
    pub needs_recheck: bool,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            tier: Tier::Free,
            source: Source::None,
            trial_days_left: None,
            needs_recheck: false,
        }
    }
}

/// Persisted license record. Deliberately a plain file: the gate is a
/// convenience, and a store receipt remains the authority. Never used to
/// delete user data on downgrade.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub pro: bool,
    /// Unix seconds of the last successful entitlement check.
    #[serde(default)]
    pub checked_unix: Option<u64>,
    /// Unix seconds when the trial started.
    #[serde(default)]
    pub trial_started_unix: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
    /// Local opt-out used by the development Free/Pro switch. A live trial
    /// would otherwise keep granting Pro after a downgrade.
    #[serde(default)]
    pub force_free: bool,
}

fn unix(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

fn days_between(from: u64, to: u64) -> u64 {
    to.saturating_sub(from) / 86_400
}

/// Pure effective-access calculation. `pro_override` lets the dev provider
/// force a tier without touching the persisted record.
pub fn access_for(record: &Record, pro_override: Option<bool>, now: SystemTime) -> Access {
    // An explicit development override wins; otherwise a local opt-out beats
    // both a purchased entitlement and an active trial.
    if pro_override.is_none() && record.force_free {
        return Access::default();
    }
    let now_s = unix(now);
    let pro = pro_override.unwrap_or(record.pro);
    if pro {
        let checked = record.checked_unix.unwrap_or(now_s);
        let stale_hours = now_s.saturating_sub(checked) / 3_600;
        let within_grace = days_between(checked, now_s) <= GRACE_DAYS;
        return Access {
            tier: if within_grace { Tier::Pro } else { Tier::Free },
            source: if within_grace {
                Source::Store
            } else {
                Source::None
            },
            trial_days_left: None,
            needs_recheck: stale_hours >= RECHECK_AFTER_HOURS,
        };
    }
    if let Some(started) = record.trial_started_unix {
        let elapsed = days_between(started, now_s);
        if elapsed < TRIAL_DAYS {
            return Access {
                tier: Tier::Pro,
                source: Source::Trial,
                trial_days_left: Some(TRIAL_DAYS - elapsed),
                needs_recheck: false,
            };
        }
    }
    Access::default()
}

/// Persisted state + the access derived from it.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub record: Record,
    pub access: Access,
    pub purchase: PurchaseState,
}

impl State {
    /// Load the record (a missing or corrupt file is the free tier).
    pub fn load(path: &Path, now: SystemTime) -> Self {
        let record = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Record>(&text).ok())
            .unwrap_or_default();
        let mut state = Self {
            access: access_for(&record, None, now),
            record,
            purchase: PurchaseState::Idle,
        };
        state.access.source = if state.record.pro {
            Source::parse(state.record.source.as_deref())
        } else if state.access.tier == Tier::Pro {
            Source::Trial
        } else {
            Source::None
        };
        state.purchase = if state.access.tier == Tier::Pro {
            PurchaseState::Active
        } else {
            PurchaseState::Idle
        };
        state
    }

    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.record) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Recompute `access` from the record. Returns true when the effective
    /// tier changed (callers may need to clamp the selected preset).
    pub fn refresh(&mut self, pro_override: Option<bool>, now: SystemTime) -> bool {
        let before = self.access.tier;
        self.access = access_for(&self.record, pro_override, now);
        if self.access.source == Source::None && self.access.tier == Tier::Free {
            self.purchase = PurchaseState::Idle;
        } else {
            self.purchase = PurchaseState::Active;
        }
        before != self.access.tier
    }

    /// Start the one-time trial. Returns false when it was already used.
    pub fn start_trial(&mut self, now: SystemTime) -> bool {
        if self.record.trial_started_unix.is_some() || self.record.pro {
            return false;
        }
        self.record.trial_started_unix = Some(unix(now));
        self.record.source = Some(Source::Trial.as_str().to_string());
        self.refresh(None, now);
        true
    }

    /// Record a successful store entitlement check.
    pub fn grant_pro(&mut self, now: SystemTime) {
        self.record.force_free = false;
        self.record.pro = true;
        self.record.checked_unix = Some(unix(now));
        self.record.source = Some(Source::Store.as_str().to_string());
        self.refresh(None, now);
    }

    /// Record a refund/expiry.
    pub fn revoke_pro(&mut self, now: SystemTime) {
        self.record.force_free = true;
        self.record.pro = false;
        self.record.checked_unix = Some(unix(now));
        self.record.source = Some(Source::None.as_str().to_string());
        self.refresh(None, now);
    }

    pub fn trial_used(&self) -> bool {
        self.record.trial_started_unix.is_some()
    }
}

/// What a store integration must provide. Implementations live beside this
/// module (`store_windows`, later `store_macos`); the free/portable builds use
/// [`NoneProvider`].
pub trait Provider {
    fn name(&self) -> &'static str;
    /// Localized price, when the store reports one.
    fn price(&self) -> Option<String>;
    /// Ask the store to start the purchase. Blocking; call off the UI thread.
    fn purchase(&mut self) -> PurchaseState;
    /// Restore a previous purchase and return the resulting state.
    fn restore(&mut self) -> PurchaseState;
    /// Deep link to the store's manage/cancel page.
    fn manage_url(&self) -> Option<String>;
    /// Force a tier in development (`HIPPO_TRANSCRIBE_TIER=pro|free`).
    fn override_tier(&self) -> Option<bool> {
        None
    }
}

/// No store: the portable/unsigned build. Buying is not possible, the trial is.
#[derive(Debug, Default)]
pub struct NoneProvider;

impl Provider for NoneProvider {
    fn name(&self) -> &'static str {
        "none"
    }
    fn price(&self) -> Option<String> {
        None
    }
    fn purchase(&mut self) -> PurchaseState {
        PurchaseState::Unavailable
    }
    fn restore(&mut self) -> PurchaseState {
        PurchaseState::Unavailable
    }
    fn manage_url(&self) -> Option<String> {
        None
    }
}

/// Development override: `HIPPO_TRANSCRIBE_TIER=pro` (or `free`).
#[derive(Debug, Default)]
pub struct DevProvider;

impl DevProvider {
    fn forced() -> Option<bool> {
        match std::env::var("HIPPO_TRANSCRIBE_TIER") {
            Ok(value) if value.eq_ignore_ascii_case("pro") => Some(true),
            Ok(value) if value.eq_ignore_ascii_case("free") => Some(false),
            _ => None,
        }
    }
}

impl Provider for DevProvider {
    fn name(&self) -> &'static str {
        "dev"
    }
    fn price(&self) -> Option<String> {
        Some("$14.99".to_string())
    }
    fn purchase(&mut self) -> PurchaseState {
        match Self::forced() {
            Some(true) => PurchaseState::Active,
            _ => PurchaseState::Unavailable,
        }
    }
    fn restore(&mut self) -> PurchaseState {
        self.purchase()
    }
    fn manage_url(&self) -> Option<String> {
        None
    }
    fn override_tier(&self) -> Option<bool> {
        Self::forced()
    }
}

/// Pick the provider for this build. Store implementations are added later;
/// until then every build reports no store and relies on the trial/dev env.
pub fn provider() -> Box<dyn Provider> {
    if DevProvider::forced().is_some() {
        Box::new(DevProvider)
    } else {
        Box::new(NoneProvider)
    }
}

/// Display price used before a store client is wired up.
pub const FALLBACK_PRICE: &str = "$14.99";

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn audiences_map_to_tiers() {
        assert!(Audience::Everyone.visible_for(Tier::Free));
        assert!(Audience::Everyone.visible_for(Tier::Pro));
        assert!(Audience::FreeOnly.visible_for(Tier::Free));
        assert!(!Audience::FreeOnly.visible_for(Tier::Pro));
        assert!(Audience::ProOnly.visible_for(Tier::Pro));
        assert!(!Audience::ProOnly.visible_for(Tier::Free));
    }

    #[test]
    fn free_tier_unlocks_nothing_paid() {
        for feature in Feature::ALL {
            assert!(!tier_allows(Tier::Free, feature), "{}", feature.label());
            assert!(tier_allows(Tier::Pro, feature), "{}", feature.label());
        }
    }

    #[test]
    fn trial_runs_for_seven_days() {
        let record = Record {
            trial_started_unix: Some(1_000_000),
            ..Record::default()
        };
        let day0 = access_for(&record, None, at(1_000_000));
        assert_eq!(day0.tier, Tier::Pro);
        assert_eq!(day0.source, Source::Trial);
        assert_eq!(day0.trial_days_left, Some(7));

        let day6 = access_for(&record, None, at(1_000_000 + 6 * 86_400));
        assert_eq!(day6.tier, Tier::Pro);
        assert_eq!(day6.trial_days_left, Some(1));

        let day7 = access_for(&record, None, at(1_000_000 + 7 * 86_400));
        assert_eq!(day7.tier, Tier::Free);
        assert_eq!(day7.source, Source::None);
    }

    #[test]
    fn paid_entitlement_survives_grace_then_expires() {
        let record = Record {
            pro: true,
            checked_unix: Some(2_000_000),
            source: Some("store".to_string()),
            ..Record::default()
        };
        let fresh = access_for(&record, None, at(2_000_000));
        assert_eq!(fresh.tier, Tier::Pro);
        assert!(!fresh.needs_recheck);

        let stale = access_for(&record, None, at(2_000_000 + 2 * 86_400));
        assert_eq!(stale.tier, Tier::Pro);
        assert!(stale.needs_recheck);

        let expired = access_for(&record, None, at(2_000_000 + 31 * 86_400));
        assert_eq!(expired.tier, Tier::Free);
    }

    #[test]
    fn dev_override_wins_over_the_record() {
        let record = Record::default();
        assert_eq!(
            access_for(&record, Some(true), at(0)).tier,
            Tier::Pro,
            "override should force Pro"
        );
        let paid = Record {
            pro: true,
            checked_unix: Some(0),
            ..Record::default()
        };
        assert_eq!(access_for(&paid, Some(false), at(0)).tier, Tier::Free);
    }

    #[test]
    fn a_local_opt_out_beats_an_active_trial() {
        let mut state = State::default();
        state.start_trial(at(10));
        assert_eq!(state.access.tier, Tier::Pro);
        state.revoke_pro(at(20));
        assert_eq!(state.access.tier, Tier::Free);
        state.grant_pro(at(30));
        assert_eq!(state.access.tier, Tier::Pro);
    }

    #[test]
    fn trial_can_only_start_once() {
        let mut state = State::default();
        assert!(state.start_trial(at(10)));
        assert!(!state.start_trial(at(20)));
        assert!(state.trial_used());
        assert_eq!(state.access.tier, Tier::Pro);
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("hippo-license-{}", std::process::id()));
        let path = dir.join("license.json");
        let mut state = State::default();
        state.start_trial(at(5_000));
        state.save(&path);
        let loaded = State::load(&path, at(5_000));
        assert_eq!(loaded.access.tier, Tier::Pro);
        assert_eq!(loaded.access.source, Source::Trial);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
