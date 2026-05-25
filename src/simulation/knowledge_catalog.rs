//! Catalog scaffold (Phase B of the knowledge-system overhaul).
//!
//! `TechDef` / `TECH_TREE` (in `technology.rs`) stay canonical for id, name,
//! era, prerequisites, triggers, and bonus. This module layers the **new**
//! ontology axes on top: knowledge domain, kind (skill / technique / belief /
//! lore), truth status, declarative effects, and belief-group membership.
//!
//! The intent is a single `KnowledgeDef` view that bundles both — accessed via
//! `knowledge_def(id)`. Phase E (building techniques) and Phase H (belief
//! content) read these axes; existing gate sites keep calling `tech_def(id)`
//! unchanged.

use super::technology::{
    self, current_era, tech_def, ActivityKind, Era, TechDef, TechId, ARD_PLOW, BRIDGE_BUILDING,
    BRONZE_CASTING, BRONZE_TOOLS, BRONZE_WEAPONS, COPPER_TOOLS, COPPER_WORKING,
    CITY_STATE_ORG, CROP_CULTIVATION, CUNEIFORM_WRITING, DAM_BUILDING, DUGOUT_CANOE,
    FIRED_POTTERY, FIRE_MAKING, FISHING, FLINT_KNAPPING, FOOD_SMOKING, GRANARY,
    HORSEBACK_RIDING, HORSE_TAMING, HUNTING_SPEAR, IRRIGATION, LOOM_WEAVING,
    LUNAR_CALENDAR, MONUMENTAL_BUILDING, OCHRE_PAINTING, OX_CART, PERM_SETTLEMENT,
    PORTABLE_DWELLINGS, POTTERS_WHEEL, PROFESSIONAL_ARMY, SACRED_RITUAL, SADDLE_QUERN,
    SCALE_ARMOR, SIEGE_ENGINEERING, ARMOR_PLATING, POWERED_TRACTION, TALLY_MARKS,
    TECH_COUNT, TIN_PROSPECTING, WAR_CHARIOT, WELL_DIGGING, ANIMAL_HUSBANDRY,
    BOW_AND_ARROW, BONE_TOOLS, MICROLITHIC_TOOLS, DOG_DOMESTICATION, LOG_RAFT,
    DRIED_MEAT, FERMENTATION, LONG_DIST_TRADE,
    ADOBE_BRICK, ASHLAR_DRESSING, COB_WALLING, CUT_STONE_MASONRY, DRY_STONE_WALLING,
    HYDRAULIC_MASONRY, MUDBRICK_MOULDING, PIT_HOUSE, REED_MATTING, STAKE_AND_HIDE_TENT,
    THATCH_ROOFING, TIMBER_LONGHOUSE_FRAMING, WATTLE_AND_DAUB, WATTLE_SCREENS,
    // Phase G foundations:
    ANIMAL_TRACKING, CLAY_TOKENS, CORDAGE, EDGE_GEOMETRY, EMBER_CARRYING, FIRE_USE, HAFTING,
    HIDE_WORKING, MEASURES_AND_UNITS, ORAL_TRADITION, PRACTICAL_GEOMETRY, RATION_ARITHMETIC,
    ROUTE_MEMORY, SEASONAL_MEMORY, TOOLSTONE_RECOGNITION, WATER_SOURCE_MEMORY,
    // Phase H beliefs:
    ECLIPSE_OMENS, GEOCENTRIC_COSMOS, MIASMA_THEORY, SKY_DOME, SPIRIT_ILLNESS, WEATHER_OMENS,
};

/// `KnowledgeId` is the long-term name for `TechId` once the catalog grows
/// past pure-technology entries (foundations, beliefs, building techniques).
/// Alias preserves every existing `TechId`-typed signature.
pub type KnowledgeId = TechId;

/// Top-level grouping of knowledge by subject area. Drives UI tabs and
/// targets per-domain effects (Construction → technique selection,
/// Cosmology → belief panel, Medicine → treatment HTN bias, etc.).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KnowledgeDomain {
    /// Hunting, foraging, farming, fishing, food preservation.
    Subsistence,
    /// Toolmaking, metallurgy, ceramics, weaving, mechanical craft.
    Craft,
    /// Walls, dwellings, civic structures, hydraulic works.
    Construction,
    /// Carts, boats, mounts, vehicles, beasts of burden.
    Transport,
    /// Faction organisation, ritual, writing, accounting, law.
    Institutional,
    /// Wound binding, herbal compounding, sanitation, anatomy.
    Medicine,
    /// Cosmological models, astronomy practice, calendars.
    Cosmology,
    /// Omens, agricultural lore, route memory — folk knowledge.
    Lore,
    /// Weapons, tactics, fortification, siege.
    Martial,
}

