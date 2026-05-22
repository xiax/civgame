//! Swimming mechanics — the per-agent half of Phase 2 of `plans/swimming.md`.
//!
//! Amphibious *pathfinding* (see `pathfinding/`) lets a human plan a route
//! that crosses water. This module owns what happens **while** that human
//! is in the water: a `SwimmingState` is attached on water entry and
//! removed on bank arrival; swimming drains `Energy` heavily and grants
//! `SkillKind::Swimming` XP; an exhausted swimmer in deep water starts to
//! drown after a grace period.

use bevy::prelude::*;

use super::combat::{Body, BodyPart};
use super::energy::Energy;
use super::lod::LodLevel;
use super::person::Person;
use super::schedule::{BucketSlot, SimClock};
use super::skills::{SkillKind, Skills};
use crate::world::chunk::ChunkMap;
use crate::world::terrain::TILE_SIZE;
use crate::world::water_current::WaterCurrentField;

/// Extra `Energy` drained per real second while in the water — on top of
/// the per-tile movement drain. Swimming is one of the most tiring
/// activities an agent can do.
pub const SWIM_ENERGY_DRAIN: f32 = 4.0;

/// Swimming XP granted per `SWIM_XP_INTERVAL_TICKS` spent wet.
pub const SWIM_XP_PER_GRANT: u32 = 3;
/// Ticks between Swimming-XP grants while wet.
pub const SWIM_XP_INTERVAL_TICKS: u64 = 20;

/// Water-column depth (Z-units) at or above which a tile counts as "deep"
/// — deep enough that an exhausted, failing swimmer can drown.
pub const DEEP_WATER_DEPTH: f32 = 1.5;

/// Consecutive exhausted-in-deep-water ticks an agent survives before
/// drowning damage begins. Generous: fatigue and the slowdown bite first
/// (the plan's "fatigue-first risk"); drowning is the last resort.
pub const DROWN_GRACE_TICKS: u32 = 200;

/// `Body` torso damage applied per tick once a swimmer is past the
/// drowning grace period.
pub const DROWN_DAMAGE_PER_TICK: u8 = 1;

/// Attached to a `Person` while they are in the water; removed on bank
/// arrival. Drives swim XP accrual and the fatigue-first drowning model.
#[derive(Component, Clone, Copy, Debug)]
pub struct SwimmingState {
    /// Ticks spent continuously wet this crossing.
    pub wet_ticks: u32,
    /// Consecutive ticks spent exhausted while in deep water. Reset
    /// whenever the swimmer recovers above exhaustion or reaches shallow
    /// water. Drowning damage begins once it passes `DROWN_GRACE_TICKS`.
    pub exhausted_ticks: u32,
    /// Last `SimClock.tick` a Swimming-XP grant fired.
    pub last_xp_tick: u64,
    /// Last dry tile the agent stood on before entering the water — the
    /// bank to retreat to. `None` until a dry tile has been observed.
    pub last_safe_tile: Option<(i32, i32, i8)>,
    /// Last water tile occupied — diffed against the current tile to
    /// recover the swim heading (for current resist / assist).
    pub last_tile: Option<(i32, i32)>,
}

impl SwimmingState {
    fn new(last_safe: Option<(i32, i32, i8)>) -> Self {
        Self {
            wet_ticks: 0,
            exhausted_ticks: 0,
            last_xp_tick: 0,
            last_safe_tile: last_safe,
            last_tile: None,
        }
    }

    /// True once the swimmer has been exhausted in deep water long enough
    /// that drowning damage applies.
    pub fn is_drowning(&self) -> bool {
        self.exhausted_ticks > DROWN_GRACE_TICKS
    }
}

