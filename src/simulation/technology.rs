use bevy::prelude::*;

pub type TechId = u16;
pub const TECH_COUNT: usize = 86;
pub const ACTIVITY_COUNT: usize = 14;

// ── Tech ID constants ─────────────────────────────────────────────────────────

// Paleolithic
pub const FIRE_MAKING: TechId = 0;
pub const FLINT_KNAPPING: TechId = 1;
pub const HUNTING_SPEAR: TechId = 2;
pub const FOOD_SMOKING: TechId = 3;
pub const BONE_TOOLS: TechId = 4;
pub const OCHRE_PAINTING: TechId = 5;
// Mesolithic
pub const BOW_AND_ARROW: TechId = 6;
pub const FISHING: TechId = 7;
pub const MICROLITHIC_TOOLS: TechId = 8;
pub const DOG_DOMESTICATION: TechId = 9;
pub const LOG_RAFT: TechId = 10;
pub const DRIED_MEAT: TechId = 11;
// Neolithic
pub const CROP_CULTIVATION: TechId = 12;
pub const ANIMAL_HUSBANDRY: TechId = 13;
pub const FIRED_POTTERY: TechId = 14;
pub const LOOM_WEAVING: TechId = 15;
pub const SADDLE_QUERN: TechId = 16;
pub const PERM_SETTLEMENT: TechId = 17;
pub const GRANARY: TechId = 18;
pub const IRRIGATION: TechId = 19;
pub const FERMENTATION: TechId = 20;
pub const DUGOUT_CANOE: TechId = 21;
// Chalcolithic
pub const COPPER_WORKING: TechId = 22;
pub const COPPER_TOOLS: TechId = 23;
pub const POTTERS_WHEEL: TechId = 24;
pub const OX_CART: TechId = 25;
pub const ARD_PLOW: TechId = 26;
pub const LONG_DIST_TRADE: TechId = 27;
pub const TALLY_MARKS: TechId = 28;
pub const SACRED_RITUAL: TechId = 29;
// Bronze Age
pub const TIN_PROSPECTING: TechId = 30;
pub const BRONZE_CASTING: TechId = 31;
pub const BRONZE_TOOLS: TechId = 32;
pub const BRONZE_WEAPONS: TechId = 33;
pub const SCALE_ARMOR: TechId = 34;
pub const HORSE_TAMING: TechId = 35;
pub const HORSEBACK_RIDING: TechId = 36;
pub const WAR_CHARIOT: TechId = 37;
pub const CUNEIFORM_WRITING: TechId = 38;
pub const CITY_STATE_ORG: TechId = 39;
pub const PROFESSIONAL_ARMY: TechId = 40;
pub const MONUMENTAL_BUILDING: TechId = 41;
pub const LUNAR_CALENDAR: TechId = 42;
// Nomadic-mode addition. Sits in Neolithic so Neolithic+ factions
// (settled or nomadic) get it Aware+Learned via `seeded_through_era`.
pub const PORTABLE_DWELLINGS: TechId = 43;
// Chalcolithic public-works tech. Gates `BuildSiteKind::Bridge` (timber
// span over a river tile) and AI bridge-intent generation.
pub const BRIDGE_BUILDING: TechId = 44;
// Neolithic public-water structure. Gates `BuildSiteKind::Well` and
// the organic-settlement WaterAccess pressure.
pub const WELL_DIGGING: TechId = 45;
// Bronze-Age hydraulic engineering. Gates `BuildSiteKind::Dam` (the
// recipe `tech_gate`) and the AI `dam_intent_emitter_system`. Distinct
// from `BRIDGE_BUILDING` — impounding a watercourse is later, larger-
// scale civil engineering than spanning one.
pub const DAM_BUILDING: TechId = 46;
// ── Siege & war-vehicle techs (see plans/vehicle-system-tanks.md) ──────────
// All three sit in the Bronze-Age cap. `SIEGE_ENGINEERING` gates siege
// templates + the `turret` vehicle part; `ARMOR_PLATING` gates the
// `armor_plate` / `track` parts + the armored wagon; `POWERED_TRACTION`
// gates the abstract `engine` part + the `tank` template.
pub const SIEGE_ENGINEERING: TechId = 47;
pub const ARMOR_PLATING: TechId = 48;
pub const POWERED_TRACTION: TechId = 49;

// ── Building techniques (Phase E of the knowledge-system overhaul) ─────────
// Construction-domain `KnowledgeKind::PracticalTechnique` entries. Each
// technique gates one `BuildingTechnique` variant in `building_technique.rs`
// and maps to a `WallMaterial` for the render/combat surface. Prereqs only
// reference existing techs or earlier-listed new techniques so the
// "prereqs are strictly lower id" catalog invariant holds.
pub const STAKE_AND_HIDE_TENT: TechId = 50;
pub const REED_MATTING: TechId = 51;
pub const WATTLE_SCREENS: TechId = 52;
pub const PIT_HOUSE: TechId = 53;
pub const WATTLE_AND_DAUB: TechId = 54;
pub const TIMBER_LONGHOUSE_FRAMING: TechId = 55;
pub const THATCH_ROOFING: TechId = 56;
pub const COB_WALLING: TechId = 57;
pub const ADOBE_BRICK: TechId = 58;
pub const MUDBRICK_MOULDING: TechId = 59;
pub const DRY_STONE_WALLING: TechId = 60;
pub const CUT_STONE_MASONRY: TechId = 61;
pub const ASHLAR_DRESSING: TechId = 62;
pub const HYDRAULIC_MASONRY: TechId = 63;

