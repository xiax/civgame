//! Data-driven plant species catalog.
//!
//! Species are loaded from `assets/data/plants/*.ron` at startup into a
//! `PlantCatalog` resource indexed by `PlantSpeciesId(u16)` (deterministic
//! alphabetical sort of `key`). Per-species attributes — biome/realm
//! eligibility, lifecycle, multi-profile harvest, sprite form, seed,
//! farm tile class — live in `PlantDef`, not in scattered match blocks on
//! `PlantKind`. The legacy 3-variant `PlantKind` enum stays as a coarse
//! taxonomic bucket (annual food crop / perennial shrub / tree); per-species
//! behavior routes through the catalog.
//!
//! Loaded once at `WorldPlugin::build` via `load_plant_catalog()` and
//! installed via `core_plant_ids::install_catalog(catalog)` so hot paths
//! can pull a process-global ref without a system param.

use crate::collections::AHashMap;
use bevy::prelude::*;
use serde::Deserialize;

use crate::economy::resource_catalog::{ResourceCatalog, ResourceId};
use crate::simulation::plants::{GrowthStage, PlantKind};
use crate::simulation::tools::ToolForm;
use crate::world::globe::Biome;
use crate::world::seasons::Season;
use crate::world::tile::TileKind;

// ---------------------------------------------------------------------------
// Identifiers and enums
// ---------------------------------------------------------------------------

/// Stable per-species id. Assigned at catalog-load time by sorting
/// `PlantDef.key` alphabetically. Sentinel `NONE = u16::MAX`.
#[derive(Copy, Clone, Eq, Hash, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlantSpeciesId(pub u16);

impl Default for PlantSpeciesId {
    fn default() -> Self {
        Self::NONE
    }
}

impl PlantSpeciesId {
    pub const NONE: Self = Self(u16::MAX);
    pub const fn raw(self) -> u16 {
        self.0
    }
    pub fn is_valid(self) -> bool {
        self.0 != u16::MAX
    }
}

/// Coarse morphological form. Drives sprite template selection and
/// scatter-mechanics class (annuals despawn winter, woody never despawn from
/// frost, etc.). Maps loosely onto `PlantKind` for back-compat.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlantForm {
    Grass,
    Forb,
    Shrub,
    Vine,
    Tree,
    Aquatic,
    Cactus,
    Tuber,
}

impl PlantForm {
    /// Every form, in stable order. Single source of truth for the
    /// form-fallback PNG generator and the sprite-audit tests so they can't
    /// drift from each other.
    pub const ALL: [PlantForm; 8] = [
        PlantForm::Grass,
        PlantForm::Forb,
        PlantForm::Shrub,
        PlantForm::Vine,
        PlantForm::Tree,
        PlantForm::Aquatic,
        PlantForm::Cactus,
        PlantForm::Tuber,
    ];

    /// Legacy form bucket for code paths that still match on `PlantKind`.
    /// Grass/Forb/Tuber/Cactus → Grain (annual food-crop-shaped); Shrub/Vine/
    /// Aquatic → BerryBush (perennial regrow-shaped); Tree → Tree.
    pub fn legacy_kind(self) -> PlantKind {
        match self {
            PlantForm::Grass | PlantForm::Forb | PlantForm::Tuber | PlantForm::Cactus => {
                PlantKind::Grain
            }
            PlantForm::Shrub | PlantForm::Vine | PlantForm::Aquatic => PlantKind::BerryBush,
            PlantForm::Tree => PlantKind::Tree,
        }
    }

    /// Vertical vegetation layer this form occupies in a plant community.
    /// Fill priority is `Canopy` → `Understory` → `GroundCover`, so a forest
    /// reads as scattered canopy trees with shrubs + grass filling between —
    /// one `Plant` per tile. Aquatic keeps its existing coastal/river gating
    /// and is classed GroundCover (it never competes with trees for a land tile).
    pub fn stratum(self) -> Stratum {
        match self {
            PlantForm::Tree => Stratum::Canopy,
            PlantForm::Shrub | PlantForm::Vine => Stratum::Understory,
            PlantForm::Grass
            | PlantForm::Forb
            | PlantForm::Tuber
            | PlantForm::Cactus
            | PlantForm::Aquatic => Stratum::GroundCover,
        }
    }
}

