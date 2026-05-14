//! Faction archetype + capability bundle (Phase 1a of the
//! capabilities/CommunityHub/storage-parity refactor).
//!
//! Replaces ad-hoc `(Lifestyle, EconomyPreset)` enum dispatch with a
//! single per-faction `FactionCapabilities` value computed once at
//! faction creation. Every site that previously branched on
//! `is_nomadic()` or `match EconomyPreset` should consult a capability
//! accessor instead.
//!
//! P1a is purely additive: the legacy `FactionData.lifestyle`,
//! `economic_policy`, and `land_policy` fields are kept, and
//! `derive_from_legacy(...)` produces a capability bundle that mirrors
//! today's behaviour bit-for-bit. Later phases relocate the source of
//! truth into capabilities and load archetypes from RON.

use crate::economy::policy::{LandPolicy, ResourceControlPolicy};
use crate::economy::resource_catalog::{ResourceCatalog, ResourceId};
use crate::game_state::EconomyPreset;
use crate::simulation::faction::Lifestyle;
use ahash::AHashMap;
use bevy::prelude::Resource;
use serde::Deserialize;

/// How a faction's home location is anchored. Settled factions stay put
/// at a fixed `home_tile`; nomadic factions migrate seasonally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeMobility {
    Anchored,
    Mobile { migration_period_min_days: u32 },
}

impl HomeMobility {
    pub fn is_mobile(self) -> bool {
        matches!(self, HomeMobility::Mobile { .. })
    }
    pub fn is_anchored(self) -> bool {
        matches!(self, HomeMobility::Anchored)
    }
}

/// Where the faction physically holds its goods.
///
/// - `FactionTile`: settled, deposits flow to `FactionStorageTile`s.
/// - `MemberPool`: nomadic, goods live in member inventories.
/// - `Hybrid`: caravan-style, both tile and member pools.
/// - `CaravanBundles`: future, packed bundles on pack animals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageBackendKind {
    FactionTile,
    MemberPool,
    Hybrid,
    CaravanBundles,
}

/// Whether shelter structures are permanent (Hut/Longhouse) or
/// portable (Tent/Yurt/Bedroll) on migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShelterMode {
    Permanent,
    Portable,
    Mixed,
}

/// Settlement form. `FullSettlement` is a real settled village/town;
/// `Camp` is a stripped nomadic node (no plots, no zones); `None` is
/// for special-case archetypes that never get an economic node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementMode {
    FullSettlement { carves_plots: bool },
    Camp,
    None,
}

impl SettlementMode {
    pub fn is_full_settlement(self) -> bool {
        matches!(self, SettlementMode::FullSettlement { .. })
    }
    pub fn is_camp(self) -> bool {
        matches!(self, SettlementMode::Camp)
    }
    pub fn carves_plots(self) -> bool {
        matches!(self, SettlementMode::FullSettlement { carves_plots: true })
    }
}

/// Coordination layer for productive work.
///
/// - `Disabled`: no posting layer; members run autonomous personal
///   needs only (current nomadic behaviour).
/// - `Enabled`: chief and/or private actors post jobs; per-resource
///   `economic_policy[rid]` decides whether the chief allocates labour
///   or workers self-post.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostingMode {
    Disabled,
    Enabled,
}

impl PostingMode {
    pub fn enabled(self) -> bool {
        matches!(self, PostingMode::Enabled)
    }
    pub fn is_disabled(self) -> bool {
        matches!(self, PostingMode::Disabled)
    }
}

/// What happens to a plot when its tenant gets evicted (rent-collection
/// path) or when an archetype switch retires the plot system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictionPolicy {
    /// Today's behaviour: structures stay in place, plot reverts to
    /// `StateOwned`.
    LeaveStructures,
    /// Structures stay, marked state-owned, queued for reuse.
    RevertToState,
    /// Despawn structures + drop refund stacks via
    /// `Deployable::compute_refund_drop`.
    Demolish,
}

/// Faction-wide land tenure modalities. Wraps `LandPolicy` (existing
/// field-level governance flags) with archetype-level toggles for
/// whether plots are carved at all and how eviction is handled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LandModalities {
    pub policy: LandPolicy,
    pub carves_plots: bool,
    pub eviction_policy: EvictionPolicy,
}