/// What *kind* of knowledge an entry is — distinct from its truth status.
/// A skill carries mastery; a belief carries confidence; lore carries
/// neither. Practical techniques behave like skills with respect to
/// mastery accrual.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KnowledgeKind {
    /// Doable activity with a skill curve (Wound Binding, Knapping, Pulse
    /// Diagnosis). Has `learned` + optional mastery 0..3.
    PracticalSkill,
    /// A way to build or make something (Wattle-and-Daub, Mudbrick
    /// Moulding). Has `learned` + optional mastery 0..3.
    PracticalTechnique,
    /// A proposition about the world (Geocentric, Heliocentric, Four
    /// Humors). Held with confidence in a belief group; **no mastery**.
    Belief,
    /// Body of factual recall (Seasonal Memory, Oral Tradition). Has
    /// `learned` only — no mastery, no acceptance.
    Lore,
}

/// Whether the knowledge is true, mostly-true-and-useful, harmful, or
/// genuinely contested. Practical skills are almost always `True` (they
/// either work or they don't); beliefs span the full range.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TruthStatus {
    /// Matches reality. Default for practical skills + techniques.
    True,
    /// False model, but its associated behaviour produces net-positive
    /// outcomes anyway (Miasma Theory → latrines, Four Humors → some
    /// herbal compounding).
    FalseUseful,
    /// False model, harmful in net effect (Spirit Illness → ritual
    /// instead of treatment).
    FalseHarmful,
    /// Empirically contested or genuinely uncertain.
    Contested,
}

/// Belief group identifier. Phase H content fills these; Phase B just
/// reserves the type. A `KnowledgeKind::Belief` entry has `Some(group)`;
/// all other kinds have `None`.
pub type BeliefGroupId = u8;

/// Reserved belief group ids. Used by Phase H content to anchor competing
/// model relationships (e.g. Geocentric vs Heliocentric both sit in
/// `cosmology`).
pub const BELIEF_GROUP_COSMOLOGY: BeliefGroupId = 1;
pub const BELIEF_GROUP_DISEASE_CAUSATION: BeliefGroupId = 2;
pub const BELIEF_GROUP_OMENS: BeliefGroupId = 3;

/// Declarative effect hint for the entry. Phase E reads
/// `UnlockBuildingTechnique`; Phase H reads `BiasPriority`. Today nothing
/// gates on these — they document what the entry **will** do once its
/// consumer wires up.
#[derive(Clone, Copy, Debug)]
pub enum KnowledgeEffect {
    /// Unlocks one `BuildingTechnique` variant (Phase E content).
    UnlockBuildingTechnique,
    /// Raises a settlement-level intent priority (e.g. latrine intent).
    BiasPriority,
    /// Raises the agent's effective tool tier when mastery > 0.
    ToolTierBoost,
    /// Reserved generic hook so future effects don't bump the enum size
    /// every time.
    Other,
}

/// Per-id metadata layered atop `TechDef`. Indexed by `KnowledgeId`.
#[derive(Clone, Copy, Debug)]
pub struct KnowledgeMeta {
    pub domain: KnowledgeDomain,
    pub kind: KnowledgeKind,
    pub truth: TruthStatus,
    pub belief_group: Option<BeliefGroupId>,
    pub contradicts: &'static [KnowledgeId],
    pub effects: &'static [KnowledgeEffect],
}

impl KnowledgeMeta {
    /// Default metadata: a true practical skill with no belief
    /// associations. The 50 existing TechDef entries default to this
    /// unless overridden in the table below.
    pub const DEFAULT: Self = Self {
        domain: KnowledgeDomain::Craft,
        kind: KnowledgeKind::PracticalSkill,
        truth: TruthStatus::True,
        belief_group: None,
        contradicts: &[],
        effects: &[],
    };
}

