use bevy::prelude::*;

pub type TechId = u16;
pub const TECH_COUNT: usize = 43;
pub const ACTIVITY_COUNT: usize = 13;

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
            TechTrigger {
                activity: ActivityKind::Foraging,
                per_unit_chance: 0.001,
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
];

// Discovery is now per-person and per-action; see
// `simulation::knowledge::discovery_system`. The old season-boundary
// faction-level roller has been removed.