/// Vertical layer of a plant community (`chunk_streaming::spawn_chunk_plants`).
/// A tile fills the highest stratum that wins its independent per-tile roll;
/// claiming it for a higher stratum removes it from lower strata, preserving
/// the one-`Plant`-per-tile invariant.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stratum {
    Canopy,
    Understory,
    GroundCover,
}

impl Stratum {
    /// Fill order — lower index fills first. Index aligns with the
    /// `[canopy, understory, ground]` arrays in the seeder.
    pub const ORDER: [Stratum; 3] = [Stratum::Canopy, Stratum::Understory, Stratum::GroundCover];
}

/// Functional uses — used for memory tagging, recipe matching, UI display.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlantUse {
    Food,
    Grain,
    Fiber,
    Medicine,
    Dye,
    Resin,
    Oil,
    Latex,
    Reed,
    Wood,
    Bark,
    Luxury,
    Fodder,
}

/// Procedural floristic realm signature. A `FloraRegion` (`world/flora_regions.rs`)
/// carries one of these and the catalog filters wild-spawn candidates by
/// `def.native_realms.contains(region.realm)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FloraRealmKind {
    Boreal,
    Taiga,
    TempForest,
    Mediterranoid,
    GrasslandTemp,
    GrasslandTrop,
    RainforestTrop,
    DesertHot,
    DesertCold,
    MontaneTemp,
    MontaneTrop,
    CoastalWetland,
}

impl FloraRealmKind {
    pub const ALL: &'static [FloraRealmKind] = &[
        FloraRealmKind::Boreal,
        FloraRealmKind::Taiga,
        FloraRealmKind::TempForest,
        FloraRealmKind::Mediterranoid,
        FloraRealmKind::GrasslandTemp,
        FloraRealmKind::GrasslandTrop,
        FloraRealmKind::RainforestTrop,
        FloraRealmKind::DesertHot,
        FloraRealmKind::DesertCold,
        FloraRealmKind::MontaneTemp,
        FloraRealmKind::MontaneTrop,
        FloraRealmKind::CoastalWetland,
    ];

    pub fn name(self) -> &'static str {
        match self {
            FloraRealmKind::Boreal => "Boreal",
            FloraRealmKind::Taiga => "Taiga",
            FloraRealmKind::TempForest => "Temperate Forest",
            FloraRealmKind::Mediterranoid => "Mediterranean",
            FloraRealmKind::GrasslandTemp => "Temperate Grassland",
            FloraRealmKind::GrasslandTrop => "Tropical Grassland",
            FloraRealmKind::RainforestTrop => "Tropical Rainforest",
            FloraRealmKind::DesertHot => "Hot Desert",
            FloraRealmKind::DesertCold => "Cold Desert",
            FloraRealmKind::MontaneTemp => "Temperate Highland",
            FloraRealmKind::MontaneTrop => "Tropical Highland",
            FloraRealmKind::CoastalWetland => "Coastal Wetland",
        }
    }
}

/// Where in the plant lifecycle / season a harvest profile fires.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarvestTrigger {
    /// Plant must be in `GrowthStage::Mature`. Default for grain/berry-like.
    OnMature,
    /// Plant must be Mature AND `calendar.season` matches. Allows oak →
    /// acorns in Autumn, banana → fruit in Summer, etc., without felling.
    OnFruitSeason(SeasonWire),
    /// Felling a tree — destroys the plant, yields wood. Tool-gated.
    OnFell,
}

/// Wire-friendly mirror of `Season` for RON parsing (avoids needing serde
/// on the `Season` enum).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeasonWire {
    Spring,
    Summer,
    Autumn,
    Winter,
}

impl SeasonWire {
    pub fn into_season(self) -> Season {
        match self {
            SeasonWire::Spring => Season::Spring,
            SeasonWire::Summer => Season::Summer,
            SeasonWire::Autumn => Season::Autumn,
            SeasonWire::Winter => Season::Winter,
        }
    }
}

/// What kind of tile the species roots on. Used both by chunk-spawn and
/// scatter-target validation. RON authors pick from this enum; the catalog
/// translates to a `TileKind` predicate.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceTag {
    Grass,
    Forest,
    Sand,
    Snow,
    Marsh,
    Scrub,
    Cropland,
    SoilLike,
    Water,
    River,
}