/// Bundled view: `TechDef` + `KnowledgeMeta`. Phase E consumers take a
/// `KnowledgeDef` borrow rather than separate `TechDef` + `KnowledgeMeta`
/// borrows.
#[derive(Clone, Copy)]
pub struct KnowledgeDef {
    pub tech: &'static TechDef,
    pub meta: &'static KnowledgeMeta,
}

impl KnowledgeDef {
    #[inline]
    pub fn id(&self) -> KnowledgeId {
        self.tech.id
    }
    #[inline]
    pub fn era(&self) -> Era {
        self.tech.era
    }
    #[inline]
    pub fn name(&self) -> &'static str {
        self.tech.name
    }
    #[inline]
    pub fn prerequisites(&self) -> &'static [KnowledgeId] {
        self.tech.prerequisites
    }
    #[inline]
    pub fn triggers(&self) -> &'static [super::technology::TechTrigger] {
        self.tech.triggers
    }
    #[inline]
    pub fn domain(&self) -> KnowledgeDomain {
        self.meta.domain
    }
    #[inline]
    pub fn kind(&self) -> KnowledgeKind {
        self.meta.kind
    }
    #[inline]
    pub fn truth(&self) -> TruthStatus {
        self.meta.truth
    }
    #[inline]
    pub fn belief_group(&self) -> Option<BeliefGroupId> {
        self.meta.belief_group
    }
}

/// O(1) lookup by id. Bundles `tech_def(id)` with the per-id metadata
/// table. Panics on out-of-range id — same contract as `tech_def`.
#[inline]
pub fn knowledge_def(id: KnowledgeId) -> KnowledgeDef {
    KnowledgeDef {
        tech: tech_def(id),
        meta: &KNOWLEDGE_META[id as usize],
    }
}

/// Read just the metadata axis.
#[inline]
pub fn knowledge_meta(id: KnowledgeId) -> &'static KnowledgeMeta {
    &KNOWLEDGE_META[id as usize]
}

/// Helper used by future-phase content tests: faction's current era from
/// chief Aware. Re-exported here so consumers don't pull from two modules.
#[inline]
pub fn current_era_of(techs: &crate::simulation::faction::FactionTechs) -> Era {
    current_era(techs)
}