/// How currency flows between agents, household sub-factions, and
/// parent overlords. Replaces the unconditional 10% household skim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IncomeFlow {
    /// Fraction of each agent's market earnings redirected to their
    /// household sub-faction's treasury. Default 0 (Subsistence);
    /// Mixed/Market = 0.10.
    pub household_skim_pct: f32,
    /// Fraction of household income passed up to a parent overlord
    /// (feudal vassal chain). Default 0.
    pub upward_skim_to_parent_pct: f32,
}

impl Default for IncomeFlow {
    fn default() -> Self {
        Self {
            household_skim_pct: 0.0,
            upward_skim_to_parent_pct: 0.0,
        }
    }
}

/// What sub-factions inherit from their parent. P1a leaves the
/// existing `spawn_household` rules intact; later phases drive
/// inheritance off this spec.
#[derive(Clone, Debug, PartialEq)]
pub struct InheritanceSpec {
    pub child_archetype_key: Option<String>,
    pub seed_treasury: f32,
    pub seed_storage_tile: bool,
}

impl Default for InheritanceSpec {
    fn default() -> Self {
        Self {
            child_archetype_key: None,
            seed_treasury: 0.0,
            seed_storage_tile: false,
        }
    }
}

/// Per-faction capability bundle. One value per faction, attached to
/// `FactionData.caps`. Computed once at faction creation by
/// `derive_from_legacy(...)`; later phases load it from RON.
#[derive(Clone, Debug)]
pub struct FactionCapabilities {
    pub archetype_key: String,
    pub home: HomeMobility,
    pub storage: StorageBackendKind,
    pub shelter: ShelterMode,
    pub settlement: SettlementMode,
    pub posting: PostingMode,
    pub land: LandModalities,
    pub economic_policy: AHashMap<ResourceId, ResourceControlPolicy>,
    pub income: IncomeFlow,
    pub inheritance: InheritanceSpec,
}

impl Default for FactionCapabilities {
    fn default() -> Self {
        // Default = settled-Subsistence: matches the historical
        // `FactionData::default()` with empty policy maps.
        Self {
            archetype_key: "settled_subsistence".to_string(),
            home: HomeMobility::Anchored,
            storage: StorageBackendKind::FactionTile,
            shelter: ShelterMode::Permanent,
            settlement: SettlementMode::FullSettlement { carves_plots: true },
            posting: PostingMode::Enabled,
            land: LandModalities {
                policy: LandPolicy::default(),
                carves_plots: true,
                eviction_policy: EvictionPolicy::LeaveStructures,
            },
            economic_policy: AHashMap::new(),
            income: IncomeFlow::default(),
            inheritance: InheritanceSpec::default(),
        }
    }
}

/// Migration-period floor for `Mobile` archetypes. Today's nomads use
/// `TICKS_PER_SEASON` (one season) as the cooldown floor; converting
/// roughly to ~30 in-game days for the capability metadata.
const NOMAD_MIGRATION_PERIOD_MIN_DAYS: u32 = 30;

