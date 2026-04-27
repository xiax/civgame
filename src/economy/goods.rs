pub const GOOD_COUNT: usize = 15;

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
    Seed = 10,
    Weapon = 11,
    Armor = 12,
    Shield = 13,
    Skin = 14,
}

impl Good {
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
            Good::Seed => "Seed",
            Good::Weapon => "Weapon",
            Good::Armor => "Armor",
            Good::Shield => "Shield",
            Good::Skin => "Skin",
        }
    }

    pub fn is_edible(self) -> bool {
        matches!(self, Good::Fruit | Good::Meat | Good::Grain)
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
            Good::Seed,
            Good::Weapon,
            Good::Armor,
            Good::Shield,
            Good::Skin,
        ]
    }
}