/// Per-id metadata table, indexed by `TechId`. Entries that don't override
/// `DEFAULT` represent practical-skill / practical-technique / institutional
/// knowledge whose truth status is `True`. The Phase B port populates
/// domains; Phase G adds new foundational ids; Phase H promotes some
/// existing entries to `Belief` kind (e.g. `SACRED_RITUAL` may become
/// belief-flavoured in the cosmology content pass).
pub static KNOWLEDGE_META: [KnowledgeMeta; TECH_COUNT] = {
    let mut arr = [KnowledgeMeta::DEFAULT; TECH_COUNT];
    // Domain assignments below are the source of truth for the Phase B
    // catalog scaffold. Where an entry needs more than just a domain bump
    // (e.g. belief_group / effects), the relevant Phase H/E pass will
    // mutate this table.
    arr[FIRE_MAKING as usize].domain = KnowledgeDomain::Subsistence;
    arr[FLINT_KNAPPING as usize].domain = KnowledgeDomain::Craft;
    arr[HUNTING_SPEAR as usize].domain = KnowledgeDomain::Martial;
    arr[FOOD_SMOKING as usize].domain = KnowledgeDomain::Subsistence;
    arr[BONE_TOOLS as usize].domain = KnowledgeDomain::Craft;
    arr[OCHRE_PAINTING as usize].domain = KnowledgeDomain::Lore;
    arr[BOW_AND_ARROW as usize].domain = KnowledgeDomain::Martial;
    arr[FISHING as usize].domain = KnowledgeDomain::Subsistence;
    arr[MICROLITHIC_TOOLS as usize].domain = KnowledgeDomain::Craft;
    arr[DOG_DOMESTICATION as usize].domain = KnowledgeDomain::Subsistence;
    arr[LOG_RAFT as usize].domain = KnowledgeDomain::Transport;
    arr[DRIED_MEAT as usize].domain = KnowledgeDomain::Subsistence;
    arr[CROP_CULTIVATION as usize].domain = KnowledgeDomain::Subsistence;
    arr[ANIMAL_HUSBANDRY as usize].domain = KnowledgeDomain::Subsistence;
    arr[FIRED_POTTERY as usize].domain = KnowledgeDomain::Craft;
    arr[LOOM_WEAVING as usize].domain = KnowledgeDomain::Craft;
    arr[SADDLE_QUERN as usize].domain = KnowledgeDomain::Craft;
    arr[PERM_SETTLEMENT as usize].domain = KnowledgeDomain::Construction;
    arr[GRANARY as usize].domain = KnowledgeDomain::Construction;
    arr[IRRIGATION as usize].domain = KnowledgeDomain::Construction;
    arr[FERMENTATION as usize].domain = KnowledgeDomain::Subsistence;
    arr[DUGOUT_CANOE as usize].domain = KnowledgeDomain::Transport;
    arr[COPPER_WORKING as usize].domain = KnowledgeDomain::Craft;
    arr[COPPER_TOOLS as usize].domain = KnowledgeDomain::Craft;
    arr[POTTERS_WHEEL as usize].domain = KnowledgeDomain::Craft;
    arr[OX_CART as usize].domain = KnowledgeDomain::Transport;
    arr[ARD_PLOW as usize].domain = KnowledgeDomain::Subsistence;
    arr[LONG_DIST_TRADE as usize].domain = KnowledgeDomain::Institutional;
    arr[TALLY_MARKS as usize].domain = KnowledgeDomain::Institutional;
    arr[SACRED_RITUAL as usize].domain = KnowledgeDomain::Institutional;
    arr[TIN_PROSPECTING as usize].domain = KnowledgeDomain::Craft;
    arr[BRONZE_CASTING as usize].domain = KnowledgeDomain::Craft;
    arr[BRONZE_TOOLS as usize].domain = KnowledgeDomain::Craft;
    arr[BRONZE_WEAPONS as usize].domain = KnowledgeDomain::Martial;
    arr[SCALE_ARMOR as usize].domain = KnowledgeDomain::Martial;
    arr[HORSE_TAMING as usize].domain = KnowledgeDomain::Subsistence;
    arr[HORSEBACK_RIDING as usize].domain = KnowledgeDomain::Transport;
    arr[WAR_CHARIOT as usize].domain = KnowledgeDomain::Martial;
    arr[CUNEIFORM_WRITING as usize].domain = KnowledgeDomain::Institutional;
    arr[CITY_STATE_ORG as usize].domain = KnowledgeDomain::Institutional;
    arr[PROFESSIONAL_ARMY as usize].domain = KnowledgeDomain::Martial;
    arr[MONUMENTAL_BUILDING as usize].domain = KnowledgeDomain::Construction;
    arr[LUNAR_CALENDAR as usize].domain = KnowledgeDomain::Cosmology;
    arr[PORTABLE_DWELLINGS as usize].domain = KnowledgeDomain::Construction;
    arr[BRIDGE_BUILDING as usize].domain = KnowledgeDomain::Construction;
    arr[WELL_DIGGING as usize].domain = KnowledgeDomain::Construction;
    arr[DAM_BUILDING as usize].domain = KnowledgeDomain::Construction;
    arr[SIEGE_ENGINEERING as usize].domain = KnowledgeDomain::Martial;
    arr[ARMOR_PLATING as usize].domain = KnowledgeDomain::Martial;
    arr[POWERED_TRACTION as usize].domain = KnowledgeDomain::Transport;
    // Phase E — building techniques. Construction-domain
    // `PracticalTechnique`s; mastery accrues from repeated practice and
    // multiplies build progress (Phase C wiring).
    let building_techniques = [
        STAKE_AND_HIDE_TENT,
        REED_MATTING,
        WATTLE_SCREENS,
        PIT_HOUSE,
        WATTLE_AND_DAUB,
        TIMBER_LONGHOUSE_FRAMING,
        THATCH_ROOFING,
        COB_WALLING,
        ADOBE_BRICK,
        MUDBRICK_MOULDING,
        DRY_STONE_WALLING,
        CUT_STONE_MASONRY,
        ASHLAR_DRESSING,
        HYDRAULIC_MASONRY,
    ];
    let mut i = 0;
    while i < building_techniques.len() {
        let id = building_techniques[i];
        arr[id as usize].domain = KnowledgeDomain::Construction;
        arr[id as usize].kind = KnowledgeKind::PracticalTechnique;
        i += 1;
    }
    // Phase G — foundational knowledge. `Lore` for memory / recall (what
    // the band knows by heart); `PracticalSkill` for doing things (fire,
    // cordage, hafting, hide working). Domain bucketed by topic.
    let foundations_subsistence: [(KnowledgeId, KnowledgeKind); 5] = [
        (FIRE_USE, KnowledgeKind::PracticalSkill),
        (EMBER_CARRYING, KnowledgeKind::PracticalSkill),
        (HIDE_WORKING, KnowledgeKind::PracticalSkill),
        (ANIMAL_TRACKING, KnowledgeKind::Lore),
        (WATER_SOURCE_MEMORY, KnowledgeKind::Lore),
    ];
    let foundations_craft: [(KnowledgeId, KnowledgeKind); 4] = [
        (TOOLSTONE_RECOGNITION, KnowledgeKind::Lore),
        (EDGE_GEOMETRY, KnowledgeKind::Lore),
        (CORDAGE, KnowledgeKind::PracticalSkill),
        (HAFTING, KnowledgeKind::PracticalSkill),
    ];
    let foundations_lore: [(KnowledgeId, KnowledgeKind); 3] = [
        (SEASONAL_MEMORY, KnowledgeKind::Lore),
        (ORAL_TRADITION, KnowledgeKind::Lore),
        (ROUTE_MEMORY, KnowledgeKind::Lore),
    ];
    let foundations_inst: [(KnowledgeId, KnowledgeKind); 4] = [
        (CLAY_TOKENS, KnowledgeKind::PracticalSkill),
        (MEASURES_AND_UNITS, KnowledgeKind::Lore),
        (RATION_ARITHMETIC, KnowledgeKind::Lore),
        (PRACTICAL_GEOMETRY, KnowledgeKind::Lore),
    ];
    let mut j = 0;
    while j < foundations_subsistence.len() {
        let (id, kind) = foundations_subsistence[j];
        arr[id as usize].domain = KnowledgeDomain::Subsistence;
        arr[id as usize].kind = kind;
        j += 1;
    }
    j = 0;
    while j < foundations_craft.len() {
        let (id, kind) = foundations_craft[j];
        arr[id as usize].domain = KnowledgeDomain::Craft;
        arr[id as usize].kind = kind;
        j += 1;
    }
    j = 0;
    while j < foundations_lore.len() {
        let (id, kind) = foundations_lore[j];
        arr[id as usize].domain = KnowledgeDomain::Lore;
        arr[id as usize].kind = kind;
        j += 1;
    }
    j = 0;
    while j < foundations_inst.len() {
        let (id, kind) = foundations_inst[j];
        arr[id as usize].domain = KnowledgeDomain::Institutional;
        arr[id as usize].kind = kind;
        j += 1;
    }
    // Phase H — beliefs. Always `KnowledgeKind::Belief`; truth status and
    // belief_group are per-entry (cosmology / disease / omens). Heliocentric
    // / Contagion / Empirical Forecasting reserved for the post-Bronze
    // content pass — they sit in the same group as their FalseUseful
    // counterparts when added.
    let beliefs: [(KnowledgeId, KnowledgeDomain, TruthStatus, BeliefGroupId); 6] = [
        (SKY_DOME, KnowledgeDomain::Cosmology, TruthStatus::FalseUseful, BELIEF_GROUP_COSMOLOGY),
        (GEOCENTRIC_COSMOS, KnowledgeDomain::Cosmology, TruthStatus::FalseUseful, BELIEF_GROUP_COSMOLOGY),
        (SPIRIT_ILLNESS, KnowledgeDomain::Medicine, TruthStatus::FalseHarmful, BELIEF_GROUP_DISEASE_CAUSATION),
        (MIASMA_THEORY, KnowledgeDomain::Medicine, TruthStatus::FalseUseful, BELIEF_GROUP_DISEASE_CAUSATION),
        (ECLIPSE_OMENS, KnowledgeDomain::Lore, TruthStatus::FalseHarmful, BELIEF_GROUP_OMENS),
        (WEATHER_OMENS, KnowledgeDomain::Lore, TruthStatus::FalseUseful, BELIEF_GROUP_OMENS),
    ];
    j = 0;
    while j < beliefs.len() {
        let (id, domain, truth, group) = beliefs[j];
        arr[id as usize].domain = domain;
        arr[id as usize].kind = KnowledgeKind::Belief;
        arr[id as usize].truth = truth;
        arr[id as usize].belief_group = Some(group);
        j += 1;
    }
    // Cross-group `contradicts` links (only one belief can be accepted per
    // group; this slot is informational for UI / belief-swap heuristics).
    arr
};