impl SurfaceTag {
    /// True iff `kind` matches this surface tag.
    pub fn matches(self, kind: TileKind) -> bool {
        match self {
            SurfaceTag::Grass => matches!(kind, TileKind::Grass),
            SurfaceTag::Forest => matches!(kind, TileKind::Forest),
            SurfaceTag::Sand => matches!(kind, TileKind::Sand),
            SurfaceTag::Snow => matches!(kind, TileKind::Snow),
            SurfaceTag::Marsh => matches!(kind, TileKind::Marsh),
            SurfaceTag::Scrub => matches!(kind, TileKind::Scrub),
            SurfaceTag::Cropland => matches!(kind, TileKind::Cropland),
            SurfaceTag::SoilLike => kind.is_soil_like(),
            SurfaceTag::Water => matches!(kind, TileKind::Water),
            SurfaceTag::River => matches!(kind, TileKind::River),
        }
    }
}

/// Where in a farm plot this species can be sown. None = wild-only.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FarmTileClass {
    /// Standard annual / herbaceous on Cropland.
    Cropland,
    /// Perennial woody crop — Orchard plot (alias for Cropland in v1; flagged
    /// distinctly for future per-class plot kinds).
    Orchard,
    /// Wetland paddy — rice / taro. v1 routes through Cropland.
    Paddy,
}

// ---------------------------------------------------------------------------
// Spawn / lifecycle / harvest profiles
// ---------------------------------------------------------------------------

/// Inclusive u8 range for fertility / relief / river-distance acceptance.
#[derive(Copy, Clone, Debug, Deserialize)]
pub struct RangeU8 {
    pub min: u8,
    pub max: u8,
}

impl RangeU8 {
    pub fn contains(&self, v: u8) -> bool {
        v >= self.min && v <= self.max
    }
    pub const FULL: RangeU8 = RangeU8 { min: 0, max: 255 };
}

impl Default for RangeU8 {
    fn default() -> Self {
        Self::FULL
    }
}

/// Spatial spawn predicate for a wild scatter or chunk-stream placement.
#[derive(Clone, Debug, Deserialize)]
pub struct PlantSpawnRule {
    /// Acceptable biomes (matches `WorldCell.biome` at the cell containing
    /// the candidate tile).
    pub biomes: Vec<Biome>,
    /// Acceptable surface tile kinds.
    pub surface_tiles: Vec<SurfaceTag>,
    /// `TileData.fertility` band.
    #[serde(default)]
    pub fertility: RangeU8,
    /// Inclusive chebyshev distance to nearest river.
    #[serde(default)]
    pub river_distance: RangeU8,
    /// Species needs adjacency to ocean (kelp wrack / mangrove / coconut).
    #[serde(default)]
    pub requires_coastal: bool,
    /// Deterministic patch-noise gate. Set wavelength=0 to disable.
    #[serde(default)]
    pub patch_noise: PatchNoise,
    /// Base weight for the weighted candidate lottery — higher = denser.
    pub base_weight: u16,
}

#[derive(Copy, Clone, Debug, Deserialize)]
pub struct PatchNoise {
    pub wavelength_tiles: u16,
    pub threshold: u8,
}

impl Default for PatchNoise {
    fn default() -> Self {
        Self {
            wavelength_tiles: 0,
            threshold: 0,
        }
    }
}

/// Calendar-driven growth profile. Replaces `season_growth` + `stage_threshold`.
#[derive(Clone, Debug, Deserialize)]
pub struct PlantLifecycleProfile {
    /// True if the species despawns at Winter onset (Grain-like annual).
    pub annual: bool,
    /// Seasons during which a chief / household may sow this species.
    pub sowing_seasons: Vec<SeasonWire>,
    /// Threshold-to-leave per `GrowthStage` (Seed, Seedling, Harvested,
    /// Mature, Overripe). Overripe always 0. RON authors as `[a,b,c,d,e]`.
    pub stage_thresholds: Vec<u16>,
    /// Growth points added at season-end, indexed Spring=0..Winter=3.
    pub season_growth: Vec<u8>,
    /// Probability/100 a Seed sprouts on threshold cross (wild only;
    /// cultivated always succeed).
    pub wild_sprout_chance: u8,
    /// Probability/100 a Mature plant scatters one seed at maturity tick.
    pub scatter_chance: u8,
    /// Chebyshev radius for scatter target picks.
    pub scatter_radius: u8,
    /// If true, harvest reverts to a regrowable stage (BerryBush-like).
    pub regrow_after_harvest: bool,
    /// Nitrogen-fixing flag (legumes) — crediting nutrients post-harvest.
    /// V1 ignores; reserved for soil cycle expansion.
    #[serde(default)]
    pub nitrogen_fixing: bool,
}

