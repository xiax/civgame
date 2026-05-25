//! Building-technique layer (Phase E of the knowledge-system overhaul).
//!
//! `WallMaterial` is the render/combat surface — five tiers, kept untouched.
//! `BuildingTechnique` is the cultural how-to: a specific method a faction
//! knows for putting up a wall, roof, or shelter. One technique requires one
//! or more `KnowledgeId` gates; each technique maps to exactly one
//! `WallMaterial` for the construction recipe + sprite path.
//!
//! Phase E ships the data model only. Phase E.2 wires `select_building_technique`
//! into `organic_settlement::pressure_to_intent` / `shelter_kind` so plan
//! choice reads local-site material availability instead of the legacy
//! era-keyed `WALL_LADDER_BY_TECH` fallback.

use crate::simulation::construction::WallMaterial;
use crate::simulation::faction::FactionTechs;
use crate::simulation::knowledge_catalog::KnowledgeId;
use crate::simulation::technology::{
    ADOBE_BRICK, ASHLAR_DRESSING, COB_WALLING, CUT_STONE_MASONRY, DRY_STONE_WALLING,
    HYDRAULIC_MASONRY, MUDBRICK_MOULDING, PIT_HOUSE, REED_MATTING, STAKE_AND_HIDE_TENT,
    THATCH_ROOFING, TIMBER_LONGHOUSE_FRAMING, WATTLE_AND_DAUB, WATTLE_SCREENS,
};
use crate::world::locality::LocalSiteContext;
use crate::world::globe::Biome;

/// One known way of erecting a wall, roof, or shelter. Each technique is
/// gated by one or more `KnowledgeId` (see [`BuildingTechnique::knowledge_gates`])
/// and maps to a `WallMaterial` for the recipe + sprite path
/// ([`BuildingTechnique::output_material`]). Roof-only or shelter-form
/// techniques (Thatch, Pit House, Stake-and-Hide, Reed Matting) map to
/// `WallMaterial::Palisade` as the closest substrate: the cultural choice
/// rides on the technique, the surface stays render-compatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuildingTechnique {
    /// Hide stretched over poles — the Paleolithic portable shelter, nomad
    /// default before Yurt-class dwellings.
    StakeAndHideTent,
    /// Bundled reeds woven into wall screens. Mesolithic wetland
    /// vernacular; mat-faced palisades.
    ReedMatting,
    /// Pliable hazel/willow rods woven through stakes — light enclosures
    /// and palisade-class wind-breaks.
    WattleScreens,
    /// Semi-subterranean dwelling with earth berm walls and a light timber
    /// roof. Mesolithic-Neolithic.
    PitHouse,
    /// Wattle screens packed with daub (clay+straw); the canonical
    /// Neolithic European farmstead wall.
    WattleAndDaub,
    /// Heavy timber framing — paired oak posts, tie-beams, gable rafters.
    /// Drives the Longhouse axis-aware layout.
    TimberLonghouseFraming,
    /// Bundled-straw or reed thatch on rafters. A roof technique; pairs
    /// with WattleAndDaub / TimberLonghouseFraming on the same dwelling.
    ThatchRoofing,
    /// Monolithic earth walls (clay+sand+straw laid wet in courses).
    /// Mudbrick-class without moulded bricks.
    CobWalling,
    /// Sun-dried clay-and-straw bricks moulded in wooden frames. Arid /
    /// hot-dry vernacular.
    AdobeBrick,
    /// Standardised mudbrick production: uniform courses, faster walls.
    /// Basis for ancient urban dwellings.
    MudbrickMoulding,
    /// Coursed field-stone laid without mortar. Rural enclosure walls,
    /// long-lived; the Neolithic stone vernacular.
    DryStoneWalling,
    /// Quarried blocks dressed with copper tools, laid in lime-mortar
    /// courses. Defensive curtain walls.
    CutStoneMasonry,
    /// Precisely-squared and finely-dressed stone blocks. Palace /
    /// monumental masonry.
    AshlarDressing,
    /// Mortar-and-rubble with lime-set joints that cure underwater —
    /// watercourses, cisterns, harbour works.
    HydraulicMasonry,
}

