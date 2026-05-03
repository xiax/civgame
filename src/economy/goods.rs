pub const GOOD_COUNT: usize = 20;

/// Encumbrance class for carrying a good in hands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bulk {
    /// Many fit per hand (food, seeds, cloth, tools).
    Small,
    /// One stack per hand (single weapons, armor pieces, coal).
    OneHand,
    /// Both hands required for one stack (logs, stone blocks, iron ingots).
    TwoHand,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Good {
    #[default]
    Fruit = 0,
    Meat = 1,
    Grain = 2,
    Wood = 3,
    Stone = 4,
    Tools = 5,
    Cloth = 6,
    Coal = 7,
    Iron = 8,
    Luxury = 9,
    GrainSeed = 10,
    Weapon = 11,
    Armor = 12,
    Shield = 13,
    Skin = 14,
    Copper = 15,
    Tin = 16,
    Gold = 17,
    Silver = 18,
    BerrySeed = 19,
}

impl Good {
    /// Inverse of `good as u8`. Returns `None` for values outside the
    /// 0..GOOD_COUNT range. Used by executors that decode goods packed into
    /// `PersonAI::craft_recipe_id`.
    pub fn try_from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Good::Fruit),
            1 => Some(Good::Meat),
            2 => Some(Good::Grain),
            3 => Some(Good::Wood),
            4 => Some(Good::Stone),
            5 => Some(Good::Tools),
            6 => Some(Good::Cloth),
            7 => Some(Good::Coal),
            8 => Some(Good::Iron),
            9 => Some(Good::Luxury),
            10 => Some(Good::GrainSeed),
            19 => Some(Good::BerrySeed),
            11 => Some(Good::Weapon),
            12 => Some(Good::Armor),
            13 => Some(Good::Shield),
            14 => Some(Good::Skin),
            15 => Some(Good::Copper),
            16 => Some(Good::Tin),
            17 => Some(Good::Gold),
            18 => Some(Good::Silver),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Good::Fruit => "Fruit",
            Good::Meat => "Meat",
            Good::Grain => "Grain",
            Good::Wood => "Wood",
            Good::Stone => "Stone",
            Good::Tools => "Tools",
            Good::Cloth => "Cloth",
            Good::Coal => "Coal",
            Good::Iron => "Iron",
            Good::Luxury => "Luxury",
            Good::GrainSeed => "Grain Seed",
            Good::BerrySeed => "Berry Seed",
            Good::Weapon => "Weapon",
            Good::Armor => "Armor",
            Good::Shield => "Shield",
            Good::Skin => "Skin",
            Good::Copper => "Copper",
            Good::Tin => "Tin",
            Good::Gold => "Gold",
            Good::Silver => "Silver",
        }
    }

    pub fn is_edible(self) -> bool {
        matches!(self, Good::Fruit | Good::Meat | Good::Grain)
    }

    /// True if this good is a planting seed. Kept in sync with
    /// `PlantKind::seed_good()` — the simulation-side table is the source of
    /// truth for what plant grows from each seed; this match arm is the
    /// inverse used by routing logic that doesn't want to pull `PlantKind`
    /// into the economy module.
    pub fn is_seed(self) -> bool {
        matches!(self, Good::GrainSeed | Good::BerrySeed)
    }

    pub fn nutrition(self) -> u8 {
        match self {
            Good::Fruit => 85,
            Good::Grain => 150,
            Good::Meat => 255,
            _ => 0,
        }
    }

    /// How fun this good is to play with as a solo distraction. Drives the
    /// willpower refill rate when an agent runs the PlaySolo plan against an
    /// item (held or adjacent). 0 = useless to play with; higher is better.
    /// Social play with another agent uses a fixed value, not this.
    pub fn entertainment_value(self) -> u8 {
        match self {
            Good::Gold | Good::Silver => 30,
            Good::Luxury => 50,
            Good::Cloth | Good::Skin => 20,
            Good::Tools | Good::Weapon | Good::Shield | Good::Armor => 15,
            Good::Wood | Good::Stone | Good::Coal | Good::Iron | Good::Copper | Good::Tin => 5,
            Good::Fruit | Good::Meat | Good::Grain | Good::GrainSeed | Good::BerrySeed => 3,
        }
    }

    /// Weight of one unit of this good in grams.
    pub fn unit_weight_g(self) -> u32 {
        match self {
            Good::GrainSeed | Good::BerrySeed => 20,
            Good::Luxury => 100,
            Good::Grain => 200,
            Good::Fruit => 250,
            Good::Cloth => 400,
            Good::Meat => 800,
            Good::Skin => 900,
            Good::Tools => 1500,
            Good::Coal => 2000,
            Good::Weapon => 2500,
            Good::Wood => 3000,
            Good::Shield => 4000,
            Good::Iron => 4500,
            Good::Copper => 4500,
            Good::Tin => 4500,
            Good::Silver => 5500,
            Good::Gold => 6000,
            Good::Stone => 5000,
            Good::Armor => 8000,
        }
    }

    /// How this good must be held in hands when carried.
    pub fn bulk(self) -> Bulk {
        match self {
            Good::GrainSeed
            | Good::BerrySeed
            | Good::Luxury
            | Good::Grain
            | Good::Fruit
            | Good::Cloth
            | Good::Tools
            | Good::Meat => Bulk::Small,
            Good::Coal
            | Good::Skin
            | Good::Weapon
            | Good::Shield
            | Good::Armor
            | Good::Gold
            | Good::Silver => Bulk::OneHand,
            Good::Wood | Good::Stone | Good::Iron | Good::Copper | Good::Tin => Bulk::TwoHand,
        }
    }

    pub fn all() -> [Good; GOOD_COUNT] {
        [
            Good::Fruit,
            Good::Meat,
            Good::Grain,
            Good::Wood,
            Good::Stone,
            Good::Tools,
            Good::Cloth,
            Good::Coal,
            Good::Iron,
            Good::Luxury,
            Good::GrainSeed,
            Good::Weapon,
            Good::Armor,
            Good::Shield,
            Good::Skin,
            Good::Copper,
            Good::Tin,
            Good::Gold,
            Good::Silver,
            Good::BerrySeed,
        ]
    }
}