/// Attach / maintain / remove `SwimmingState` and apply its effects.
/// Runs in `Sequential` after `movement_system` so it reads the agent's
/// post-move tile.
pub fn swimming_system(
    mut commands: Commands,
    time: Res<Time>,
    clock: Res<SimClock>,
    chunk_map: Res<ChunkMap>,
    // `Option` so the headless test fixture (no `WorldPlugin` water
    // resources) doesn't panic — absent ⇒ no currents.
    current_field: Option<Res<WaterCurrentField>>,
    mut query: Query<
        (
            Entity,
            &Transform,
            &BucketSlot,
            &LodLevel,
            Option<&mut Energy>,
            Option<&mut Skills>,
            Option<&mut Body>,
            Option<&mut SwimmingState>,
        ),
        With<Person>,
    >,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    for (entity, transform, slot, lod, mut energy, mut skills, mut body, mut swim) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let depth = chunk_map.water_depth_at(tx, ty);
        let in_water = depth > 0.0;
        let here_z = chunk_map.surface_z_at(tx, ty) as i8;

        if !in_water {
            // On dry land — remember the bank, shed any SwimmingState.
            if swim.is_some() {
                commands.entity(entity).remove::<SwimmingState>();
            }
            continue;
        }

        // In water. Attach SwimmingState on entry; the dry-land branch
        // above never ran this tick so `last_safe_tile` is stale-ok.
        let Some(swim) = swim.as_mut() else {
            commands
                .entity(entity)
                .insert(SwimmingState::new(Some((tx, ty, here_z))));
            continue;
        };

        if !clock.is_active(slot.0) {
            continue;
        }

        swim.wet_ticks = swim.wet_ticks.saturating_add(1);

        // Current resist / assist. The swim heading is recovered by
        // diffing this tile against the last; `resist` is +1 swimming
        // straight into a full current, −1 straight downstream, 0 across
        // or in still water.
        let current = current_field
            .as_ref()
            .and_then(|f| f.at(tx, ty))
            .map(|c| c.flow())
            .unwrap_or(Vec2::ZERO);
        let resist = match swim.last_tile {
            Some((px, py)) if (px, py) != (tx, ty) && current != Vec2::ZERO => {
                let heading = Vec2::new((tx - px) as f32, (ty - py) as f32).normalize_or_zero();
                // Heading into the current opposes `current` → negative dot
                // → positive resist.
                (-heading.dot(current)).clamp(-1.0, 1.0)
            }
            _ => 0.0,
        };
        swim.last_tile = Some((tx, ty));

        // Heavy energy drain — swimming upstream costs more, downstream
        // less. `resist` scales the drain within ±50%.
        if let Some(energy) = energy.as_deref_mut() {
            energy.drain(SWIM_ENERGY_DRAIN * (1.0 + 0.5 * resist) * dt);
        }

        // Swimming XP — periodic so a long crossing rewards practice;
        // resisting a meaningful current grants a bonus.
        if clock.tick.saturating_sub(swim.last_xp_tick) >= SWIM_XP_INTERVAL_TICKS {
            swim.last_xp_tick = clock.tick;
            if let Some(skills) = skills.as_deref_mut() {
                let bonus = if resist > 0.5 { SWIM_XP_PER_GRANT } else { 0 };
                skills.gain_xp(SkillKind::Swimming, SWIM_XP_PER_GRANT + bonus);
            }
        }

        // Fatigue-first risk: drowning only begins once the swimmer has
        // been exhausted in *deep* water past the grace period.
        let exhausted = energy.as_deref().map_or(false, |e| e.is_exhausted());
        if exhausted && depth >= DEEP_WATER_DEPTH {
            swim.exhausted_ticks = swim.exhausted_ticks.saturating_add(1);
        } else {
            swim.exhausted_ticks = 0;
        }
        if swim.is_drowning() {
            if let Some(body) = body.as_deref_mut() {
                let torso = body.get_mut(BodyPart::Torso);
                torso.current = torso.current.saturating_sub(DROWN_DAMAGE_PER_TICK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drowning_only_after_grace() {
        let mut s = SwimmingState::new(None);
        assert!(!s.is_drowning());
        s.exhausted_ticks = DROWN_GRACE_TICKS;
        assert!(!s.is_drowning());
        s.exhausted_ticks = DROWN_GRACE_TICKS + 1;
        assert!(s.is_drowning());
    }
}