impl BuildingTechnique {
    /// Every technique. Order is era-ascending so `Iterator::find` patterns
    /// pick simpler techniques first when ties don't resolve elsewhere.
    pub const ALL: [BuildingTechnique; 14] = [
        BuildingTechnique::StakeAndHideTent,
        BuildingTechnique::ReedMatting,
        BuildingTechnique::WattleScreens,
        BuildingTechnique::PitHouse,
        BuildingTechnique::WattleAndDaub,
        BuildingTechnique::TimberLonghouseFraming,
        BuildingTechnique::ThatchRoofing,
        BuildingTechnique::CobWalling,
        BuildingTechnique::AdobeBrick,
        BuildingTechnique::MudbrickMoulding,
        BuildingTechnique::DryStoneWalling,
        BuildingTechnique::CutStoneMasonry,
        BuildingTechnique::AshlarDressing,
        BuildingTechnique::HydraulicMasonry,
    ];

    /// `KnowledgeId`s a faction must have **all** Learned (poster-pool union)
    /// to use this technique. Returns a single-element slice for techniques
    /// whose gate is one tech.
    pub fn knowledge_gates(self) -> &'static [KnowledgeId] {
        match self {
            BuildingTechnique::StakeAndHideTent => &[STAKE_AND_HIDE_TENT],
            BuildingTechnique::ReedMatting => &[REED_MATTING],
            BuildingTechnique::WattleScreens => &[WATTLE_SCREENS],
            BuildingTechnique::PitHouse => &[PIT_HOUSE],
            BuildingTechnique::WattleAndDaub => &[WATTLE_AND_DAUB],
            BuildingTechnique::TimberLonghouseFraming => &[TIMBER_LONGHOUSE_FRAMING],
            BuildingTechnique::ThatchRoofing => &[THATCH_ROOFING],
            BuildingTechnique::CobWalling => &[COB_WALLING],
            BuildingTechnique::AdobeBrick => &[ADOBE_BRICK],
            BuildingTechnique::MudbrickMoulding => &[MUDBRICK_MOULDING],
            BuildingTechnique::DryStoneWalling => &[DRY_STONE_WALLING],
            BuildingTechnique::CutStoneMasonry => &[CUT_STONE_MASONRY],
            BuildingTechnique::AshlarDressing => &[ASHLAR_DRESSING],
            BuildingTechnique::HydraulicMasonry => &[HYDRAULIC_MASONRY],
        }
    }

    /// The `WallMaterial` this technique produces when applied to a wall.
    /// Roof / shelter techniques (Thatch, Pit House, Reed Matting,
    /// Stake-and-Hide) map to the closest existing wall tier — they're
    /// cultural variants that ride on top of the palisade-class substrate.
    pub fn output_material(self) -> WallMaterial {
        match self {
            BuildingTechnique::StakeAndHideTent
            | BuildingTechnique::ReedMatting
            | BuildingTechnique::WattleScreens
            | BuildingTechnique::PitHouse
            | BuildingTechnique::ThatchRoofing
            | BuildingTechnique::TimberLonghouseFraming => WallMaterial::Palisade,
            BuildingTechnique::WattleAndDaub => WallMaterial::WattleDaub,
            BuildingTechnique::CobWalling
            | BuildingTechnique::AdobeBrick
            | BuildingTechnique::MudbrickMoulding => WallMaterial::Mudbrick,
            BuildingTechnique::DryStoneWalling => WallMaterial::Stone,
            BuildingTechnique::CutStoneMasonry => WallMaterial::CutStone,
            BuildingTechnique::AshlarDressing => WallMaterial::CutStone,
            BuildingTechnique::HydraulicMasonry => WallMaterial::CutStone,
        }
    }

    /// Short human-readable label for inspector / activity log surfaces.
    pub fn label(self) -> &'static str {
        match self {
            BuildingTechnique::StakeAndHideTent => "Stake-and-Hide Tent",
            BuildingTechnique::ReedMatting => "Reed Matting",
            BuildingTechnique::WattleScreens => "Wattle Screens",
            BuildingTechnique::PitHouse => "Pit House",
            BuildingTechnique::WattleAndDaub => "Wattle and Daub",
            BuildingTechnique::TimberLonghouseFraming => "Timber Longhouse",
            BuildingTechnique::ThatchRoofing => "Thatch Roofing",
            BuildingTechnique::CobWalling => "Cob Walling",
            BuildingTechnique::AdobeBrick => "Adobe Brick",
            BuildingTechnique::MudbrickMoulding => "Mudbrick Moulding",
            BuildingTechnique::DryStoneWalling => "Dry-Stone Walling",
            BuildingTechnique::CutStoneMasonry => "Cut Stone Masonry",
            BuildingTechnique::AshlarDressing => "Ashlar Dressing",
            BuildingTechnique::HydraulicMasonry => "Hydraulic Masonry",
        }
    }

    /// Whether the technique is a roofing technique (vs a wall technique).
    /// Roof-only techniques pair with a wall technique on the same site;
    /// Phase E.2's selector treats them as complements not competitors.
    pub fn is_roof_technique(self) -> bool {
        matches!(self, BuildingTechnique::ThatchRoofing)
    }

    /// Whether the technique produces a portable / temporary shelter
    /// (nomadic camp variants — read by `nomad::seed_nomadic_camp` once
    /// E.2's selector is wired through).
    pub fn is_portable_shelter(self) -> bool {
        matches!(
            self,
            BuildingTechnique::StakeAndHideTent | BuildingTechnique::ReedMatting
        )
    }
}