/// Compute the capability bundle that mirrors today's
/// `(lifestyle, preset)` behaviour. `catalog` is needed to populate
/// the `economic_policy` map for Mixed/Market presets.
///
/// P1a invariant: the bundle this returns must be observationally
/// identical to the legacy field set. Adding new capability axes is
/// fine, but flipping any flag away from the legacy default would
/// break the regression baseline.
pub fn derive_from_legacy(
    lifestyle: Lifestyle,
    preset: EconomyPreset,
    catalog: &ResourceCatalog,
) -> FactionCapabilities {
    let archetype_key = legacy_archetype_key(lifestyle, preset).to_string();

    let (home, storage, shelter, settlement, posting) = match lifestyle {
        Lifestyle::Settled => (
            HomeMobility::Anchored,
            StorageBackendKind::FactionTile,
            ShelterMode::Permanent,
            SettlementMode::FullSettlement { carves_plots: true },
            PostingMode::Enabled,
        ),
        Lifestyle::Nomadic => (
            HomeMobility::Mobile {
                migration_period_min_days: NOMAD_MIGRATION_PERIOD_MIN_DAYS,
            },
            StorageBackendKind::MemberPool,
            ShelterMode::Portable,
            SettlementMode::Camp,
            // Phase 7 (minimal) of the nomadic plan early-outs both
            // chief construction and chief job posting for nomadic
            // factions. Mirror that as `Disabled` so capability
            // checks subsume the `is_nomadic()` early-returns.
            PostingMode::Disabled,
        ),
    };

    let mut economic_policy = AHashMap::new();
    crate::economy::policy::apply_preset(&mut economic_policy, preset, catalog);

    let land = LandModalities {
        policy: crate::economy::policy::land_policy_for(preset),
        // Carving keys on Settlement existence today, which is itself
        // gated on lifestyle. Mirror that: only settled factions carve.
        carves_plots: matches!(lifestyle, Lifestyle::Settled),
        eviction_policy: EvictionPolicy::LeaveStructures,
    };

    let income = match preset {
        // P7a: Subsistence factions don't skim agent earnings into
        // household treasuries — communist villages can't have
        // households accruing private wealth and posting paid
        // contracts against design intent. Mixed/Market keep the
        // 10% legacy skim so household-driven contract posting
        // still funds itself.
        EconomyPreset::Subsistence => IncomeFlow {
            household_skim_pct: 0.0,
            upward_skim_to_parent_pct: 0.0,
        },
        EconomyPreset::Mixed | EconomyPreset::Market => IncomeFlow {
            household_skim_pct: 0.10,
            upward_skim_to_parent_pct: 0.0,
        },
    };

    let inheritance = InheritanceSpec {
        child_archetype_key: Some(format!("household_{}", preset_key(preset))),
        // Spawn-time household seeding (`seed_market_households` in
        // person.rs) is gated on `EconomyPreset::Market` and uses
        // `HOUSEHOLD_SEED_TREASURY = 15.0`. Mirror per preset.
        seed_treasury: match preset {
            EconomyPreset::Market => 15.0,
            _ => 0.0,
        },
        seed_storage_tile: matches!(preset, EconomyPreset::Market),
    };

    FactionCapabilities {
        archetype_key,
        home,
        storage,
        shelter,
        settlement,
        posting,
        land,
        economic_policy,
        income,
        inheritance,
    }
}

/// P5: per-archetype capability bundle, keyed by `archetype_key`.
/// Constructed at startup via `default_registry(catalog)` (derives
/// each entry from `derive_from_legacy`) and inserted as a Bevy
/// resource by `WorldPlugin::build`. Future RON loading replaces the
/// internal builder without touching consumers.
#[derive(Resource, Clone, Debug, Default)]
pub struct FactionArchetypeRegistry {
    entries: AHashMap<String, FactionCapabilities>,
}

impl FactionArchetypeRegistry {
    pub fn insert(&mut self, key: String, caps: FactionCapabilities) {
        self.entries.insert(key, caps);
    }

    /// Clone-out lookup. Returns `None` for unknown keys; callers
    /// either fall back to `derive_from_legacy` or treat the missing
    /// entry as a hard error depending on the call site.
    pub fn get(&self, key: &str) -> Option<FactionCapabilities> {
        self.entries.get(key).cloned()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(|s| s.as_str())
    }
}

// ── P5: RON archetype loader ──────────────────────────────────────
//
// Each `assets/data/factions/archetypes/*.ron` declares one or more
// `ArchetypeEntry { key, lifestyle, preset }` rows. The loader builds
// the capability bundle for each entry via `derive_from_legacy` —
// the (Lifestyle x EconomyPreset) cross is still the source of truth
// for capability fields today. Adding a new archetype that maps to an
// existing cross is a one-RON-entry change with no Rust edits.

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LifestyleSpec {
    Settled,
    Nomadic,
}