/// A single harvest profile. A species may carry several (e.g. oak has
/// `OnFruitSeason(Autumn) → acorns, plant remains` AND `OnFell → wood,
/// despawns`).
#[derive(Clone, Debug, Deserialize)]
pub struct PlantHarvestProfile {
    pub trigger: HarvestTrigger,
    /// `None` = bare hands. Some(form) = required tool form (Axe for fell,
    /// Sickle for grain, Knife for fiber stems, Pick is unused here).
    #[serde(default)]
    pub tool: Option<ToolForm>,
    pub work_ticks: u16,
    /// (resource_key, qty) pairs. Resource keys resolve to `ResourceId`
    /// at catalog-load time; missing keys panic at load (failing fast).
    pub yields: Vec<(String, u32)>,
    /// True → remove plant after this harvest (despawn). False → revert
    /// to `stage_after`.
    pub despawn: bool,
    /// Stage to transition into when `despawn=false`.
    #[serde(default = "default_stage_after")]
    pub stage_after: StageWire,
    /// Activity logged for tech-discovery routing.
    pub activity: HarvestActivityWire,
    /// Skill granted XP per harvest.
    pub skill: HarvestSkillWire,
    pub skill_xp: u16,
}

fn default_stage_after() -> StageWire {
    StageWire::Harvested
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageWire {
    Seed,
    Seedling,
    Harvested,
    Mature,
    Overripe,
}

impl StageWire {
    pub fn into_growth_stage(self) -> GrowthStage {
        match self {
            StageWire::Seed => GrowthStage::Seed,
            StageWire::Seedling => GrowthStage::Seedling,
            StageWire::Harvested => GrowthStage::Harvested,
            StageWire::Mature => GrowthStage::Mature,
            StageWire::Overripe => GrowthStage::Overripe,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarvestActivityWire {
    Farming,
    Foraging,
    WoodGathering,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarvestSkillWire {
    Farming,
    Building,
    Foraging,
}

// ---------------------------------------------------------------------------
// PlantDef + PlantCatalog
// ---------------------------------------------------------------------------

/// Phase 6 per-species sprite override. Empty default means "use the
/// species `key` as the folder name and the default 3-variant layout";
/// populate `folder` to share art between near-identical species
/// (`crabapple` → `apple_tree`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PlantSpriteKeys {
    /// Asset subfolder under `assets/textures/plants/`. `None` = species key.
    #[serde(default)]
    pub folder: Option<String>,
    /// Override variant count for Seedling/Mature/Overripe (1..=3). `None` = 3.
    #[serde(default)]
    pub variants: Option<u8>,
}

/// One plant species definition.
#[derive(Clone, Debug, Deserialize)]
pub struct PlantDef {
    pub key: String,
    pub display_name: String,
    pub form: PlantForm,
    pub uses: Vec<PlantUse>,
    pub native_realms: Vec<FloraRealmKind>,
    pub spawn: PlantSpawnRule,
    pub lifecycle: PlantLifecycleProfile,
    pub harvests: Vec<PlantHarvestProfile>,
    /// Seed resource key (alphabetical sort-stable). `None` for wild-only
    /// species that can't be cultivated.
    #[serde(default)]
    pub seed: Option<String>,
    #[serde(default)]
    pub farm_tile_class: Option<FarmTileClass>,
    /// Clear-obstacle work ticks (felling/clearing the live plant).
    pub clear_work_ticks: u32,
    #[serde(default)]
    pub sprite_keys: PlantSpriteKeys,
}

/// Resolved view consumed by hot paths — same shape as `PlantDef` but with
/// resource keys pre-looked-up to `ResourceId`.
#[derive(Clone, Debug)]
pub struct ResolvedPlantDef {
    pub id: PlantSpeciesId,
    pub key: String,
    pub display_name: String,
    pub form: PlantForm,
    pub uses: Vec<PlantUse>,
    pub native_realms: Vec<FloraRealmKind>,
    pub spawn: PlantSpawnRule,
    pub lifecycle: PlantLifecycleProfile,
    pub harvests: Vec<ResolvedHarvestProfile>,
    pub seed: Option<ResourceId>,
    pub farm_tile_class: Option<FarmTileClass>,
    pub clear_work_ticks: u32,
    pub sprite_keys: PlantSpriteKeys,
}

#[derive(Clone, Debug)]
pub struct ResolvedHarvestProfile {
    pub trigger: HarvestTrigger,
    pub tool: Option<ToolForm>,
    pub work_ticks: u16,
    pub yields: Vec<(ResourceId, u32)>,
    pub despawn: bool,
    pub stage_after: GrowthStage,
    pub activity: HarvestActivityWire,
    pub skill: HarvestSkillWire,
    pub skill_xp: u16,
}

impl ResolvedPlantDef {
    /// Coarse `PlantKind` bucket (derived from `form`).
    pub fn legacy_kind(&self) -> PlantKind {
        self.form.legacy_kind()
    }
    /// True if this species' harvest yields any food (drives `AnyEdible`
    /// memory tag during vision sweeps).
    pub fn yields_food(&self) -> bool {
        self.harvests.iter().any(|h| {
            h.yields.iter().any(|(rid, _)| {
                matches!(
                    rid.class(),
                    Some(crate::economy::resource_catalog::ResourceClass::Food)
                )
            })
        })
    }
    /// Primary non-food yield (first non-food resource across profiles)
    /// for memory tagging. None if every yield is food.
    pub fn primary_non_food_yield(&self) -> Option<ResourceId> {
        for h in &self.harvests {
            for (rid, _) in &h.yields {
                if !matches!(
                    rid.class(),
                    Some(crate::economy::resource_catalog::ResourceClass::Food)
                ) {
                    return Some(*rid);
                }
            }
        }
        None
    }
    /// True if this species can be deliberately sown.
    pub fn is_farm_plantable(&self) -> bool {
        self.seed.is_some()
    }
    /// True if `season` is in the sowing window.
    pub fn is_sowable_in(&self, season: Season) -> bool {
        self.lifecycle
            .sowing_seasons
            .iter()
            .any(|s| s.into_season() == season)
    }
    /// Asset-folder slug for sprite PNGs — `sprite_keys.folder` override
    /// or the species key when unset.
    pub fn sprite_folder(&self) -> &str {
        self.sprite_keys
            .folder
            .as_deref()
            .unwrap_or(self.key.as_str())
    }
    /// Variant count for multi-variant stages (Seedling/Mature/Overripe).
    /// Single-variant stages (Seed/Harvested) always use 1 regardless.
    pub fn sprite_variants(&self) -> u8 {
        self.sprite_keys.variants.unwrap_or(3).clamp(1, 3)
    }
}

#[derive(Resource, Default, Clone, Debug)]
pub struct PlantCatalog {
    defs: Vec<ResolvedPlantDef>,
    by_key: AHashMap<String, PlantSpeciesId>,
    by_seed: AHashMap<ResourceId, PlantSpeciesId>,
    /// Cached "default species" per legacy `PlantKind` — picked at load by
    /// finding the first species whose `legacy_kind()` matches the kind.
    /// Used by `PlantKind::default_species()` for legacy spawn paths.
    default_grain: PlantSpeciesId,
    default_berry: PlantSpeciesId,
    default_tree: PlantSpeciesId,
    /// Phase 8: pre-computed per-(realm, biome) species pool. The
    /// chunk-stream lottery used to walk the full ~50-species catalog per
    /// tile; with this cache it touches at most the species native to the
    /// tile's realm AND that list `biome` under `spawn.biomes`. Coastal/
    /// fertility/surface-tile/patch-noise gates still run per tile (they
    /// depend on the tile's local context).
    native_pools: AHashMap<(FloraRealmKind, crate::world::globe::Biome), Vec<PlantSpeciesId>>,
}

impl PlantCatalog {
    /// Build a catalog from a list of `PlantDef`s + a resolved `ResourceCatalog`.
    /// Panics on:
    ///   - duplicate species `key`
    ///   - missing resource key in any harvest yield
    ///   - missing seed resource for a species with `seed: Some(...)`
    pub fn from_defs(mut defs: Vec<PlantDef>, resources: &ResourceCatalog) -> Self {
        defs.sort_by(|a, b| a.key.cmp(&b.key));

        let mut resolved = Vec::with_capacity(defs.len());
        let mut by_key = AHashMap::with_capacity_and_hasher(defs.len(), crate::collections::FixedState);
        let mut by_seed = AHashMap::default();

        for (idx, def) in defs.into_iter().enumerate() {
            assert!(
                idx < u16::MAX as usize,
                "PlantCatalog: more than {} species",
                u16::MAX
            );
            let id = PlantSpeciesId(idx as u16);
            if by_key.insert(def.key.clone(), id).is_some() {
                panic!("PlantCatalog: duplicate species key {:?}", def.key);
            }

            let seed = def.seed.as_ref().map(|k| {
                resources
                    .id_of(k)
                    .unwrap_or_else(|| panic!("PlantCatalog: seed key {:?} not in ResourceCatalog (species {:?})", k, def.key))
            });
            if let Some(rid) = seed {
                by_seed.insert(rid, id);
            }

            let harvests = def
                .harvests
                .into_iter()
                .map(|h| ResolvedHarvestProfile {
                    trigger: h.trigger,
                    tool: h.tool,
                    work_ticks: h.work_ticks,
                    yields: h
                        .yields
                        .into_iter()
                        .map(|(k, qty)| {
                            let rid = resources.id_of(&k).unwrap_or_else(|| {
                                panic!(
                                    "PlantCatalog: harvest yield key {:?} not in \
                                     ResourceCatalog (species {:?})",
                                    k, def.key
                                )
                            });
                            (rid, qty)
                        })
                        .collect(),
                    despawn: h.despawn,
                    stage_after: h.stage_after.into_growth_stage(),
                    activity: h.activity,
                    skill: h.skill,
                    skill_xp: h.skill_xp,
                })
                .collect();

            resolved.push(ResolvedPlantDef {
                id,
                key: def.key,
                display_name: def.display_name,
                form: def.form,
                uses: def.uses,
                native_realms: def.native_realms,
                spawn: def.spawn,
                lifecycle: def.lifecycle,
                harvests,
                seed,
                farm_tile_class: def.farm_tile_class,
                clear_work_ticks: def.clear_work_ticks,
                sprite_keys: def.sprite_keys,
            });
        }

        // Pick legacy defaults — first species whose form matches each
        // `PlantKind`. Authoring contract: at least one species per form
        // (test asserts this).
        let mut default_grain = PlantSpeciesId::NONE;
        let mut default_berry = PlantSpeciesId::NONE;
        let mut default_tree = PlantSpeciesId::NONE;
        for d in &resolved {
            match d.form.legacy_kind() {
                PlantKind::Grain if !default_grain.is_valid() => default_grain = d.id,
                PlantKind::BerryBush if !default_berry.is_valid() => default_berry = d.id,
                PlantKind::Tree if !default_tree.is_valid() => default_tree = d.id,
                _ => {}
            }
        }

        // Prefer canonical legacy keys when present so existing test
        // fixtures spawning `PlantKind::Grain` reliably land on emmer_wheat
        // rather than a different alphabetically-prior grain.
        if let Some(&id) = by_key.get("emmer_wheat") {
            default_grain = id;
        }
        if let Some(&id) = by_key.get("generic_berry_bush") {
            default_berry = id;
        }
        if let Some(&id) = by_key.get("oak_tree") {
            default_tree = id;
        }

        // Phase 8: build the per-(realm, biome) pool by walking the
        // catalog once. ~50 species × 12 realms × 11 biomes is < 1k
        // entries in the worst case; in practice each (realm, biome) cell
        // gets 0-12 entries.
        let mut native_pools: AHashMap<
            (FloraRealmKind, crate::world::globe::Biome),
            Vec<PlantSpeciesId>,
        > = AHashMap::default();
        for d in &resolved {
            for &realm in &d.native_realms {
                for &biome in &d.spawn.biomes {
                    native_pools
                        .entry((realm, biome))
                        .or_default()
                        .push(d.id);
                }
            }
        }

        Self {
            defs: resolved,
            by_key,
            by_seed,
            default_grain,
            default_berry,
            default_tree,
            native_pools,
        }
    }

    /// Phase 8: read the pre-built native species pool for a given
    /// `(realm, biome)`. `chunk_streaming::spawn_chunk_plants` consults
    /// this to skip the full catalog walk per tile.
    pub fn native_pool_for(
        &self,
        realm: FloraRealmKind,
        biome: crate::world::globe::Biome,
    ) -> &[PlantSpeciesId] {
        self.native_pools
            .get(&(realm, biome))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
    pub fn def(&self, id: PlantSpeciesId) -> Option<&ResolvedPlantDef> {
        if !id.is_valid() {
            return None;
        }
        self.defs.get(id.0 as usize)
    }
    pub fn def_unchecked(&self, id: PlantSpeciesId) -> &ResolvedPlantDef {
        &self.defs[id.0 as usize]
    }
    pub fn id_of(&self, key: &str) -> Option<PlantSpeciesId> {
        self.by_key.get(key).copied()
    }
    pub fn species_of_seed(&self, seed_rid: ResourceId) -> Option<PlantSpeciesId> {
        self.by_seed.get(&seed_rid).copied()
    }
    pub fn iter(&self) -> impl Iterator<Item = &ResolvedPlantDef> {
        self.defs.iter()
    }
    pub fn default_for_kind(&self, kind: PlantKind) -> PlantSpeciesId {
        match kind {
            PlantKind::Grain => self.default_grain,
            PlantKind::BerryBush => self.default_berry,
            PlantKind::Tree => self.default_tree,
        }
    }

    /// True iff this species is native to `realm`. Wild-spawn predicate.
    pub fn is_native_to(&self, species: PlantSpeciesId, realm: FloraRealmKind) -> bool {
        self.def(species)
            .map(|d| d.native_realms.contains(&realm))
            .unwrap_or(false)
    }

    /// Pick a harvest profile for a worker at a plant in `(stage, season)`
    /// carrying tools `tools`. Resolution order:
    ///   1. profile matches stage + season
    ///   2. tool requirement satisfied by `has_tool(form)`
    ///   3. prefer non-despawn profile over despawn (so an axe-carrying
    ///      forager picking acorns from an oak in Autumn gets the fruit
    ///      profile, not the fell profile, unless the worker specifically
    ///      wants wood — caller-side override is via filter).
    pub fn pick_harvest_profile<'a>(
        &'a self,
        species: PlantSpeciesId,
        stage: GrowthStage,
        season: Season,
        has_tool: impl Fn(ToolForm) -> bool,
        prefer_despawn: bool,
    ) -> Option<&'a ResolvedHarvestProfile> {
        let def = self.def(species)?;
        let mut best: Option<&ResolvedHarvestProfile> = None;
        for h in &def.harvests {
            let trigger_ok = match h.trigger {
                HarvestTrigger::OnMature => stage == GrowthStage::Mature,
                HarvestTrigger::OnFruitSeason(s) => {
                    stage == GrowthStage::Mature && s.into_season() == season
                }
                HarvestTrigger::OnFell => stage == GrowthStage::Mature,
            };
            if !trigger_ok {
                continue;
            }
            if let Some(form) = h.tool {
                if !has_tool(form) {
                    continue;
                }
            }
            match best {
                None => best = Some(h),
                Some(prev) => {
                    // Tie-break: caller's preference wins.
                    if prefer_despawn && h.despawn && !prev.despawn {
                        best = Some(h);
                    } else if !prefer_despawn && !h.despawn && prev.despawn {
                        best = Some(h);
                    }
                }
            }
        }
        best
    }
}

// ---------------------------------------------------------------------------
// Load helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PlantFile {
    pub plants: Vec<PlantDef>,
}

/// Load every `*.ron` under `assets/data/plants/`, parse, merge.
pub fn load_plant_catalog(resources: &ResourceCatalog) -> PlantCatalog {
    let dir = std::path::Path::new("assets/data/plants");
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "PlantCatalog: cannot read {:?}: {}. Plant definition files must \
             live in assets/data/plants/*.ron.",
            dir, e
        )
    });

    let mut defs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("PlantCatalog: cannot read {:?}: {}", path, e));
        let file: PlantFile = ron::from_str(&body)
            .unwrap_or_else(|e| panic!("PlantCatalog: parse error in {:?}: {}", path, e));
        defs.extend(file.plants);
    }

    if defs.is_empty() {
        panic!(
            "PlantCatalog: no plant species found in {:?}.",
            dir
        );
    }
    PlantCatalog::from_defs(defs, resources)
}

