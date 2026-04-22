use bevy::prelude::*;

pub const SKILL_COUNT: usize = 8;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillKind {
    Farming  = 0,
    Mining   = 1,
    Building = 2,
    Trading  = 3,
    Combat   = 4,
    Crafting = 5,
    Social   = 6,
    Medicine = 7,
}

impl SkillKind {
    pub fn name(self) -> &'static str {
        match self {
            SkillKind::Farming  => "Farming",
            SkillKind::Mining   => "Mining",
            SkillKind::Building => "Building",
            SkillKind::Trading  => "Trading",
            SkillKind::Combat   => "Combat",
            SkillKind::Crafting => "Crafting",
            SkillKind::Social   => "Social",
            SkillKind::Medicine => "Medicine",
        }
    }
}

/// 8 bytes — one slot per skill.
#[derive(Component, Clone, Copy)]
pub struct Skills(pub [u8; SKILL_COUNT]);

impl Default for Skills {
    fn default() -> Self {
        Skills([5; SKILL_COUNT])
    }
}

impl Skills {
    pub fn get(&self, kind: SkillKind) -> u8 {
        self.0[kind as usize]
    }

    pub fn gain_xp(&mut self, kind: SkillKind, amount: u8) {
        self.0[kind as usize] = self.0[kind as usize].saturating_add(amount);
    }
}