impl From<LifestyleSpec> for Lifestyle {
    fn from(s: LifestyleSpec) -> Self {
        match s {
            LifestyleSpec::Settled => Lifestyle::Settled,
            LifestyleSpec::Nomadic => Lifestyle::Nomadic,
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PresetSpec {
    Subsistence,
    Mixed,
    Market,
}

impl From<PresetSpec> for EconomyPreset {
    fn from(s: PresetSpec) -> Self {
        match s {
            PresetSpec::Subsistence => EconomyPreset::Subsistence,
            PresetSpec::Mixed => EconomyPreset::Mixed,
            PresetSpec::Market => EconomyPreset::Market,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ArchetypeEntry {
    key: String,
    lifestyle: LifestyleSpec,
    preset: PresetSpec,
}

#[derive(Debug, Deserialize)]
struct ArchetypeFile {
    archetypes: Vec<ArchetypeEntry>,
}

/// Load every `*.ron` file under `assets/data/factions/archetypes/`,
/// parse it, and merge entries into one registry. Each entry produces
/// a `FactionCapabilities` via `derive_from_legacy(lifestyle, preset,
/// catalog)`. Panics on missing dir / parse errors / duplicate keys —
/// startup-time failure is the right default for config-driven data.
pub fn load_archetype_registry(catalog: &ResourceCatalog) -> FactionArchetypeRegistry {
    let dir = std::path::Path::new("assets/data/factions/archetypes");
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "FactionArchetypeRegistry: cannot read {:?}: {}. \
             Archetype definitions must live in \
             assets/data/factions/archetypes/*.ron.",
            dir, e
        )
    });

    let mut reg = FactionArchetypeRegistry::default();
    let mut seen: AHashMap<String, std::path::PathBuf> = AHashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("FactionArchetypeRegistry: cannot read {:?}: {}", path, e));
        let file: ArchetypeFile = ron::from_str(&body).unwrap_or_else(|e| {
            panic!("FactionArchetypeRegistry: parse error in {:?}: {}", path, e)
        });
        for row in file.archetypes {
            if let Some(prev) = seen.get(&row.key) {
                panic!(
                    "FactionArchetypeRegistry: duplicate key {:?} in {:?} (also defined in {:?})",
                    row.key, path, prev
                );
            }
            seen.insert(row.key.clone(), path.clone());
            let caps = derive_from_legacy(row.lifestyle.into(), row.preset.into(), catalog);
            // Belt-and-suspenders: the bundle's archetype_key (assigned
            // by `legacy_archetype_key`) must match the RON-declared
            // key, otherwise the file mislabels its own row.
            assert_eq!(
                caps.archetype_key, row.key,
                "archetype key {:?} in {:?} doesn't match legacy_archetype_key for ({:?}, {:?})",
                row.key, path, row.lifestyle, row.preset
            );
            reg.insert(row.key, caps);
        }
    }

    if reg.len() == 0 {
        panic!(
            "FactionArchetypeRegistry: no archetypes found in {:?}. \
             At least one archetype definition is required.",
            dir
        );
    }
    reg
}

/// Build a registry populated with the four supported legacy archetypes
/// (`settled_subsistence`, `settled_mixed`, `settled_market`,
/// `nomadic_subsistence`). Routes through `load_archetype_registry`,
/// which reads `assets/data/factions/archetypes/*.ron` and derives
/// each entry's capabilities via `derive_from_legacy`. Adding a new
/// archetype is a one-RON-entry change with no Rust edits.
pub fn default_registry(catalog: &ResourceCatalog) -> FactionArchetypeRegistry {
    load_archetype_registry(catalog)
}

/// P5 forward-facing entry point: look up a capability bundle by key.
/// Falls back to `derive_from_legacy` only when the registry is missing
/// an entry — useful during the transition while RON loading lands.
/// New code should prefer this over `derive_from_legacy` directly.
pub fn derive_from_archetype_key(
    registry: &FactionArchetypeRegistry,
    key: &str,
    legacy_fallback: Option<(Lifestyle, EconomyPreset, &ResourceCatalog)>,
) -> Option<FactionCapabilities> {
    if let Some(caps) = registry.get(key) {
        return Some(caps);
    }
    if let Some((lifestyle, preset, catalog)) = legacy_fallback {
        return Some(derive_from_legacy(lifestyle, preset, catalog));
    }
    None
}

/// Stable archetype key for the four supported `(lifestyle, preset)`
/// combinations. Other crosses (e.g. `Nomadic + Market`) are
/// inert/unsupported today — labelled here so the field is
/// well-defined but later phases may reject them at registry load.
pub fn legacy_archetype_key(lifestyle: Lifestyle, preset: EconomyPreset) -> &'static str {
    match (lifestyle, preset) {
        (Lifestyle::Settled, EconomyPreset::Subsistence) => "settled_subsistence",
        (Lifestyle::Settled, EconomyPreset::Mixed) => "settled_mixed",
        (Lifestyle::Settled, EconomyPreset::Market) => "settled_market",
        (Lifestyle::Nomadic, EconomyPreset::Subsistence) => "nomadic_subsistence",
        (Lifestyle::Nomadic, EconomyPreset::Mixed) => "nomadic_mixed",
        (Lifestyle::Nomadic, EconomyPreset::Market) => "nomadic_market",
    }
}

