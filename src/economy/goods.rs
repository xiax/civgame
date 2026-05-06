pub const GOOD_COUNT: usize = 22;

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

impl Bulk {
    /// Catalog-driven bulk lookup. Phase 2b migration accessor — Phase
    /// 2c will replace `Good::bulk()` call sites with this. Returns
    /// `None` only when the resource is unknown to the catalog (which
    /// indicates a programming error: the catalog must define every
    /// resource referenced by simulation code).
    pub fn for_resource(
        id: super::resource_catalog::ResourceId,
        catalog: &super::resource_catalog::ResourceCatalog,
    ) -> Option<Bulk> {
        catalog.get(id).map(|d| d.bulk.as_bulk())
    }
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
    /// Clay tablet — encodes a single tech for reading. `tech_payload` on
    /// `Item` carries the encoded TechId. Reusable; consumed only by decay.
    ClayTablet = 20,
    /// Book — durable codex form of a tablet. Same payload semantics, longer
    /// to craft, lighter to carry.
    Book = 21,
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
            20 => Some(Good::ClayTablet),
            21 => Some(Good::Book),
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
            Good::ClayTablet => "Clay Tablet",
            Good::Book => "Book",
        }
    }

    /// True if this good can be eaten. Phase 2c: sources from the
    /// catalog (`edible_calories.is_some()`) rather than a hardcoded
    /// match. Adding a new edible resource = set `edible_calories` in
    /// the RON file; no Rust edit required.
    pub fn is_edible(self) -> bool {
        let id = super::core_ids::good_to_resource_id(self);
        super::core_ids::catalog()
            .get(id)
            .and_then(|d| d.edible_calories)
            .is_some()
    }

    /// True if this good is a planting seed. Phase 2c: sources from
    /// the catalog (`class == Seed`).
    pub fn is_seed(self) -> bool {
        let id = super::core_ids::good_to_resource_id(self);
        matches!(
            super::core_ids::catalog().get(id).map(|d| d.class),
            Some(super::resource_catalog::ResourceClass::Seed)
        )
    }

    /// Calories restored per unit when eaten. Phase 2c: sources from the
    /// catalog (`edible_calories`); `None`/inedible → 0. Truncates from
    /// `u16` to `u8` since the legacy contract was `u8` — values past
    /// 255 cap rather than wrap.
    pub fn nutrition(self) -> u8 {
        let id = super::core_ids::good_to_resource_id(self);
        super::core_ids::catalog()
            .get(id)
            .and_then(|d| d.edible_calories)
            .map(|c| c.min(u8::MAX as u16) as u8)
            .unwrap_or(0)
    }

    /// How fun this good is to play with as a solo distraction. Phase
    /// 2c: sources from the catalog (`entertainment_value`). Drives the
    /// PlaySolo plan's willpower refill rate.
    pub fn entertainment_value(self) -> u8 {
        let id = super::core_ids::good_to_resource_id(self);
        super::core_ids::catalog()
            .get(id)
            .map(|d| d.entertainment_value)
            .unwrap_or(0)
    }

    /// Weight of one unit of this good in grams. Phase 2c: sources
    /// from the catalog (`weight_g`).
    pub fn unit_weight_g(self) -> u32 {
        let id = super::core_ids::good_to_resource_id(self);
        super::core_ids::catalog()
            .get(id)
            .map(|d| d.weight_g)
            .unwrap_or(0)
    }

    /// How this good must be held in hands when carried. Phase 2c:
    /// sources from the catalog (`bulk`). Defaults to `Small` if the
    /// catalog is missing the entry — should be impossible since every
    /// `Good` is in `core.ron`.
    pub fn bulk(self) -> Bulk {
        let id = super::core_ids::good_to_resource_id(self);
        super::core_ids::catalog()
            .get(id)
            .map(|d| d.bulk.as_bulk())
            .unwrap_or(Bulk::Small)
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
            Good::ClayTablet,
            Good::Book,
        ]
    }
}