/// What the structure is for. Drives technique selection — a defensive curtain
/// wall prefers Dry-Stone / Cut Stone; a dwelling prefers WattleAndDaub /
/// Timber Longhouse; a hydraulic work prefers Hydraulic Masonry; a nomad
/// shelter prefers Stake-and-Hide / Reed Matting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructurePurpose {
    /// A permanent dwelling — Hut / Longhouse / urban courtyard house.
    Dwelling,
    /// A storage building — Granary / Warehouse.
    Storage,
    /// A civic / institutional building — Market / Shrine / Table / Monument.
    Civic,
    /// A defensive curtain wall or palisade.
    Defensive,
    /// A hydraulic engineering work — Dam / aqueduct / cistern.
    Hydraulic,
    /// A nomadic shelter — packable when the band moves.
    NomadShelter,
}

/// Pure-fn building-technique selector. Phase E.2 of the knowledge-system
/// overhaul.
///
/// Filters [`BuildingTechnique::ALL`] to those whose every gate is in
/// `techs.has(...)`, scores each surviving candidate by purpose match +
/// locality fit, and returns the best one. Returns `None` when no candidate
/// passes the knowledge gate — callers fall back to the legacy
/// `select_wall_material` ladder.
///
/// The selector is intentionally read-only of [`MaterialAvailabilityView`]
/// (the existing wall-material selector continues to drive market-haul /
/// emergency-shelter logic on the resulting `WallMaterial`). Phase E.2's
/// contribution is the *cultural* layer — given the same Bronze-Age
/// chief-tech set, a forest faction prefers Timber, a clay-river faction
/// prefers Mudbrick, a temperate-stone faction prefers Dry-Stone.
pub fn select_building_technique(
    techs: &FactionTechs,
    locality: Option<&LocalSiteContext>,
    purpose: StructurePurpose,
) -> Option<BuildingTechnique> {
    let mut best: Option<(BuildingTechnique, i32)> = None;
    for t in BuildingTechnique::ALL {
        // Hard gate (a): faction must know every gating KnowledgeId.
        if !t.knowledge_gates().iter().all(|&id| techs.has(id)) {
            continue;
        }
        // Roof techniques don't stand alone as a wall choice — Phase E.2's
        // selector returns wall techniques. Phase F's recipe pass will
        // chain a roof technique alongside.
        if t.is_roof_technique() {
            continue;
        }
        // Purpose filter: a NomadShelter purpose only accepts portable
        // shelters; everything else rejects portable shelters.
        match (purpose, t.is_portable_shelter()) {
            (StructurePurpose::NomadShelter, false) => continue,
            (p, true) if !matches!(p, StructurePurpose::NomadShelter) => continue,
            _ => {}
        }
        let score = score_technique(t, locality, purpose);
        let better = match best {
            None => true,
            Some((_, best_score)) => score > best_score,
        };
        if better {
            best = Some((t, score));
        }
    }
    best.map(|(t, _)| t)
}

/// Score a candidate technique against locality + purpose. Higher = better.
/// Locality is optional — `None` collapses to "no terrain bonus", at which
/// point ties resolve by `purpose_score` alone (deterministic when no
/// locality is known, e.g. headless fixtures + emergency fallback).
fn score_technique(
    t: BuildingTechnique,
    locality: Option<&LocalSiteContext>,
    purpose: StructurePurpose,
) -> i32 {
    let mut s = purpose_score(t, purpose);
    if let Some(loc) = locality {
        s += locality_score(t, loc);
    }
    s
}

