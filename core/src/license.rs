#![allow(dead_code)] // The store seam is exercised by the Windows/macOS clients.
//!
//! Free/Pro entitlement: tiers, features, offline grace and the platform
//! store provider seam.
//!
//! Nothing here talks to a store. A `Provider` implementation supplies the
//! purchased state; `State` turns that into an effective tier, honouring a
//! 30-day offline grace period. The Windows/Mac store clients plug in behind
//! [`Provider`] without touching the gating code.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

/// Everything the paid tier unlocks. Free keeps speed-model transcription,
/// one speaker, copy, save, search and recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// The `balanced` preset.
    BalancedPreset,
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
    /// The generated briefing for a finished transcript.
    Briefing,
    /// Freeform labels for organizing files.
    Labels,
    /// Export formats and generated reports.
    Export,
}

impl Feature {
    pub const ALL: [Feature; 10] = [
        Feature::BalancedPreset,
        Feature::AccuracyPreset,
        Feature::Diarization,
        Feature::Batch,
        Feature::Editing,
        Feature::Playback,
        Feature::GpuAcceleration,
        Feature::Briefing,
        Feature::Labels,
        Feature::Export,
    ];

    /// Human-readable name used by the paywall and toasts.
    pub fn label(self) -> &'static str {
        match self {
            Feature::BalancedPreset => "Balanced preset",
            Feature::AccuracyPreset => "Accuracy preset",
            Feature::Diarization => "Speaker labels",
            Feature::Batch => "Batch transcription",
            Feature::Editing => "Transcript editing",
            Feature::Playback => "Audio playback",
            Feature::GpuAcceleration => "GPU acceleration",
            Feature::Briefing => "Summaries",
            Feature::Labels => "Labels",
            Feature::Export => "Exports and reports",
        }
    }
}

/// The single source of truth for what each tier can do.
pub fn tier_allows(tier: Tier, feature: Feature) -> bool {
    match feature {
        Feature::BalancedPreset
        | Feature::AccuracyPreset
        | Feature::Diarization
        | Feature::Batch
        | Feature::Editing
        | Feature::Playback
        | Feature::GpuAcceleration
        | Feature::Briefing
        | Feature::Labels
        | Feature::Export => tier == Tier::Pro,
    }
}

/// Where the current tier came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// No entitlement: the free tier.
    None,
    /// A store receipt (or the dev override).
    Store,
    /// Compiled in: the always-Pro edition, which has nothing to buy and
    /// nothing that can lapse.
    Bundled,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::None => "Free",
            Source::Store => "purchased",
            Source::Bundled => "included",
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Source::None => "none",
            Source::Store => "store",
            Source::Bundled => "bundled",
        }
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("store") => Source::Store,
            Some("bundled") => Source::Bundled,
            _ => Source::None,
        }
    }
}

/// True in the always-Pro edition (`--features pro`, the direct-download
/// build). The tier is a property of the binary: there is no store to ask and
/// no receipt that can lapse, so the grace window below does not apply. Tests
/// keep the normal rules so those paths stay covered.
pub const UNLOCKED: bool = cfg!(feature = "pro");

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
    /// True when a Pro entitlement is older than `RECHECK_AFTER_HOURS`.
    pub needs_recheck: bool,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            tier: Tier::Free,
            source: Source::None,
            needs_recheck: false,
        }
    }
}

/// Persisted license record. The gate is a convenience and a store receipt
/// remains the authority; `last_seen_unix` only stops a clock rollback from
/// stretching the offline grace window. Deleting the file still resets it.
/// Never used to delete user data on downgrade.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub pro: bool,
    /// Unix seconds of the last successful entitlement check.
    #[serde(default)]
    pub checked_unix: Option<u64>,
    /// Latest wall-clock time this record has been observed at. A clock rolled
    /// back below this cannot stretch the grace window.
    #[serde(default)]
    pub last_seen_unix: Option<u64>,
    #[serde(default)]
    pub source: Option<String>,
    /// Local opt-out used by the development Free/Pro switch.
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

