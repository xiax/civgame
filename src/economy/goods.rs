pub const GOOD_COUNT: usize = 12;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Good {
    #[default]
    Food   = 0,
    Wood   = 1,
    Stone  = 2,
    Tools  = 3,
    Cloth  = 4,
    Coal   = 5,
    Iron   = 6,
    Luxury = 7,
    Seed   = 8,
    Weapon = 9,
    Armor  = 10,
    Shield = 11,
}

impl Good {
    pub fn name(self) -> &'static str {
        match self {
            Good::Food   => "Food",
            Good::Wood   => "Wood",
            Good::Stone  => "Stone",
            Good::Tools  => "Tools",
            Good::Cloth  => "Cloth",
            Good::Coal   => "Coal",
            Good::Iron   => "Iron",
            Good::Luxury => "Luxury",
            Good::Seed   => "Seed",
            Good::Weapon => "Weapon",
            Good::Armor  => "Armor",
            Good::Shield => "Shield",
        }
    }

    pub fn all() -> [Good; GOOD_COUNT] {
        [
            Good::Food, Good::Wood, Good::Stone, Good::Tools,
            Good::Cloth, Good::Coal, Good::Iron, Good::Luxury, Good::Seed,
            Good::Weapon, Good::Armor, Good::Shield,
        ]
    }
}