// ---------------------------------------------------------------------------
// Process-global catalog (mirrors core_ids pattern)
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

static CATALOG: OnceLock<PlantCatalog> = OnceLock::new();

/// Install the process-global catalog. Called once at `WorldPlugin::build`.
/// Idempotent on repeated calls with `Plays::Local` reloads — but the value
/// stays the value installed by the first call.
pub fn install_catalog(cat: PlantCatalog) {
    let _ = CATALOG.set(cat);
}

/// Process-global catalog accessor — lazy-loads from disk on first call if
/// no catalog has been installed (for tests / fixtures).
pub fn catalog() -> &'static PlantCatalog {
    CATALOG.get_or_init(|| {
        let resources = crate::economy::core_ids::catalog().clone();
        load_plant_catalog(&resources)
    })
}

/// Reset for tests — best-effort no-op since OnceLock can't reset. Tests
/// must install once at process start.
#[cfg(test)]
pub fn _has_catalog() -> bool {
    CATALOG.get().is_some()
}

// ---------------------------------------------------------------------------
// Tests — catalog invariants (Phase 7 of biome-native plants follow-ups).
// Loads the on-disk catalog through the same code path runtime uses so
// regressions in `assets/data/plants/*.ron` fail fast.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::simulation::plants::PlantKind;

    fn load() -> PlantCatalog {
        let resources = crate::economy::resource_catalog::load_resource_catalog();
        load_plant_catalog(&resources)
    }

    #[test]
    fn every_species_has_native_realms() {
        let cat = load();
        assert!(!cat.is_empty(), "catalog must have at least one species");
        for def in cat.iter() {
            assert!(
                !def.native_realms.is_empty(),
                "species {:?} has no native_realms — wild spawn would never fire",
                def.key
            );
        }
    }

    #[test]
    fn every_plantable_seed_roundtrips_through_species_of_seed() {
        let cat = load();
        for def in cat.iter() {
            if let Some(seed_rid) = def.seed {
                let resolved = cat.species_of_seed(seed_rid);
                assert_eq!(
                    resolved,
                    Some(def.id),
                    "species {:?} seed {:?} does not round-trip via species_of_seed",
                    def.key,
                    seed_rid
                );
            }
        }
    }

    #[test]
    fn legacy_keys_resolve_to_defaults() {
        let cat = load();
        let grain = cat.id_of("emmer_wheat").expect("emmer_wheat present");
        let berry = cat.id_of("generic_berry_bush").expect("generic_berry_bush present");
        let tree = cat.id_of("oak_tree").expect("oak_tree present");
        assert_eq!(cat.default_for_kind(PlantKind::Grain), grain);
        assert_eq!(cat.default_for_kind(PlantKind::BerryBush), berry);
        assert_eq!(cat.default_for_kind(PlantKind::Tree), tree);
    }

    #[test]
    fn every_biome_has_at_least_one_native_species() {
        use crate::world::globe::Biome;
        let cat = load();
        // Ocean is the one biome no plant should claim. Every other land
        // biome should have at least one species that lists it under
        // `spawn.biomes` so chunk-gen never finds an empty candidate list
        // for a typical land tile.
        let biomes = [
            Biome::Tundra,
            Biome::Taiga,
            Biome::Temperate,
            Biome::Grassland,
            Biome::Tropical,
            Biome::Desert,
            Biome::Mountain,
            Biome::Wetland,
            Biome::Steppe,
            Biome::Badlands,
        ];
        for b in biomes {
            let count = cat
                .iter()
                .filter(|d| d.spawn.biomes.contains(&b))
                .count();
            assert!(
                count >= 1,
                "biome {:?} has no native species",
                b
            );
        }
    }

    #[test]
    fn every_form_has_at_least_one_species() {
        let cat = load();
        for form in [
            PlantForm::Grass,
            PlantForm::Forb,
            PlantForm::Shrub,
            PlantForm::Tree,
        ] {
            assert!(
                cat.iter().any(|d| d.form == form),
                "form {:?} has no species",
                form
            );
        }
    }
}
