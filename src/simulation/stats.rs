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

fn roll_3d6_from(rng: &mut fastrand::Rng) -> u8 {
    rng.u8(1..=6) + rng.u8(1..=6) + rng.u8(1..=6)
}

impl Stats {
    /// Deterministic 3d6-per-attribute roll from a caller-supplied local RNG.
    /// Production-sim callers build it from [`super::sim_rng::SimRng`] keyed on
    /// the spawned person's stable id (spawn slot / entity).
    pub fn roll_3d6_from(rng: &mut fastrand::Rng) -> Self {
        Stats {
            strength: roll_3d6_from(rng),
            dexterity: roll_3d6_from(rng),
            constitution: roll_3d6_from(rng),
            intelligence: roll_3d6_from(rng),
            wisdom: roll_3d6_from(rng),
            charisma: roll_3d6_from(rng),
        }
    }

    /// Dev/test convenience; production simulation uses
    /// [`roll_3d6_from`](Self::roll_3d6_from).
    pub fn roll_3d6() -> Self {
        Self::roll_3d6_from(&mut fastrand::Rng::new())
    }

    /// Deterministic inheritance blend from a caller-supplied local RNG.
    pub fn inherit_from(a: &Stats, b: &Stats, rng: &mut fastrand::Rng) -> Self {
        Stats {
            strength: blend(a.strength, b.strength, rng),
            dexterity: blend(a.dexterity, b.dexterity, rng),
            constitution: blend(a.constitution, b.constitution, rng),
            intelligence: blend(a.intelligence, b.intelligence, rng),
            wisdom: blend(a.wisdom, b.wisdom, rng),
            charisma: blend(a.charisma, b.charisma, rng),
        }
    }

    /// Dev/test convenience; production simulation uses
    /// [`inherit_from`](Self::inherit_from).
    pub fn inherit(a: &Stats, b: &Stats) -> Self {
        Self::inherit_from(a, b, &mut fastrand::Rng::new())
    }
}

fn blend(a: u8, b: u8, rng: &mut fastrand::Rng) -> u8 {
    let mean = ((a as i16 + b as i16) / 2) as i16;
    let variance = rng.i16(-1..=1);
    (mean + variance).max(1) as u8
}

pub fn modifier(score: u8) -> i8 {
    (score as i16 - 10).div_euclid(2) as i8
}

/// Total complexity points a person can hold across their Learned techs.
/// Intelligence 10 → 20 points (≈12 simple OR ≈3 advanced techs); int 16 → 32.
#[inline]
pub fn knowledge_capacity(intelligence: u8) -> u16 {
    (intelligence as u16).saturating_mul(2)
}
