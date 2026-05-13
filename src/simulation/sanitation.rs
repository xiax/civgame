//! Sanitation / contamination — Phase 4 of the thirst-and-drinking
//! system.
//!
//! Two primitives:
//! - `SanitationMap` (Resource): tile-keyed contamination scalar, the
//!   read surface every drink path consults via
//!   `is_water_contaminated(tile)`.
//! - `WastePile` (Component): a discrete waste source at a tile. Each
//!   pile carries an `intensity` and a tick stamp. `Latrine` is a marker
//!   that contains a co-located pile to a fraction of its raw radius.
//!
//! Two systems:
//! - `sanitation_emit_system` (Economy, daily): for every WastePile,
//!   distribute its `intensity` into `SanitationMap` within
//!   `CONTAMINATION_RADIUS` using `1/(d²+1)` falloff. Latrine-marked piles
//!   scale by `LATRINE_CONTAINMENT_FACTOR`.
//! - `sanitation_decay_system` (Economy, daily): exponential decay toward
//!   zero with `CONTAMINATION_HALF_LIFE_TICKS` (default 4 days). Cells
//!   below the floor are removed so the map stays sparse.
//!
//! Drink-path integration: `is_water_contaminated(tile)` returns true
//! when the cell's value exceeds `CONTAMINATION_DRINK_THRESHOLD`. The
//! drink executor and `animal_drink_system` use this to decide whether
//! a tile-drink rolls sickness (Phase 5).
//!
//! Latrine *structure placement* is deferred — today nothing creates
//! `WastePile` entities automatically. The agent defecation tick + the
//! seeded-camp Latrine placement land alongside the rest of Phase 4 in
//! a follow-up: the infrastructure shipped here is everything the
//! drink path needs, without yet committing to a settlement-layout
//! change.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::world::seasons::TICKS_PER_DAY;

/// Chebyshev radius around a `WastePile` that contributes contamination
/// to surrounding tiles.
pub const CONTAMINATION_RADIUS: i32 = 6;

/// Half-life of the per-cell contamination scalar, in ticks. With
/// `sanitation_decay_system` running daily, a single deposit drops to
/// roughly half value every 4 days; below `CONTAMINATION_FLOOR` the
/// entry is purged so the map stays sparse.
pub const CONTAMINATION_HALF_LIFE_TICKS: u64 = TICKS_PER_DAY as u64 * 4;

/// Map values below this are dropped from `SanitationMap` after decay.
pub const CONTAMINATION_FLOOR: f32 = 0.05;

/// Threshold at which a water cell is considered contaminated enough to
/// roll sickness on drink. Tuned so a fresh waste pile within ~3 tiles
/// flips the cell across the line.
pub const CONTAMINATION_DRINK_THRESHOLD: f32 = 0.5;

/// Latrine containment factor: piles tagged as `LatrineContained` emit
/// only this fraction of their raw intensity into the surrounding map.
/// Mathematically a clean swap for `intensity *= LATRINE_CONTAINMENT_FACTOR`.
pub const LATRINE_CONTAINMENT_FACTOR: f32 = 0.25;

/// Per-tile contamination scalar. Sparse — only cells actually touched
/// by a `WastePile` are present. Read by `is_water_contaminated`.
#[derive(Resource, Default)]
pub struct SanitationMap {
    pub cells: AHashMap<(i32, i32), f32>,
}

impl SanitationMap {
    pub fn contamination_at(&self, tile: (i32, i32)) -> f32 {
        self.cells.get(&tile).copied().unwrap_or(0.0)
    }

    /// Pure helper for tests: returns true when the cell exceeds the
    /// drinking-sickness threshold.
    pub fn is_contaminated(&self, tile: (i32, i32)) -> bool {
        self.contamination_at(tile) >= CONTAMINATION_DRINK_THRESHOLD
    }
}

