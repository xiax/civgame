use bevy::prelude::*;

#[derive(Component, Clone, Copy, Debug)]
pub struct Stats {
    pub strength: u8,
    pub dexterity: u8,
    pub constitution: u8,
    pub intelligence: u8,
    pub wisdom: u8,
    pub charisma: u8,
}

fn roll_3d6() -> u8 {
    (fastrand::u8(1..=6) + fastrand::u8(1..=6) + fastrand::u8(1..=6)) as u8
}

impl Stats {
    pub fn roll_3d6() -> Self {
        Stats {
            strength: roll_3d6(),
            dexterity: roll_3d6(),
            constitution: roll_3d6(),
            intelligence: roll_3d6(),
            wisdom: roll_3d6(),
            charisma: roll_3d6(),
        }
    }

    pub fn inherit(a: &Stats, b: &Stats) -> Self {
        Stats {
            strength: blend(a.strength, b.strength),
            dexterity: blend(a.dexterity, b.dexterity),
            constitution: blend(a.constitution, b.constitution),
            intelligence: blend(a.intelligence, b.intelligence),
            wisdom: blend(a.wisdom, b.wisdom),
            charisma: blend(a.charisma, b.charisma),
        }
    }
}

fn blend(a: u8, b: u8) -> u8 {
    let mean = ((a as i16 + b as i16) / 2) as i16;
    let variance = fastrand::i16(-1..=1);
    (mean + variance).max(1) as u8
}

pub fn modifier(score: u8) -> i8 {
    (score as i16 - 10).div_euclid(2) as i8
}