// ── Foundational knowledge (Phase G of the knowledge-system overhaul) ──────
// Universal `AdoptionScale::Personal` knowledge — every founder is born
// knowing the era-≤-target foundations regardless of role. `tech_scale` arms
// classify these as Personal so `seeded_realistic_through_era` auto-Learns
// them on every founder. Catalog `KnowledgeKind` is `Lore` for memory /
// recall entries and `PracticalSkill` for the doing entries. No retroactive
// prereq additions to existing techs (per the Phase G design rule).
pub const FIRE_USE: TechId = 64;
pub const EMBER_CARRYING: TechId = 65;
pub const TOOLSTONE_RECOGNITION: TechId = 66;
pub const EDGE_GEOMETRY: TechId = 67;
pub const CORDAGE: TechId = 68;
pub const HAFTING: TechId = 69;
pub const HIDE_WORKING: TechId = 70;
pub const ANIMAL_TRACKING: TechId = 71;
pub const SEASONAL_MEMORY: TechId = 72;
pub const ORAL_TRADITION: TechId = 73;
pub const ROUTE_MEMORY: TechId = 74;
pub const WATER_SOURCE_MEMORY: TechId = 75;
pub const CLAY_TOKENS: TechId = 76;
pub const MEASURES_AND_UNITS: TechId = 77;
pub const RATION_ARITHMETIC: TechId = 78;
pub const PRACTICAL_GEOMETRY: TechId = 79;

// ── Beliefs (Phase H of the knowledge-system overhaul) ─────────────────────
// `KnowledgeKind::Belief` entries — held with confidence in a belief group,
// no mastery. Cosmology / disease_causation / omens groups defined in
// `knowledge_catalog.rs`. Truth status varies (FalseUseful drives positive
// behaviour despite being wrong; FalseHarmful biases the agent away from
// what works). Heliocentric / Contagion / Empirical Forecasting reserved
// for the post-Bronze content pass.
pub const SKY_DOME: TechId = 80;
pub const GEOCENTRIC_COSMOS: TechId = 81;
pub const SPIRIT_ILLNESS: TechId = 82;
pub const MIASMA_THEORY: TechId = 83;
pub const ECLIPSE_OMENS: TechId = 84;
pub const WEATHER_OMENS: TechId = 85;

// ── Types ─────────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    Paleolithic = 0,
    Mesolithic = 1,
    Neolithic = 2,
    Chalcolithic = 3,
    BronzeAge = 4,
}

impl Era {
    pub fn name(self) -> &'static str {
        match self {
            Era::Paleolithic => "Paleolithic",
            Era::Mesolithic => "Mesolithic",
            Era::Neolithic => "Neolithic",
            Era::Chalcolithic => "Chalcolithic",
            Era::BronzeAge => "Bronze Age",
        }
    }
}

/// Which activity category feeds into discovery probability.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    Foraging = 0,
    Farming = 1,
    WoodGathering = 2,
    StoneMining = 3,
    CoalMining = 4,
    IronMining = 5,
    Combat = 6,
    Socializing = 7,
    Trading = 8,
    CopperMining = 9,
    TinMining = 10,
    GoldMining = 11,
    SilverMining = 12,
    Fishing = 13,
}

/// A single activity/probability pair that drives tech discovery.
#[derive(Clone, Copy, Debug)]
pub struct TechTrigger {
    pub activity: ActivityKind,
    /// Chance added per unit of this activity in the season.
    /// e.g. 0.002 → 100 events adds 20% chance.
    pub per_unit_chance: f32,
}

/// Additive production/combat bonuses granted once a tech is unlocked.
#[derive(Clone, Copy, Debug)]
pub struct TechBonus {
    /// Added to base food yield multiplier (0.3 = +30%).
    pub food_yield_bonus: f32,
    pub wood_yield_bonus: f32,
    pub stone_yield_bonus: f32,
    /// Added to effective faction food storage capacity.
    pub food_storage_bonus: f32,
    /// Flat damage added to base ATTACK_DAMAGE.
    pub combat_damage_bonus: u8,
}

impl TechBonus {
    pub const ZERO: TechBonus = TechBonus {
        food_yield_bonus: 0.0,
        wood_yield_bonus: 0.0,
        stone_yield_bonus: 0.0,
        food_storage_bonus: 0.0,
        combat_damage_bonus: 0,
    };
}

/// Static descriptor for one node in the tech tree.
#[derive(Clone, Copy)]
pub struct TechDef {
    pub id: TechId,
    pub name: &'static str,
    pub description: &'static str,
    pub era: Era,
    /// All of these must be unlocked before discovery is possible.
    pub prerequisites: &'static [TechId],
    /// Activities that accumulate discovery probability each season.
    pub triggers: &'static [TechTrigger],
    pub bonus: TechBonus,
}

/// O(1) lookup by TechId (IDs are dense 0..TECH_COUNT).
#[inline]
pub fn tech_def(id: TechId) -> &'static TechDef {
    &TECH_TREE[id as usize]
}