/// The clock to judge access by: never earlier than the last moment this
/// record was seen, so moving the system clock back cannot stretch the offline
/// grace window.
fn observed_now(record: &Record, now_s: u64) -> u64 {
    now_s.max(record.last_seen_unix.unwrap_or(0))
}

/// Pure effective-access calculation. `pro_override` lets the dev provider
/// force a tier without touching the persisted record.
pub fn access_for(record: &Record, pro_override: Option<bool>, now: SystemTime) -> Access {
    // The always-Pro edition never consults a receipt or the clock.
    if UNLOCKED && !cfg!(test) {
        return Access {
            tier: Tier::Pro,
            source: Source::Bundled,
            needs_recheck: false,
        };
    }
    // An explicit development override wins; otherwise a local opt-out beats
    // a purchased entitlement.
    if pro_override.is_none() && record.force_free {
        return Access::default();
    }
    let now_s = observed_now(record, unix(now));
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
            needs_recheck: stale_hours >= RECHECK_AFTER_HOURS,
        };
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
        // The record only says where a store receipt came from; the always-Pro
        // edition reports its own source.
        if !(UNLOCKED && !cfg!(test)) {
            state.access.source = if state.record.pro {
                Source::parse(state.record.source.as_deref())
            } else {
                Source::None
            };
        }
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
        let now_s = unix(now);
        self.record.last_seen_unix = Some(match self.record.last_seen_unix {
            Some(seen) => seen.max(now_s),
            None => now_s,
        });
        let before = self.access.tier;
        self.access = access_for(&self.record, pro_override, now);
        if self.access.source == Source::None && self.access.tier == Tier::Free {
            self.purchase = PurchaseState::Idle;
        } else {
            self.purchase = PurchaseState::Active;
        }
        before != self.access.tier
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

    /// Drop a store entitlement the store no longer reports (a refund, or a
    /// subscription that ended). Unlike [`Self::revoke_pro`] this leaves the
    /// development switch alone.
    pub fn expire_store(&mut self, now: SystemTime) {
        self.record.pro = false;
        self.record.checked_unix = Some(unix(now));
        self.record.source = Some(Source::None.as_str().to_string());
        self.refresh(None, now);
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
}

/// No store: the portable/unsigned build. Buying is not possible, so the tier
/// stays free unless the `pro` feature is compiled in.
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

/// Pick the provider for this build. There is deliberately no development
/// override: a shipped binary must not be able to grant itself Pro. Store
/// clients are selected by the platform client, and the always-Pro edition is
/// compiled in instead.
pub fn provider() -> Box<dyn Provider> {
    Box::new(NoneProvider)
}

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
    fn refresh_records_the_latest_clock() {
        let mut state = State::default();
        state.grant_pro(at(100));
        assert_eq!(state.record.last_seen_unix, Some(100));
        state.refresh(None, at(100 + 86_400));
        assert_eq!(state.record.last_seen_unix, Some(100 + 86_400));
        // A later refresh with an earlier clock never moves the floor back.
        state.refresh(None, at(50));
        assert_eq!(state.record.last_seen_unix, Some(100 + 86_400));
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
    fn a_local_opt_out_beats_a_paid_entitlement() {
        let mut state = State::default();
        state.grant_pro(at(10));
        assert_eq!(state.access.tier, Tier::Pro);
        state.revoke_pro(at(20));
        assert_eq!(state.access.tier, Tier::Free);
        state.grant_pro(at(30));
        assert_eq!(state.access.tier, Tier::Pro);
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("hippo-license-{}", std::process::id()));
        let path = dir.join("license.json");
        let mut state = State::default();
        state.grant_pro(at(5_000));
        state.save(&path);
        let loaded = State::load(&path, at(5_000));
        assert_eq!(loaded.access.tier, Tier::Pro);
        assert_eq!(loaded.access.source, Source::Store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