/// Purpose-match score, in `[0, 100]`. Read top-down: a defensive purpose
/// scores stone-class techniques highest; a dwelling scores frame / earthen
/// dwelling techniques; a hydraulic scores Hydraulic Masonry only.
fn purpose_score(t: BuildingTechnique, purpose: StructurePurpose) -> i32 {
    use BuildingTechnique::*;
    use StructurePurpose::*;
    match (purpose, t) {
        // Defensive wants stone-class. Hydraulic Masonry also defensive-good
        // (the late-Bronze ladder uses it as a curtain-wall variant).
        (Defensive, AshlarDressing | HydraulicMasonry) => 100,
        (Defensive, CutStoneMasonry) => 90,
        (Defensive, DryStoneWalling) => 75,
        (Defensive, MudbrickMoulding | AdobeBrick | CobWalling) => 50,
        (Defensive, WattleAndDaub | TimberLonghouseFraming) => 30,
        (Defensive, _) => 10,
        // Hydraulic works are gated on the hydraulic technique itself.
        (Hydraulic, HydraulicMasonry) => 100,
        (Hydraulic, CutStoneMasonry | AshlarDressing) => 60,
        (Hydraulic, DryStoneWalling) => 30,
        (Hydraulic, _) => 0,
        // Storage / Civic prefer durable masonry but accept earthen.
        (Storage | Civic, AshlarDressing) => 95,
        (Storage | Civic, CutStoneMasonry) => 85,
        (Storage | Civic, MudbrickMoulding) => 70,
        (Storage | Civic, AdobeBrick | CobWalling) => 60,
        (Storage | Civic, DryStoneWalling) => 55,
        (Storage | Civic, WattleAndDaub) => 45,
        (Storage | Civic, TimberLonghouseFraming) => 40,
        (Storage | Civic, _) => 15,
        // Dwelling — earthen + timber dominate; mason-class works but heavy.
        (Dwelling, TimberLonghouseFraming) => 90,
        (Dwelling, WattleAndDaub) => 85,
        (Dwelling, MudbrickMoulding) => 80,
        (Dwelling, AdobeBrick) => 75,
        (Dwelling, CobWalling) => 70,
        (Dwelling, DryStoneWalling) => 70,
        (Dwelling, CutStoneMasonry) => 45,
        (Dwelling, PitHouse) => 35,
        (Dwelling, AshlarDressing) => 30,
        (Dwelling, HydraulicMasonry) => 10,
        (Dwelling, _) => 20,
        // NomadShelter only ever sees the two portable techniques — pick by
        // climate when a tie-break is needed.
        (NomadShelter, StakeAndHideTent) => 90,
        (NomadShelter, ReedMatting) => 75,
        (NomadShelter, _) => 0,
    }
}