/// Capacity-cost of a tech in a person's Learned set. Era-based with a few
/// high-cognition bumps. Used by `PersonKnowledge::try_learn` against the
/// intelligence-derived `knowledge_capacity`.
#[inline]
pub fn complexity(id: TechId) -> u8 {
    // Specific high-cognition bumps.
    match id {
        CUNEIFORM_WRITING | LUNAR_CALENDAR | CITY_STATE_ORG => return 6,
        _ => {}
    }
    match TECH_TREE[id as usize].era {
        Era::Paleolithic => 1,
        Era::Mesolithic => 2,
        Era::Neolithic => 3,
        Era::Chalcolithic => 4,
        Era::BronzeAge => 5,
    }
}

/// Highest era for which the faction has unlocked at least one tech.
/// Defaults to Paleolithic when nothing has been discovered yet.
pub fn current_era(techs: &crate::simulation::faction::FactionTechs) -> Era {
    let mut highest = Era::Paleolithic;
    for id in 0..TECH_COUNT as TechId {
        if techs.has(id) {
            let era = TECH_TREE[id as usize].era;
            if era as u8 > highest as u8 {
                highest = era;
            }
        }
    }
    highest
}

// ── Static tech tree ──────────────────────────────────────────────────────────

pub static TECH_TREE: [TechDef; TECH_COUNT] = [
    // ── Paleolithic ───────────────────────────────────────────────────────────
    TechDef {
        id: FIRE_MAKING,
        era: Era::Paleolithic,
        name: "Fire Making",
        description: "Control of fire enables cooking, warmth, light, and protection.",
        prerequisites: &[],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.001,
            },
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            food_storage_bonus: 0.20,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: FLINT_KNAPPING,
        era: Era::Paleolithic,
        name: "Flint Knapping",
        description: "Shaping flint and obsidian into sharp tools and projectile points.",
        prerequisites: &[FIRE_MAKING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            stone_yield_bonus: 0.10,
            combat_damage_bonus: 1,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: HUNTING_SPEAR,
        era: Era::Paleolithic,
        name: "Hunting Spear",
        description: "Hafted stone-tipped spears allow pursuit of large game.",
        prerequisites: &[FLINT_KNAPPING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.15,
            combat_damage_bonus: 2,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: FOOD_SMOKING,
        era: Era::Paleolithic,
        name: "Food Smoking",
        description: "Smoking meat and fish over fire greatly extends their preservation.",
        prerequisites: &[FIRE_MAKING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.20,
            food_storage_bonus: 0.30,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: BONE_TOOLS,
        era: Era::Paleolithic,
        name: "Bone Tools",
        description: "Animal bones fashioned into needles, awls, and hooks.",
        prerequisites: &[FLINT_KNAPPING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.05,
            wood_yield_bonus: 0.05,
            stone_yield_bonus: 0.05,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: OCHRE_PAINTING,
        era: Era::Paleolithic,
        name: "Ochre Painting",
        description: "Red ochre pigment used in ritual and symbolic expression.",
        prerequisites: &[FIRE_MAKING],
        triggers: &[TechTrigger {
            activity: ActivityKind::Socializing,
            per_unit_chance: 0.004,
        }],
        bonus: TechBonus::ZERO,
    },
    // ── Mesolithic ────────────────────────────────────────────────────────────
    TechDef {
        id: BOW_AND_ARROW,
        era: Era::Mesolithic,
        name: "Bow and Arrow",
        description: "Flexible bows and light arrows extend hunting range dramatically.",
        prerequisites: &[HUNTING_SPEAR],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::WoodGathering,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.20,
            combat_damage_bonus: 3,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: FISHING,
        era: Era::Mesolithic,
        name: "Fishing",
        description: "Woven baskets, hooks, and weirs to harvest fish from rivers.",
        prerequisites: &[BONE_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.25,
            food_storage_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: MICROLITHIC_TOOLS,
        era: Era::Mesolithic,
        name: "Microlithic Tools",
        description: "Tiny precisely-knapped blades set in handles for composite tools.",
        prerequisites: &[FLINT_KNAPPING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.05,
            wood_yield_bonus: 0.10,
            stone_yield_bonus: 0.10,
            combat_damage_bonus: 1,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: DOG_DOMESTICATION,
        era: Era::Mesolithic,
        name: "Dog Domestication",
        description: "Wolves tamed into hunting companions and camp guards.",
        prerequisites: &[FIRE_MAKING, HUNTING_SPEAR],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.002,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.003,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.15,
            combat_damage_bonus: 1,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: LOG_RAFT,
        era: Era::Mesolithic,
        name: "Log Raft",
        description: "Bound logs enable water travel and access to new territories.",
        prerequisites: &[BONE_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::WoodGathering,
                per_unit_chance: 0.005,
            },
            // Rafts are discovered by people working the water — fishing
            // drives the urge to range farther downstream.
            TechTrigger {
                activity: ActivityKind::Fishing,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: DRIED_MEAT,
        era: Era::Mesolithic,
        name: "Dried Meat",
        description: "Sun-drying and salting meat creates portable, long-lasting food.",
        prerequisites: &[FOOD_SMOKING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.20,
            food_storage_bonus: 0.20,
            ..TechBonus::ZERO
        },
    },
    // ── Neolithic ─────────────────────────────────────────────────────────────
    TechDef {
        id: CROP_CULTIVATION,
        era: Era::Neolithic,
        name: "Crop Cultivation",
        description: "Deliberate planting and tending of grain crops enables reliable harvests.",
        prerequisites: &[FOOD_SMOKING, BONE_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.35,
            food_storage_bonus: 0.15,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: ANIMAL_HUSBANDRY,
        era: Era::Neolithic,
        name: "Animal Husbandry",
        description: "Selective breeding of sheep, goats, and cattle for sustained yields.",
        prerequisites: &[DOG_DOMESTICATION],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.25,
            food_storage_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: FIRED_POTTERY,
        era: Era::Neolithic,
        name: "Fired Pottery",
        description: "Clay vessels hardened by fire allow boiling, fermenting, and food storage.",
        prerequisites: &[FIRE_MAKING, CROP_CULTIVATION],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            food_storage_bonus: 0.40,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: LOOM_WEAVING,
        era: Era::Neolithic,
        name: "Loom Weaving",
        description: "Frame looms produce cloth from plant fibers and wool.",
        prerequisites: &[CROP_CULTIVATION],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: SADDLE_QUERN,
        era: Era::Neolithic,
        name: "Saddle Quern",
        description: "Stone grinding slabs process grain into flour, improving nutrition.",
        prerequisites: &[CROP_CULTIVATION, MICROLITHIC_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.20,
            food_storage_bonus: 0.15,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: PERM_SETTLEMENT,
        era: Era::Neolithic,
        name: "Permanent Settlement",
        description: "Fixed villages with durable structures anchor communities to fertile land.",
        prerequisites: &[CROP_CULTIVATION, FIRED_POTTERY],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.002,
            },
            TechTrigger {
                activity: ActivityKind::WoodGathering,
                per_unit_chance: 0.002,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            food_storage_bonus: 0.30,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: GRANARY,
        era: Era::Neolithic,
        name: "Granary",
        description: "Raised storage buildings keep grain dry and safe from pests.",
        prerequisites: &[PERM_SETTLEMENT, FIRED_POTTERY],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::WoodGathering,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_storage_bonus: 0.50,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: IRRIGATION,
        era: Era::Neolithic,
        name: "Irrigation",
        description: "Ditches and channels deliver water to fields independent of rainfall.",
        prerequisites: &[CROP_CULTIVATION, PERM_SETTLEMENT],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.40,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: FERMENTATION,
        era: Era::Neolithic,
        name: "Fermentation",
        description: "Controlled fermentation of grain produces beer and preserves surplus.",
        prerequisites: &[FIRED_POTTERY, CROP_CULTIVATION],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            food_storage_bonus: 0.25,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: DUGOUT_CANOE,
        era: Era::Neolithic,
        name: "Dugout Canoe",
        description: "Hollowed tree trunks provide reliable watercraft for fishing and trade.",
        prerequisites: &[LOG_RAFT, MICROLITHIC_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::WoodGathering,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    // ── Chalcolithic ──────────────────────────────────────────────────────────
    TechDef {
        id: COPPER_WORKING,
        era: Era::Chalcolithic,
        name: "Copper Working",
        description: "Native copper hammered and later smelted into tools and ornaments.",
        prerequisites: &[MICROLITHIC_TOOLS, PERM_SETTLEMENT],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::CopperMining,
                per_unit_chance: 0.008,
            },
        ],
        bonus: TechBonus {
            stone_yield_bonus: 0.10,
            combat_damage_bonus: 2,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: COPPER_TOOLS,
        era: Era::Chalcolithic,
        name: "Copper Tools",
        description: "Copper axes, chisels, and sickles outperform stone in durability.",
        prerequisites: &[COPPER_WORKING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            wood_yield_bonus: 0.10,
            stone_yield_bonus: 0.15,
            combat_damage_bonus: 1,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: POTTERS_WHEEL,
        era: Era::Chalcolithic,
        name: "Potter's Wheel",
        description: "Rotating platform allows rapid, uniform pottery production.",
        prerequisites: &[FIRED_POTTERY, COPPER_WORKING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.002,
            },
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.003,
            },
        ],
        bonus: TechBonus {
            food_storage_bonus: 0.20,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: OX_CART,
        era: Era::Chalcolithic,
        name: "Ox Cart",
        description: "Wheeled carts pulled by oxen transform bulk transport of goods.",
        prerequisites: &[ANIMAL_HUSBANDRY, COPPER_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.002,
            },
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.004,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.15,
            food_storage_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: ARD_PLOW,
        era: Era::Chalcolithic,
        name: "Ard Plow",
        description: "Ox-drawn ard breaks soil faster, enabling far larger cultivated areas.",
        prerequisites: &[ANIMAL_HUSBANDRY, COPPER_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.006,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.35,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: LONG_DIST_TRADE,
        era: Era::Chalcolithic,
        name: "Long Distance Trade",
        description: "Organised trade routes carry prestige goods across hundreds of miles.",
        prerequisites: &[OX_CART, DUGOUT_CANOE],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.007,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: TALLY_MARKS,
        era: Era::Chalcolithic,
        name: "Tally Marks",
        description: "Notched bones and clay tokens record quantities and debts.",
        prerequisites: &[LONG_DIST_TRADE],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.003,
            },
        ],
        bonus: TechBonus {
            food_storage_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: SACRED_RITUAL,
        era: Era::Chalcolithic,
        name: "Sacred Ritual",
        description: "Organised ceremony and burial rites bind communities spiritually.",
        prerequisites: &[OCHRE_PAINTING, FERMENTATION],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.006,
            },
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.05,
            food_storage_bonus: 0.15,
            ..TechBonus::ZERO
        },
    },
    // ── Bronze Age ────────────────────────────────────────────────────────────
    TechDef {
        id: TIN_PROSPECTING,
        era: Era::BronzeAge,
        name: "Tin Prospecting",
        description: "Systematic search for cassiterite (tin ore) deposits in stream beds.",
        prerequisites: &[COPPER_WORKING, MICROLITHIC_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::TinMining,
                per_unit_chance: 0.008,
            },
        ],
        bonus: TechBonus {
            stone_yield_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: BRONZE_CASTING,
        era: Era::BronzeAge,
        name: "Bronze Casting",
        description: "Mixing copper and tin alloy poured into clay moulds creates bronze.",
        prerequisites: &[TIN_PROSPECTING, COPPER_WORKING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::CoalMining,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::IronMining,
                per_unit_chance: 0.003,
            },
        ],
        bonus: TechBonus {
            stone_yield_bonus: 0.10,
            combat_damage_bonus: 2,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: BRONZE_TOOLS,
        era: Era::BronzeAge,
        name: "Bronze Tools",
        description: "Bronze axes, saws, and chisels revolutionise woodworking and farming.",
        prerequisites: &[BRONZE_CASTING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.15,
            wood_yield_bonus: 0.20,
            stone_yield_bonus: 0.20,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: BRONZE_WEAPONS,
        era: Era::BronzeAge,
        name: "Bronze Weapons",
        description: "Swords, spearheads, and axes of bronze give warriors decisive advantage.",
        prerequisites: &[BRONZE_CASTING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            combat_damage_bonus: 4,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: SCALE_ARMOR,
        era: Era::BronzeAge,
        name: "Scale Armor",
        description: "Overlapping bronze scales sewn to leather provide superior protection.",
        prerequisites: &[BRONZE_CASTING, LOOM_WEAVING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: HORSE_TAMING,
        era: Era::BronzeAge,
        name: "Horse Taming",
        description: "Wild horses broken and kept for transport and draft work.",
        prerequisites: &[ANIMAL_HUSBANDRY, ARD_PLOW],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.002,
            },
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.20,
            combat_damage_bonus: 1,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: HORSEBACK_RIDING,
        era: Era::BronzeAge,
        name: "Horseback Riding",
        description: "Mounted warriors and herders gain speed and reach unprecedented in warfare.",
        prerequisites: &[HORSE_TAMING],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.001,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            combat_damage_bonus: 2,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: WAR_CHARIOT,
        era: Era::BronzeAge,
        name: "War Chariot",
        description: "Horse-drawn chariots armed with archers dominate open battlefield.",
        prerequisites: &[HORSEBACK_RIDING, BRONZE_WEAPONS],
        triggers: &[TechTrigger {
            activity: ActivityKind::Combat,
            per_unit_chance: 0.006,
        }],
        bonus: TechBonus {
            combat_damage_bonus: 5,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: CUNEIFORM_WRITING,
        era: Era::BronzeAge,
        name: "Cuneiform Writing",
        description: "Wedge-shaped marks on clay tablets record trade and administration.",
        prerequisites: &[TALLY_MARKS, PERM_SETTLEMENT],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.003,
            },
        ],
        bonus: TechBonus {
            food_storage_bonus: 0.20,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: CITY_STATE_ORG,
        era: Era::BronzeAge,
        name: "City-State Organisation",
        description: "Stratified society with specialised labour, laws, and centralised authority.",
        prerequisites: &[CUNEIFORM_WRITING, GRANARY],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.005,
            },
            TechTrigger {
                activity: ActivityKind::Trading,
                per_unit_chance: 0.003,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            food_storage_bonus: 0.30,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: PROFESSIONAL_ARMY,
        era: Era::BronzeAge,
        name: "Professional Army",
        description: "Full-time soldiers trained in formation tactics and siege warfare.",
        prerequisites: &[CITY_STATE_ORG, BRONZE_WEAPONS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Combat,
                per_unit_chance: 0.007,
            },
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            combat_damage_bonus: 3,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: MONUMENTAL_BUILDING,
        era: Era::BronzeAge,
        name: "Monumental Building",
        description: "Pyramids, ziggurats, and palaces built with organised labour corvées.",
        prerequisites: &[CITY_STATE_ORG, BRONZE_TOOLS],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::WoodGathering,
                per_unit_chance: 0.003,
            },
            TechTrigger {
                activity: ActivityKind::StoneMining,
                per_unit_chance: 0.005,
            },
        ],
        bonus: TechBonus {
            stone_yield_bonus: 0.20,
            food_storage_bonus: 0.20,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: LUNAR_CALENDAR,
        era: Era::BronzeAge,
        name: "Lunar Calendar",
        description:
            "Systematic observation of moon phases creates a shared agricultural calendar.",
        prerequisites: &[CUNEIFORM_WRITING, SACRED_RITUAL],
        triggers: &[
            TechTrigger {
                activity: ActivityKind::Socializing,
                per_unit_chance: 0.004,
            },
            TechTrigger {
                activity: ActivityKind::Farming,
                per_unit_chance: 0.002,
            },
        ],
        bonus: TechBonus {
            food_yield_bonus: 0.10,
            food_storage_bonus: 0.10,
            ..TechBonus::ZERO
        },
    },
    TechDef {
        id: PORTABLE_DWELLINGS,
        era: Era::Neolithic,
        name: "Portable Dwellings",
        description: "Felt-and-lattice yurts and packable hide tents — bands can carry an \
             entire shelter on a few pack animals and re-pitch at the next camp.",
        prerequisites: &[LOOM_WEAVING],
        triggers: &[TechTrigger {
            activity: ActivityKind::Foraging,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: BRIDGE_BUILDING,
        era: Era::Chalcolithic,
        name: "Bridge Building",
        description: "Timber spans and causeways across river channels — settlements link \
             both banks and routes no longer have to detour around water.",
        prerequisites: &[PERM_SETTLEMENT, DUGOUT_CANOE, COPPER_TOOLS],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: WELL_DIGGING,
        era: Era::Neolithic,
        name: "Well Digging",
        description: "Lined shafts reach the water table — settlements gain a clean public \
             water source independent of rivers and springs.",
        prerequisites: &[FLINT_KNAPPING, PERM_SETTLEMENT],
        triggers: &[TechTrigger {
            activity: ActivityKind::StoneMining,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: DAM_BUILDING,
        era: Era::BronzeAge,
        name: "Dam Building",
        description: "Stone-and-timber barriers impound rivers for reservoirs, irrigation, \
             and dry-season water security — a civilisation reshapes its watershed.",
        prerequisites: &[BRIDGE_BUILDING, MONUMENTAL_BUILDING],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: SIEGE_ENGINEERING,
        era: Era::BronzeAge,
        name: "Siege Engineering",
        description: "Battering rams, siege towers, and torsion artillery — organised \
             labour turns timber and rope into engines that breach fortifications.",
        prerequisites: &[WAR_CHARIOT, MONUMENTAL_BUILDING],
        triggers: &[TechTrigger {
            activity: ActivityKind::Combat,
            per_unit_chance: 0.003,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ARMOR_PLATING,
        era: Era::BronzeAge,
        name: "Armor Plating",
        description: "Bronze-faced timber plating and continuous track shields a vehicle \
             body — war wagons that shrug off arrows and roll over broken ground.",
        prerequisites: &[SCALE_ARMOR, BRONZE_CASTING],
        triggers: &[TechTrigger {
            activity: ActivityKind::Combat,
            per_unit_chance: 0.003,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: POWERED_TRACTION,
        era: Era::BronzeAge,
        name: "Powered Traction",
        description: "Abstract powered draft — a self-propelled vehicle that needs no \
             draft team. The capstone of the war-vehicle line.",
        prerequisites: &[SIEGE_ENGINEERING, BRONZE_CASTING],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    // ── Building techniques (Phase E) ────────────────────────────────────
    TechDef {
        id: STAKE_AND_HIDE_TENT,
        era: Era::Paleolithic,
        name: "Stake-and-Hide Tent",
        description: "Hide stretched over a wooden pole frame — the original portable shelter.",
        prerequisites: &[HUNTING_SPEAR, FOOD_SMOKING],
        triggers: &[TechTrigger {
            activity: ActivityKind::WoodGathering,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: REED_MATTING,
        era: Era::Mesolithic,
        name: "Reed Matting",
        description: "Bundled reeds woven into wall screens, roofing panels, and floor mats.",
        prerequisites: &[BONE_TOOLS],
        triggers: &[TechTrigger {
            activity: ActivityKind::Foraging,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: WATTLE_SCREENS,
        era: Era::Mesolithic,
        name: "Wattle Screens",
        description: "Pliable hazel rods woven through upright stakes — wind-break panels and \
             light enclosures.",
        prerequisites: &[BONE_TOOLS],
        triggers: &[TechTrigger {
            activity: ActivityKind::WoodGathering,
            per_unit_chance: 0.002,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: PIT_HOUSE,
        era: Era::Mesolithic,
        name: "Pit House",
        description: "Semi-subterranean dwelling: shallow pit, sloped earth berm, light timber roof.",
        prerequisites: &[BONE_TOOLS],
        triggers: &[TechTrigger {
            activity: ActivityKind::WoodGathering,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: WATTLE_AND_DAUB,
        era: Era::Neolithic,
        name: "Wattle and Daub",
        description: "Woven wattle screens packed with clay and straw — load-bearing walls in \
             permanent dwellings.",
        prerequisites: &[WATTLE_SCREENS, PERM_SETTLEMENT],
        triggers: &[TechTrigger {
            activity: ActivityKind::WoodGathering,
            per_unit_chance: 0.002,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: TIMBER_LONGHOUSE_FRAMING,
        era: Era::Neolithic,
        name: "Timber Longhouse Framing",
        description: "Heavy oak posts, paired tie-beams, and gable rafters carry a long shared \
             roof — the Neolithic European longhouse.",
        prerequisites: &[PERM_SETTLEMENT],
        triggers: &[TechTrigger {
            activity: ActivityKind::WoodGathering,
            per_unit_chance: 0.003,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: THATCH_ROOFING,
        era: Era::Neolithic,
        name: "Thatch Roofing",
        description: "Bundled straw or reed tied to roof rafters — sheds rain, insulates against \
             cold, lasts decades when maintained.",
        prerequisites: &[CROP_CULTIVATION],
        triggers: &[TechTrigger {
            activity: ActivityKind::Farming,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: COB_WALLING,
        era: Era::Neolithic,
        name: "Cob Walling",
        description: "Monolithic earth walls — clay, sand, straw, and water mixed and laid wet in \
             courses, dried in place.",
        prerequisites: &[PERM_SETTLEMENT],
        triggers: &[TechTrigger {
            activity: ActivityKind::Farming,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ADOBE_BRICK,
        era: Era::Neolithic,
        name: "Adobe Brick",
        description: "Sun-dried clay-and-straw bricks moulded in wooden frames — modular, stackable \
             building blocks for arid-climate dwellings.",
        prerequisites: &[FIRED_POTTERY],
        triggers: &[TechTrigger {
            activity: ActivityKind::Farming,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: MUDBRICK_MOULDING,
        era: Era::Neolithic,
        name: "Mudbrick Moulding",
        description: "Standardised mudbrick production in wooden moulds — uniform courses, faster \
             walls, the basis for ancient urban dwellings.",
        prerequisites: &[ADOBE_BRICK],
        triggers: &[TechTrigger {
            activity: ActivityKind::Farming,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: DRY_STONE_WALLING,
        era: Era::Neolithic,
        name: "Dry-Stone Walling",
        description: "Coursed field-stone laid without mortar — the rural enclosure wall, durable \
             for generations.",
        prerequisites: &[PERM_SETTLEMENT],
        triggers: &[TechTrigger {
            activity: ActivityKind::StoneMining,
            per_unit_chance: 0.002,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: CUT_STONE_MASONRY,
        era: Era::Chalcolithic,
        name: "Cut Stone Masonry",
        description: "Quarried blocks dressed with copper tools, laid in lime-mortar courses — \
             the defensive curtain wall.",
        prerequisites: &[COPPER_TOOLS, DRY_STONE_WALLING],
        triggers: &[TechTrigger {
            activity: ActivityKind::StoneMining,
            per_unit_chance: 0.003,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ASHLAR_DRESSING,
        era: Era::BronzeAge,
        name: "Ashlar Dressing",
        description: "Precisely-squared and finely-dressed stone blocks — palace and monumental \
             construction without visible joints.",
        prerequisites: &[BRONZE_TOOLS, CUT_STONE_MASONRY],
        triggers: &[TechTrigger {
            activity: ActivityKind::StoneMining,
            per_unit_chance: 0.002,
        }],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: HYDRAULIC_MASONRY,
        era: Era::BronzeAge,
        name: "Hydraulic Masonry",
        description: "Mortar-and-rubble construction with lime-set joints that cure underwater — \
             watercourses, cisterns, port works.",
        prerequisites: &[MONUMENTAL_BUILDING, MUDBRICK_MOULDING],
        triggers: &[TechTrigger {
            activity: ActivityKind::StoneMining,
            per_unit_chance: 0.001,
        }],
        bonus: TechBonus::ZERO,
    },
    // ── Phase G foundations ─────────────────────────────────────────────
    // Every founder is born knowing the era-≤-target foundations. Triggers
    // are intentionally sparse: discovery rolls cost work, and these are
    // assumed common knowledge in any era they sit in.
    TechDef {
        id: FIRE_USE,
        era: Era::Paleolithic,
        name: "Fire Use",
        description: "Working understanding of what fire is, what it does, and how to feed it.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: EMBER_CARRYING,
        era: Era::Paleolithic,
        name: "Ember Carrying",
        description: "Banked coals carried over distance — fire travels with the band before fire-making.",
        prerequisites: &[FIRE_USE],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: TOOLSTONE_RECOGNITION,
        era: Era::Paleolithic,
        name: "Toolstone Recognition",
        description: "Eye for flint, chert, obsidian, and quartzite — which cobbles knap and which crumble.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: EDGE_GEOMETRY,
        era: Era::Paleolithic,
        name: "Edge Geometry",
        description: "Intuitive grasp of bevel, edge angle, and percussion — the geometry of a working blade.",
        prerequisites: &[TOOLSTONE_RECOGNITION],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: CORDAGE,
        era: Era::Paleolithic,
        name: "Cordage",
        description: "Twisting plant fibre and sinew into cord, twine, and rope.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: HAFTING,
        era: Era::Paleolithic,
        name: "Hafting",
        description: "Binding a stone head to a wooden shaft with cord and pitch.",
        prerequisites: &[EDGE_GEOMETRY, CORDAGE],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: HIDE_WORKING,
        era: Era::Paleolithic,
        name: "Hide Working",
        description: "Scraping, defleshing, soaking, and stretching hides into supple working leather.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ANIMAL_TRACKING,
        era: Era::Paleolithic,
        name: "Animal Tracking",
        description: "Reading prints, droppings, browse, and trail-spoor — the band's hunting eye.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: SEASONAL_MEMORY,
        era: Era::Paleolithic,
        name: "Seasonal Memory",
        description: "Lived knowledge of when each plant fruits, fish run, and herd moves through the year.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ORAL_TRADITION,
        era: Era::Paleolithic,
        name: "Oral Tradition",
        description: "Stories, songs, and genealogies kept alive by recitation — the band's history before writing.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ROUTE_MEMORY,
        era: Era::Paleolithic,
        name: "Route Memory",
        description: "Travel routes, landmarks, and trail-knowledge held in head — no map needed.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: WATER_SOURCE_MEMORY,
        era: Era::Paleolithic,
        name: "Water Source Memory",
        description: "Where every reliable spring, seep, and seasonal pool lies within the band's range.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: CLAY_TOKENS,
        era: Era::Neolithic,
        name: "Clay Tokens",
        description: "Shaped clay counters representing grain, livestock, or labour debts — the administrative precursor to tablets.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: MEASURES_AND_UNITS,
        era: Era::Neolithic,
        name: "Measures and Units",
        description: "Standard volumes (basket, jar, sack) and lengths (forearm, pace) by which work and yield are counted.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: RATION_ARITHMETIC,
        era: Era::Neolithic,
        name: "Ration Arithmetic",
        description: "Counting grain by adult-day, dividing stores by mouths, sizing the next year's seed reserve.",
        prerequisites: &[MEASURES_AND_UNITS],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: PRACTICAL_GEOMETRY,
        era: Era::Neolithic,
        name: "Practical Geometry",
        description: "Squaring a foundation, walking a circle, dropping a rope-and-stake right angle — geometry of the working site.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    // ── Phase H beliefs ─────────────────────────────────────────────────
    // Held with confidence in a belief group, not Learned-and-used. The
    // catalog `TruthStatus` axis decides whether the belief is genuinely
    // true (Heliocentric, Contagion — reserved), useful-but-false (Miasma,
    // Geocentric — drive sensible behaviour for the wrong reason), or
    // harmful (Spirit Illness — biases the patient toward ritual instead of
    // treatment).
    TechDef {
        id: SKY_DOME,
        era: Era::Paleolithic,
        name: "Sky Dome",
        description: "The sky is a solid vault above the earth — stars are points fixed to its inner face.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: GEOCENTRIC_COSMOS,
        era: Era::Neolithic,
        name: "Geocentric Cosmos",
        description: "Earth at the centre; sun, moon, and stars revolve in nested celestial spheres.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: SPIRIT_ILLNESS,
        era: Era::Paleolithic,
        name: "Spirit Illness",
        description: "Sickness is offence given to spirits or ancestors — the cure is propitiation and ritual.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: MIASMA_THEORY,
        era: Era::Neolithic,
        name: "Miasma Theory",
        description: "Sickness rises from bad air — foul vapours from waste, marsh, and rotting matter.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: ECLIPSE_OMENS,
        era: Era::Paleolithic,
        name: "Eclipse Omens",
        description: "An eclipse foretells calamity — the moon swallows the sun, the world holds its breath.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
    TechDef {
        id: WEATHER_OMENS,
        era: Era::Paleolithic,
        name: "Weather Omens",
        description: "Clouds, wind, and bird-flight foretell coming weather — the band reads the sky.",
        prerequisites: &[],
        triggers: &[],
        bonus: TechBonus::ZERO,
    },
];

// Discovery is now per-person and per-action; see
// `simulation::knowledge::discovery_system`. The old season-boundary
// faction-level roller has been removed.

#[cfg(test)]
mod tests {
    use super::*;

    /// `tech_def` indexes `TECH_TREE[id]` directly, so the array MUST be
    /// dense and ordered: entry `i` has `id == i`, and length == TECH_COUNT.
    #[test]
    fn tech_tree_is_dense_and_ordered() {
        assert_eq!(TECH_TREE.len(), TECH_COUNT);
        for (i, def) in TECH_TREE.iter().enumerate() {
            assert_eq!(def.id as usize, i, "TECH_TREE[{i}].id must equal {i}");
        }
    }

    /// Every prerequisite must be a lower id than the tech itself — the tree
    /// is a DAG topologically sorted by id (era-monotone), which guards
    /// against an unreachable tech and is relied on by seeding/discovery.
    #[test]
    fn prerequisites_precede_dependents() {
        for def in TECH_TREE.iter() {
            for &pre in def.prerequisites {
                assert!(
                    pre < def.id,
                    "tech {} ({}) lists prereq {} that is not earlier",
                    def.id,
                    def.name,
                    pre
                );
            }
        }
    }

    /// The siege / war-vehicle techs all sit in the Bronze-Age cap.
    #[test]
    fn siege_techs_are_bronze_age() {
        for id in [SIEGE_ENGINEERING, ARMOR_PLATING, POWERED_TRACTION] {
            assert_eq!(tech_def(id).era, Era::BronzeAge);
        }
        // Phase E added 14 building-technique entries (50..=63);
        // Phase G added 16 foundational knowledge entries (64..=79);
        // Phase H added 6 belief entries (80..=85).
        assert_eq!(TECH_COUNT, 86);
        // POWERED_TRACTION caps the line — its prereqs precede it.
        assert!(tech_def(POWERED_TRACTION)
            .prerequisites
            .contains(&SIEGE_ENGINEERING));
    }

    /// DAM_BUILDING is the dedicated Bronze-Age dam gate (split from
    /// BRIDGE_BUILDING), prereqed on bridge + monumental engineering.
    #[test]
    fn dam_building_is_bronze_age_engineering() {
        let d = tech_def(DAM_BUILDING);
        assert_eq!(d.era, Era::BronzeAge);
        assert!(d.prerequisites.contains(&BRIDGE_BUILDING));
        assert!(d.prerequisites.contains(&MONUMENTAL_BUILDING));
        assert_eq!(complexity(DAM_BUILDING), 5);
    }
}