fn preset_key(preset: EconomyPreset) -> &'static str {
    match preset {
        EconomyPreset::Subsistence => "subsistence",
        EconomyPreset::Mixed => "mixed",
        EconomyPreset::Market => "market",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::resource_catalog::{load_resource_catalog, ResourceCatalog};

    fn test_catalog() -> ResourceCatalog {
        // Install the catalog into the OnceLock so `core_ids::wood()` etc.
        // resolve in tests; idempotent for repeat callers.
        let cat = load_resource_catalog();
        crate::economy::core_ids::install_catalog(cat.clone());
        cat
    }

    #[test]
    fn settled_subsistence_matches_legacy_defaults() {
        let cat = test_catalog();
        let caps = derive_from_legacy(Lifestyle::Settled, EconomyPreset::Subsistence, &cat);
        assert_eq!(caps.archetype_key, "settled_subsistence");
        assert!(caps.home.is_anchored());
        assert_eq!(caps.storage, StorageBackendKind::FactionTile);
        assert_eq!(caps.shelter, ShelterMode::Permanent);
        assert!(caps.settlement.is_full_settlement());
        assert!(caps.posting.enabled());
        assert!(caps.land.carves_plots);
        assert!(caps.economic_policy.is_empty(), "Subsistence = empty map");
        assert!(!caps.land.policy.state_sells_land);
    }

    #[test]
    fn settled_market_capitalist_on_every_resource() {
        let cat = test_catalog();
        let caps = derive_from_legacy(Lifestyle::Settled, EconomyPreset::Market, &cat);
        assert_eq!(caps.archetype_key, "settled_market");
        assert_eq!(caps.economic_policy.len(), cat.iter().count());
        for (_id, policy) in caps.economic_policy.iter() {
            assert!(policy.private_actors_allowed);
            assert!(!policy.chief_allocates_labor);
        }
        assert!(caps.land.policy.state_sells_land);
        assert!(caps.land.policy.private_freehold_allowed);
        assert!(caps.inheritance.seed_storage_tile);
        assert_eq!(caps.inheritance.seed_treasury, 15.0);
    }

    #[test]
    fn nomadic_disables_posting_and_uses_member_pool() {
        let cat = test_catalog();
        let caps = derive_from_legacy(Lifestyle::Nomadic, EconomyPreset::Subsistence, &cat);
        assert_eq!(caps.archetype_key, "nomadic_subsistence");
        assert!(caps.home.is_mobile());
        assert_eq!(caps.storage, StorageBackendKind::MemberPool);
        assert_eq!(caps.shelter, ShelterMode::Portable);
        assert!(caps.settlement.is_camp());
        assert!(caps.posting.is_disabled());
        assert!(!caps.land.carves_plots);
    }

    /// P5: `default_registry` populates entries for the four supported
    /// legacy archetypes; lookup returns capability bundles
    /// observably equal to `derive_from_legacy`.
    #[test]
    fn default_registry_carries_supported_archetypes() {
        let cat = test_catalog();
        let reg = default_registry(&cat);
        assert_eq!(reg.len(), 4);
        for key in [
            "settled_subsistence",
            "settled_mixed",
            "settled_market",
            "nomadic_subsistence",
        ] {
            assert!(reg.get(key).is_some(), "missing archetype: {key}");
        }
        assert!(
            reg.get("nomadic_market").is_none(),
            "nomadic_market is unsupported and must not be in the default registry"
        );
    }

    /// P5: `derive_from_archetype_key` prefers the registry; the
    /// fallback path is exercised when a key isn't authored yet.
    #[test]
    fn derive_from_archetype_key_prefers_registry() {
        let cat = test_catalog();
        let reg = default_registry(&cat);

        let from_key = derive_from_archetype_key(&reg, "settled_market", None)
            .expect("registry has settled_market");
        let from_legacy = derive_from_legacy(Lifestyle::Settled, EconomyPreset::Market, &cat);
        // Bundle equality where comparable: archetype_key + key flags.
        assert_eq!(from_key.archetype_key, from_legacy.archetype_key);
        assert_eq!(from_key.storage, from_legacy.storage);
        assert_eq!(from_key.posting, from_legacy.posting);
        assert_eq!(
            from_key.income.household_skim_pct,
            from_legacy.income.household_skim_pct
        );

        // Unknown key + no fallback → None.
        let missing = derive_from_archetype_key(&reg, "feudal_lord", None);
        assert!(missing.is_none());

        // Unknown key + legacy fallback → returns legacy bundle.
        let fallback = derive_from_archetype_key(
            &reg,
            "feudal_lord",
            Some((Lifestyle::Settled, EconomyPreset::Market, &cat)),
        );
        assert!(fallback.is_some());
        assert_eq!(fallback.unwrap().archetype_key, "settled_market");
    }

    /// P5: `load_archetype_registry` reads the on-disk RON files and
    /// produces capability bundles bit-for-bit identical to the
    /// pre-RON `default_registry` builder. Pins the regression
    /// invariant: switching the source from a hardcoded list to RON
    /// must not perturb any flag.
    #[test]
    fn ron_loaded_registry_matches_legacy_derivation() {
        let cat = test_catalog();
        let ron = load_archetype_registry(&cat);

        for (lifestyle, preset) in [
            (Lifestyle::Settled, EconomyPreset::Subsistence),
            (Lifestyle::Settled, EconomyPreset::Mixed),
            (Lifestyle::Settled, EconomyPreset::Market),
            (Lifestyle::Nomadic, EconomyPreset::Subsistence),
        ] {
            let key = legacy_archetype_key(lifestyle, preset);
            let from_ron = ron
                .get(key)
                .unwrap_or_else(|| panic!("RON registry missing {key}"));
            let from_legacy = derive_from_legacy(lifestyle, preset, &cat);

            assert_eq!(from_ron.archetype_key, from_legacy.archetype_key);
            assert_eq!(from_ron.home, from_legacy.home);
            assert_eq!(from_ron.storage, from_legacy.storage);
            assert_eq!(from_ron.shelter, from_legacy.shelter);
            assert_eq!(from_ron.settlement, from_legacy.settlement);
            assert_eq!(from_ron.posting, from_legacy.posting);
            assert_eq!(from_ron.land.carves_plots, from_legacy.land.carves_plots);
            assert_eq!(
                from_ron.land.eviction_policy,
                from_legacy.land.eviction_policy
            );
            assert_eq!(
                from_ron.land.policy.state_sells_land,
                from_legacy.land.policy.state_sells_land
            );
            assert_eq!(
                from_ron.land.policy.state_rents_land,
                from_legacy.land.policy.state_rents_land
            );
            assert_eq!(
                from_ron.land.policy.state_sharecrops,
                from_legacy.land.policy.state_sharecrops
            );
            assert_eq!(
                from_ron.income.household_skim_pct,
                from_legacy.income.household_skim_pct
            );
            assert_eq!(
                from_ron.economic_policy.len(),
                from_legacy.economic_policy.len(),
                "policy map size differs for {key}"
            );
            for (rid, p) in from_legacy.economic_policy.iter() {
                let q = from_ron
                    .economic_policy
                    .get(rid)
                    .unwrap_or_else(|| panic!("RON {key} missing policy for {:?}", rid));
                assert_eq!(p.chief_allocates_labor, q.chief_allocates_labor);
                assert_eq!(p.private_actors_allowed, q.private_actors_allowed);
            }
        }
    }

    #[test]
    fn settled_mixed_lands_in_between() {
        let cat = test_catalog();
        let caps = derive_from_legacy(Lifestyle::Settled, EconomyPreset::Mixed, &cat);
        assert_eq!(caps.archetype_key, "settled_mixed");
        assert!(caps.land.policy.state_rents_land);
        assert!(caps.land.policy.state_sharecrops);
        assert!(!caps.land.policy.state_sells_land);
        // Farm-planner: Mixed now applies `mixed()` to *every* resource,
        // including Wood/Stone/edibles (chief still allocates AND private
        // actors allowed). The previous skip prevented private grain.
        let wood = crate::economy::core_ids::wood();
        let stone = crate::economy::core_ids::stone();
        let wp = caps.economic_policy.get(&wood).copied().unwrap_or_default();
        let sp = caps.economic_policy.get(&stone).copied().unwrap_or_default();
        assert!(wp.private_actors_allowed);
        assert!(sp.private_actors_allowed);
        assert!(wp.chief_allocates_labor);
        assert!(sp.chief_allocates_labor);
    }
}