// Silence unused-import lints when downstream phases haven't wired up
// the activity-kind helpers yet.
#[allow(dead_code)]
const _ACTIVITY_KIND_REF: ActivityKind = ActivityKind::Foraging;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tech_id_has_metadata() {
        for id in 0..TECH_COUNT as KnowledgeId {
            let def = knowledge_def(id);
            assert_eq!(def.id(), id);
            // Phase B contract: only `KnowledgeKind::Belief` entries carry a
            // `belief_group`; every other kind has `None`. Phase H added the
            // first beliefs (cosmology / disease / omens groups).
            match def.kind() {
                KnowledgeKind::Belief => {
                    assert!(
                        def.belief_group().is_some(),
                        "{} is Belief but has no belief_group",
                        def.name()
                    );
                }
                _ => {
                    assert!(
                        def.belief_group().is_none(),
                        "{} is {:?} but has a belief_group",
                        def.name(),
                        def.kind()
                    );
                }
            }
        }
    }

    #[test]
    fn catalog_is_acyclic_and_prereqs_lower_id() {
        for id in 0..TECH_COUNT as KnowledgeId {
            let def = knowledge_def(id);
            for &pre in def.prerequisites() {
                assert!(
                    pre < id,
                    "{} has prereq {} that is not strictly lower id",
                    def.name(),
                    pre
                );
            }
        }
    }

    #[test]
    fn era_and_complexity_round_trip_through_proxy() {
        // Phase B golden — `knowledge_def(id).era()` must match
        // `tech_def(id).era` for every existing id.
        for id in 0..TECH_COUNT as KnowledgeId {
            let kd = knowledge_def(id);
            let td = technology::tech_def(id);
            assert_eq!(kd.era(), td.era);
            assert_eq!(kd.name(), td.name);
        }
    }

    /// Phase G golden — every era-N starting founder has all era ≤ N
    /// foundational knowledge Learned. The plan's headline contract.
    #[test]
    fn phase_g_foundations_seed_for_every_role_at_target_era() {
        use crate::simulation::knowledge::{FounderRole, PersonKnowledge};
        use crate::simulation::technology::{
            ANIMAL_TRACKING, CLAY_TOKENS, CORDAGE, EDGE_GEOMETRY, EMBER_CARRYING, FIRE_USE,
            HAFTING, HIDE_WORKING, MEASURES_AND_UNITS, ORAL_TRADITION, PRACTICAL_GEOMETRY,
            RATION_ARITHMETIC, ROUTE_MEMORY, SEASONAL_MEMORY, TOOLSTONE_RECOGNITION,
            WATER_SOURCE_MEMORY,
        };
        // Paleolithic foundations Learned at every era for every role.
        let paleo: &[KnowledgeId] = &[
            FIRE_USE,
            EMBER_CARRYING,
            TOOLSTONE_RECOGNITION,
            EDGE_GEOMETRY,
            CORDAGE,
            HAFTING,
            HIDE_WORKING,
            ANIMAL_TRACKING,
            SEASONAL_MEMORY,
            ORAL_TRADITION,
            ROUTE_MEMORY,
            WATER_SOURCE_MEMORY,
        ];
        // Neolithic foundations Learned at Neolithic+ for every role.
        let neo: &[KnowledgeId] = &[
            CLAY_TOKENS,
            MEASURES_AND_UNITS,
            RATION_ARITHMETIC,
            PRACTICAL_GEOMETRY,
        ];
        let roles = [
            FounderRole::Common,
            FounderRole::Specialist,
            FounderRole::Chief,
            FounderRole::Scribe,
        ];
        for &role in &roles {
            for &era in &[Era::Paleolithic, Era::Mesolithic, Era::Neolithic, Era::Chalcolithic, Era::BronzeAge] {
                let k = PersonKnowledge::seeded_realistic_through_era(era, role, 0);
                for &id in paleo {
                    assert!(
                        k.has_learned(id),
                        "{:?} era={:?} should Learn Paleolithic foundation {}",
                        role, era, knowledge_def(id).name(),
                    );
                }
                if (era as u8) >= Era::Neolithic as u8 {
                    for &id in neo {
                        assert!(
                            k.has_learned(id),
                            "{:?} era={:?} should Learn Neolithic foundation {}",
                            role, era, knowledge_def(id).name(),
                        );
                    }
                }
            }
        }
    }

    /// Phase H — every belief KnowledgeId is tagged `KnowledgeKind::Belief`,
    /// carries a `belief_group`, and has a non-True `TruthStatus`. Catches
    /// drift if a Phase H entry's metadata is missed.
    #[test]
    fn phase_h_beliefs_are_grouped_and_typed() {
        use crate::simulation::technology::{
            ECLIPSE_OMENS, GEOCENTRIC_COSMOS, MIASMA_THEORY, SKY_DOME, SPIRIT_ILLNESS,
            WEATHER_OMENS,
        };
        let beliefs = [
            (SKY_DOME, BELIEF_GROUP_COSMOLOGY),
            (GEOCENTRIC_COSMOS, BELIEF_GROUP_COSMOLOGY),
            (SPIRIT_ILLNESS, BELIEF_GROUP_DISEASE_CAUSATION),
            (MIASMA_THEORY, BELIEF_GROUP_DISEASE_CAUSATION),
            (ECLIPSE_OMENS, BELIEF_GROUP_OMENS),
            (WEATHER_OMENS, BELIEF_GROUP_OMENS),
        ];
        for (id, group) in beliefs {
            let def = knowledge_def(id);
            assert_eq!(def.kind(), KnowledgeKind::Belief, "{} not Belief kind", def.name());
            assert_eq!(def.belief_group(), Some(group), "{} wrong group", def.name());
            assert!(
                !matches!(def.truth(), TruthStatus::True),
                "{} is True — Phase H beliefs are FalseUseful or FalseHarmful only",
                def.name()
            );
        }
    }

    /// Phase H — `seeded_realistic_through_era` populates the per-group
    /// belief map. Pre-Neolithic: Sky Dome + Spirit Illness. Neolithic+:
    /// Geocentric + Miasma. Beliefs must NOT land in the `learned` bitset.
    #[test]
    fn phase_h_seeded_beliefs_pin_era_appropriate_models() {
        use crate::simulation::knowledge::{FounderRole, PersonKnowledge};
        use crate::simulation::technology::{
            GEOCENTRIC_COSMOS, MIASMA_THEORY, SKY_DOME, SPIRIT_ILLNESS,
        };
        // Paleo: Sky Dome + Spirit Illness.
        let k = PersonKnowledge::seeded_realistic_through_era(Era::Paleolithic, FounderRole::Common, 0);
        let cosmo = k.belief_in(BELIEF_GROUP_COSMOLOGY).unwrap();
        let disease = k.belief_in(BELIEF_GROUP_DISEASE_CAUSATION).unwrap();
        assert_eq!(cosmo.accepted, SKY_DOME);
        assert_eq!(disease.accepted, SPIRIT_ILLNESS);
        assert!(cosmo.confidence > 100);
        // Beliefs must never land in `learned` — they're held, not used.
        assert!(!k.has_learned(SKY_DOME));
        assert!(!k.has_learned(SPIRIT_ILLNESS));

        // Neolithic: Geocentric + Miasma.
        let k = PersonKnowledge::seeded_realistic_through_era(Era::Neolithic, FounderRole::Common, 0);
        let cosmo = k.belief_in(BELIEF_GROUP_COSMOLOGY).unwrap();
        let disease = k.belief_in(BELIEF_GROUP_DISEASE_CAUSATION).unwrap();
        assert_eq!(cosmo.accepted, GEOCENTRIC_COSMOS);
        assert_eq!(disease.accepted, MIASMA_THEORY);
        assert!(!k.has_learned(GEOCENTRIC_COSMOS));
        assert!(!k.has_learned(MIASMA_THEORY));
    }

    #[test]
    fn construction_techs_are_in_construction_domain() {
        for id in [
            PERM_SETTLEMENT,
            GRANARY,
            IRRIGATION,
            MONUMENTAL_BUILDING,
            BRIDGE_BUILDING,
            WELL_DIGGING,
            DAM_BUILDING,
        ] {
            assert_eq!(
                knowledge_def(id).domain(),
                KnowledgeDomain::Construction,
                "{} should be Construction domain",
                knowledge_def(id).name()
            );
        }
    }
}