/// Discrete contamination source. Spawned on agent defecation, corpse
/// rotting, or industrial-waste paths (deferred). The `created_tick`
/// stamp lets the emit system age piles uniformly; a pile keeps emitting
/// at full intensity until it's cleared (despawned) or decays out.
#[derive(Component, Clone, Copy, Debug)]
pub struct WastePile {
    /// Raw intensity in the same scale as `SanitationMap.cells`. A
    /// single agent-day deposit is ~1.0.
    pub intensity: f32,
    /// Game-tick stamp when the pile spawned. Reserved for future
    /// per-pile decay (currently the map-side decay handles aging).
    pub created_tick: u64,
}

/// Marker for `WastePile`s contained inside a `Latrine` structure. Emit
/// system multiplies their intensity by `LATRINE_CONTAINMENT_FACTOR`.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LatrineContained;

/// `Latrine` structure marker. Deferred Phase 4 follow-up wires
/// build-site plumbing; today the marker exists so `WastePile`s
/// authored near a latrine can attach `LatrineContained` and the
/// pipeline reflects the containment factor end-to-end.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Latrine;

/// Drive contamination spread into `SanitationMap` from every live
/// `WastePile`. Runs Economy schedule, daily. Per pile, walks chebyshev
/// `CONTAMINATION_RADIUS` and adds `intensity / (d² + 1)` to each cell.
/// Latrine-contained piles emit at `LATRINE_CONTAINMENT_FACTOR` strength.
pub fn sanitation_emit_system(
    mut map: ResMut<SanitationMap>,
    piles: Query<(&Transform, &WastePile, Option<&LatrineContained>)>,
) {
    use crate::world::terrain::TILE_SIZE;
    for (transform, pile, contained) in piles.iter() {
        let cx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cy = (transform.translation.y / TILE_SIZE).floor() as i32;
        let strength = if contained.is_some() {
            pile.intensity * LATRINE_CONTAINMENT_FACTOR
        } else {
            pile.intensity
        };
        for dy in -CONTAMINATION_RADIUS..=CONTAMINATION_RADIUS {
            for dx in -CONTAMINATION_RADIUS..=CONTAMINATION_RADIUS {
                let d2 = (dx * dx + dy * dy) as f32;
                let contribution = strength / (d2 + 1.0);
                if contribution < CONTAMINATION_FLOOR {
                    continue;
                }
                let key = (cx + dx, cy + dy);
                let entry = map.cells.entry(key).or_insert(0.0);
                *entry = (*entry + contribution).min(10.0);
            }
        }
    }
}

/// How often each Person agent emits a `WastePile` at their current
/// tile. One per two game-days keeps the contamination drumbeat at a
/// rate that materially fills `SanitationMap` near population centres
/// without overwhelming the daily emit pass.
pub const DEFECATION_INTERVAL_TICKS: u64 = crate::world::seasons::TICKS_PER_DAY as u64 * 2;

/// Per-pile intensity for an agent defecation. Calibrated so a small
/// village dropping waste every two days within ~5 tiles of a river
/// can flip the river contaminated before sanitation decay catches up.
pub const DEFECATION_INTENSITY: f32 = 1.0;

/// Latrine routing radius. When an agent defecates within this chebyshev
/// distance of a `Latrine` entity, the spawned `WastePile` is tagged
/// `LatrineContained` so the emit pass scales its intensity by
/// `LATRINE_CONTAINMENT_FACTOR`.
pub const LATRINE_ROUTING_RADIUS: i32 = 8;

/// Per-agent defecation marker. Stores the last tick the agent emitted
/// a `WastePile` so the system can stagger by tick offset rather than
/// firing every agent at once.
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct DefecationCadence {
    pub last_emit_tick: u64,
}