/// Locality-fit score, in `[-30, +60]`. Rewards techniques whose primary
/// materials are abundant in the local site context; penalises mismatches
/// (e.g. Timber Framing on a treeless steppe).
fn locality_score(t: BuildingTechnique, loc: &LocalSiteContext) -> i32 {
    use BuildingTechnique::*;
    let forest = loc.forest_density as i32;
    let clay = loc.clay as i32;
    let wet = loc.wetland as i32;
    let river = loc.river_silt as i32;
    let has_stone = loc.stone_kind.is_some();
    // Two-band scaling: 0..=255 → roughly -8..=+30. We treat 128 (50%) as
    // "neutral" — at that level material is present but not abundant.
    let band = |p: i32| -> i32 { (p - 128) / 8 };
    match t {
        // Heavy timber framing is forest-dependent — at 50% density it pays
        // its keep, below that it falls off sharply.
        TimberLonghouseFraming => band(forest) * 2 + 5,
        WattleAndDaub => band(forest) / 2 + band(clay) / 2 + 10,
        WattleScreens => band(forest) / 3 + 5,
        CobWalling => band(clay) * 3 / 2 + 5,
        AdobeBrick => band(clay) + 5 + if loc.biome == Biome::Desert { 15 } else { 0 },
        // Standardised mudbrick production rewards genuinely clay-rich sites
        // (the recipe wants kiln-quality clay, not trace fines).
        MudbrickMoulding => band(clay) * 2 + 5,
        DryStoneWalling => {
            if has_stone {
                25 + band(forest) / -4
            } else {
                -10
            }
        }
        CutStoneMasonry => {
            if has_stone {
                30
            } else {
                -10
            }
        }
        AshlarDressing => {
            if has_stone {
                25
            } else {
                -15
            }
        }
        HydraulicMasonry => band(river) + if has_stone { 10 } else { -5 },
        ReedMatting => band(wet) + band(river) / 2 + 10,
        PitHouse => band(clay) / 2 + 5,
        StakeAndHideTent => band(forest) / 3 + 5,
        ThatchRoofing => 0, // filtered out before scoring
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::knowledge_catalog::{knowledge_def, KnowledgeDomain, KnowledgeKind};

    #[test]
    fn every_technique_gates_exist_in_catalog() {
        for t in BuildingTechnique::ALL {
            for &id in t.knowledge_gates() {
                let def = knowledge_def(id);
                assert_eq!(
                    def.domain(),
                    KnowledgeDomain::Construction,
                    "{:?} gate {} not Construction-domain",
                    t,
                    def.name()
                );
                assert_eq!(
                    def.kind(),
                    KnowledgeKind::PracticalTechnique,
                    "{:?} gate {} not PracticalTechnique kind",
                    t,
                    def.name()
                );
            }
        }
    }

    #[test]
    fn output_materials_cover_full_ladder() {
        use std::collections::HashSet;
        let mut seen: HashSet<WallMaterial> = HashSet::new();
        for t in BuildingTechnique::ALL {
            seen.insert(t.output_material());
        }
        // Every wall tier reachable through at least one technique.
        for m in WallMaterial::ALL {
            assert!(
                seen.contains(&m),
                "no BuildingTechnique outputs {:?}",
                m
            );
        }
    }

    #[test]
    fn roof_and_shelter_predicates() {
        assert!(BuildingTechnique::ThatchRoofing.is_roof_technique());
        assert!(!BuildingTechnique::WattleAndDaub.is_roof_technique());
        assert!(BuildingTechnique::StakeAndHideTent.is_portable_shelter());
        assert!(!BuildingTechnique::TimberLonghouseFraming.is_portable_shelter());
    }

    // ── Selector tests (Phase E.2 golden) ─────────────────────────────────

    /// Helper: a `FactionTechs` with the given `KnowledgeId`s set.
    fn techs_with(ids: &[KnowledgeId]) -> FactionTechs {
        let mut t = FactionTechs::default();
        for &id in ids {
            t.unlock(id);
        }
        t
    }

    /// Bronze-Age forest faction (every wall technique Learned). Locality has
    /// dense forest, no clay/stone. Should pick `TimberLonghouseFraming`.
    #[test]
    fn forest_faction_picks_timber_longhouse() {
        let techs = techs_with(&[
            WATTLE_AND_DAUB,
            TIMBER_LONGHOUSE_FRAMING,
            COB_WALLING,
            MUDBRICK_MOULDING,
            DRY_STONE_WALLING,
            CUT_STONE_MASONRY,
        ]);
        let loc = LocalSiteContext {
            biome: Biome::Temperate,
            stone_kind: None,
            forest_density: 220,
            clay: 40,
            wetland: 10,
            river_silt: 50,
        };
        let pick = select_building_technique(&techs, Some(&loc), StructurePurpose::Dwelling);
        assert_eq!(pick, Some(BuildingTechnique::TimberLonghouseFraming));
    }

    /// Bronze-Age clay-river faction. Same tech pool; locality has river-silt
    /// + clay, sparse forest. Should pick `MudbrickMoulding`.
    #[test]
    fn clay_river_faction_picks_mudbrick() {
        let techs = techs_with(&[
            WATTLE_AND_DAUB,
            TIMBER_LONGHOUSE_FRAMING,
            COB_WALLING,
            ADOBE_BRICK,
            MUDBRICK_MOULDING,
            DRY_STONE_WALLING,
            CUT_STONE_MASONRY,
        ]);
        let loc = LocalSiteContext {
            biome: Biome::Wetland,
            stone_kind: None,
            forest_density: 40,
            clay: 220,
            wetland: 100,
            river_silt: 220,
        };
        let pick = select_building_technique(&techs, Some(&loc), StructurePurpose::Dwelling);
        assert_eq!(pick, Some(BuildingTechnique::MudbrickMoulding));
    }

    /// Temperate-stone faction (granite/limestone outcrop in arid relief).
    /// Sparse clay, sparse forest. Should pick `DryStoneWalling` over the
    /// pre-Chalcolithic techniques.
    #[test]
    fn temperate_stone_faction_picks_dry_stone() {
        let techs = techs_with(&[
            WATTLE_AND_DAUB,
            TIMBER_LONGHOUSE_FRAMING,
            COB_WALLING,
            DRY_STONE_WALLING,
        ]);
        let loc = LocalSiteContext {
            biome: Biome::Temperate,
            stone_kind: Some(WallMaterialKind::Limestone),
            forest_density: 40,
            clay: 30,
            wetland: 10,
            river_silt: 30,
        };
        let pick = select_building_technique(&techs, Some(&loc), StructurePurpose::Dwelling);
        assert_eq!(pick, Some(BuildingTechnique::DryStoneWalling));
    }

    /// Locality `None` (no site survey). Should still return a viable
    /// dwelling technique — purpose_score alone gives a deterministic
    /// argmax — and `TimberLonghouseFraming` is the highest dwelling-purpose
    /// score among the Learned set.
    #[test]
    fn locality_none_falls_back_to_purpose_score() {
        let techs = techs_with(&[
            WATTLE_AND_DAUB,
            TIMBER_LONGHOUSE_FRAMING,
            COB_WALLING,
            DRY_STONE_WALLING,
        ]);
        let pick = select_building_technique(&techs, None, StructurePurpose::Dwelling);
        assert_eq!(pick, Some(BuildingTechnique::TimberLonghouseFraming));
    }

    /// No technique Learned ⇒ selector returns `None`. Caller falls back to
    /// the legacy `select_wall_material` ladder (where `EmergencyShelter`
    /// emits a bare bed).
    #[test]
    fn no_techniques_learned_returns_none() {
        let techs = FactionTechs::default();
        let loc = LocalSiteContext {
            biome: Biome::Temperate,
            stone_kind: None,
            forest_density: 128,
            clay: 128,
            wetland: 0,
            river_silt: 0,
        };
        let pick = select_building_technique(&techs, Some(&loc), StructurePurpose::Dwelling);
        assert_eq!(pick, None);
    }

    /// Nomad purpose only ever selects a portable shelter. Selector should
    /// reject Timber / Wattle even when their gates are Learned.
    #[test]
    fn nomad_purpose_picks_portable_shelter() {
        let techs = techs_with(&[
            STAKE_AND_HIDE_TENT,
            REED_MATTING,
            TIMBER_LONGHOUSE_FRAMING,
            CUT_STONE_MASONRY,
        ]);
        let loc = LocalSiteContext {
            biome: Biome::Grassland,
            stone_kind: None,
            forest_density: 64,
            clay: 20,
            wetland: 0,
            river_silt: 0,
        };
        let pick = select_building_technique(&techs, Some(&loc), StructurePurpose::NomadShelter);
        assert_eq!(pick, Some(BuildingTechnique::StakeAndHideTent));
    }

    /// Defensive purpose with a stone-bearing locality should pick the
    /// highest-tier masonry technique Learned (CutStoneMasonry beats
    /// DryStoneWalling).
    #[test]
    fn defensive_purpose_prefers_high_tier_masonry() {
        let techs = techs_with(&[
            WATTLE_AND_DAUB,
            DRY_STONE_WALLING,
            CUT_STONE_MASONRY,
        ]);
        let loc = LocalSiteContext {
            biome: Biome::Temperate,
            stone_kind: Some(WallMaterialKind::Granite),
            forest_density: 80,
            clay: 50,
            wetland: 0,
            river_silt: 0,
        };
        let pick = select_building_technique(&techs, Some(&loc), StructurePurpose::Defensive);
        assert_eq!(pick, Some(BuildingTechnique::CutStoneMasonry));
    }

    // Tile-kind alias for test readability — `stone_kind` is `TileKind` but
    // the tests only care about the discriminant (the relief gate decides
    // whether stone is *exposed* in the first place).
    type WallMaterialKind = crate::world::tile::TileKind;
}