/// Daily-ish (every `DEFECATION_INTERVAL_TICKS`) pass that spawns a
/// `WastePile` at every active Person agent's tile, tagging it
/// `LatrineContained` when a `Latrine` entity is within
/// `LATRINE_ROUTING_RADIUS` chebyshev tiles. Stagger per-agent via a
/// per-agent `DefecationCadence.last_emit_tick`.
pub fn agent_defecation_system(
    clock: Res<crate::simulation::schedule::SimClock>,
    mut commands: Commands,
    mut agents: Query<
        (
            Entity,
            &Transform,
            &crate::simulation::lod::LodLevel,
            Option<&mut DefecationCadence>,
        ),
        With<crate::simulation::person::Person>,
    >,
    latrines: Query<&Transform, (With<Latrine>, Without<crate::simulation::person::Person>)>,
) {
    let now = clock.tick;
    for (entity, transform, lod, cadence) in agents.iter_mut() {
        if *lod == crate::simulation::lod::LodLevel::Dormant {
            continue;
        }
        // Stagger emission via per-agent tick offset.
        let offset = (entity.index() as u64) % DEFECATION_INTERVAL_TICKS;
        let last_tick = cadence.as_deref().map(|c| c.last_emit_tick).unwrap_or(0);
        let due = now >= last_tick + DEFECATION_INTERVAL_TICKS
            || (last_tick == 0 && now % DEFECATION_INTERVAL_TICKS == offset);
        if !due {
            continue;
        }

        let pos = transform.translation;
        let tx = (pos.x / crate::world::terrain::TILE_SIZE).floor() as i32;
        let ty = (pos.y / crate::world::terrain::TILE_SIZE).floor() as i32;

        let near_latrine = latrines.iter().any(|t| {
            let lx = (t.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
            let ly = (t.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
            (lx - tx).abs().max((ly - ty).abs()) <= LATRINE_ROUTING_RADIUS
        });

        let mut pile = commands.spawn((
            Transform::from_xyz(pos.x, pos.y, 0.1),
            GlobalTransform::default(),
            WastePile {
                intensity: DEFECATION_INTENSITY,
                created_tick: now,
            },
        ));
        if near_latrine {
            pile.insert(LatrineContained);
        }

        if let Some(mut c) = cadence {
            c.last_emit_tick = now;
        } else {
            commands
                .entity(entity)
                .insert(DefecationCadence { last_emit_tick: now });
        }
    }
}

/// Pure-function core of daily decay. Easier to unit-test than the
/// `ResMut`-bound system. One call ≈ one game-day's decay.
pub fn apply_daily_decay(map: &mut SanitationMap) {
    // factor = 0.5^(TICKS_PER_DAY / HALF_LIFE). With HALF_LIFE = 4 days
    // the per-call multiplier is 2^(-1/4) ≈ 0.841.
    let factor = 0.5f32.powf(TICKS_PER_DAY as f32 / CONTAMINATION_HALF_LIFE_TICKS as f32);
    map.cells.retain(|_, v| {
        *v *= factor;
        *v >= CONTAMINATION_FLOOR
    });
}

/// Daily decay pass: exponential half-life toward zero. Cells below the
/// floor are removed so the map stays sparse.
pub fn sanitation_decay_system(mut map: ResMut<SanitationMap>) {
    apply_daily_decay(&mut map);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_map_reads_zero() {
        let m = SanitationMap::default();
        assert_eq!(m.contamination_at((0, 0)), 0.0);
        assert!(!m.is_contaminated((0, 0)));
    }

    #[test]
    fn contamination_decays_exponentially() {
        let mut m = SanitationMap::default();
        m.cells.insert((0, 0), 1.0);
        apply_daily_decay(&mut m);
        // ~0.841 of original after one daily call.
        let v = m.contamination_at((0, 0));
        assert!(v > 0.8 && v < 0.9, "expected ~0.84, got {v}");
    }

    #[test]
    fn floor_purges_stale_cells() {
        let mut m = SanitationMap::default();
        m.cells.insert((0, 0), 0.05);
        for _ in 0..30 {
            apply_daily_decay(&mut m);
        }
        // After many days well below floor — cell purged.
        assert_eq!(m.contamination_at((0, 0)), 0.0);
    }

    #[test]
    fn drink_threshold_check() {
        let mut m = SanitationMap::default();
        m.cells.insert((1, 1), CONTAMINATION_DRINK_THRESHOLD + 0.1);
        m.cells
            .insert((2, 2), CONTAMINATION_DRINK_THRESHOLD - 0.1);
        assert!(m.is_contaminated((1, 1)));
        assert!(!m.is_contaminated((2, 2)));
    }
}
