//! Wild herd LOD simulation (Phase 10).
//!
//! Background-scale herds of grazing animals (Horse, Cow) without paying for
//! hundreds of full ECS entities at once. Each herd is one row in
//! [`WildHerdRegistry`] carrying an `aggregate_count` of distant members and
//! a `leader_tile` that drifts seasonally. When the player camera (any
//! `SimulationFocus.is_camera` point) moves within `BLOOM_RADIUS_TILES` of a
//! leader, the herd "blooms" into individual entities clustered around the
//! leader, capped at `BLOOM_VISIBLE_CAP`. When the camera leaves
//! `COLLAPSE_RADIUS_TILES`, the herd collapses back to aggregate.
//!
//! Members spawned during bloom are normal `Horse` / `Cow` entities — they
//! interact with combat, hunting, and nomadic-faction tamed-herd cohesion
//! exactly like the per-tile `spawn_animals` cluster. The difference is
//! lifecycle: they're owned by the [`WildHerd::members`] vec and despawn on
//! collapse instead of persisting forever.

use ahash::AHashSet;
use bevy::prelude::*;

use crate::simulation::animals::{AnimalAI, AnimalNeeds, AnimalReproductionCooldown, Cow, Horse};
use crate::simulation::combat::CombatTarget;

/// Subset of grazer species that wild herds spawn at v1. Horse / Cow only
/// — wolf packs and deer remain individual via `spawn_animals`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WildHerdSpecies {
    Horse,
    Cow,
}
use crate::simulation::combat::{CombatCooldown, Health};
use crate::simulation::lod::LodLevel;
use crate::simulation::region::SimulationFocus;
use crate::simulation::reproduction::BiologicalSex;
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::world::chunk::{ChunkMap, CHUNK_SIZE};
use crate::world::seasons::{Calendar, Season, TICKS_PER_DAY};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

/// Number of distinct wild herds the world spawns at game start. Two horse +
/// one cow herd is enough to surface the LOD mechanic without flooding the
/// agent budget when all three bloom simultaneously.
pub const WILD_HERD_COUNT: u32 = 3;

/// Aggregate-count target per herd. The user explicitly asked for "large
/// herds of hundreds" — 120 lands close to the upper bound while keeping
/// bloom counts reasonable when all herds simultaneously enter player view.
pub const WILD_HERD_AGGREGATE: u16 = 120;

/// Radius (in tiles) the player camera must enter for a herd to bloom.
/// 32 tiles ≈ one chunk; bloomed members are visible inside the camera's
/// chunk-streamed window.
pub const BLOOM_RADIUS_TILES: i32 = 32;

/// Camera distance beyond which a bloomed herd collapses back to aggregate.
/// Hysteresis: collapse > bloom so a camera lingering at the edge doesn't
/// flicker.
pub const COLLAPSE_RADIUS_TILES: i32 = 48;

/// Cap on individual entities spawned at bloom time. Prevents a single
/// 200+ aggregate from instantly creating 200 ECS entities; the surplus
/// stays in `aggregate_count` invisible until predation drops it.
pub const BLOOM_VISIBLE_CAP: u16 = 60;

/// Fixed offset used for the herd's range-bounding box. The leader_tile
/// drifts by ±2 daily within `home_range`; in Winter the range biases
/// south by `WINTER_SHIFT_TILES`, in Spring north by `SPRING_SHIFT_TILES`.
pub const HERD_RANGE_HALF: i32 = 30;
pub const WINTER_SHIFT_TILES: i32 = 12;
pub const SPRING_SHIFT_TILES: i32 = 12;

#[derive(Clone, Debug)]
pub struct WildHerd {
    pub id: u32,
    pub species: WildHerdSpecies,
    /// Total head-count, both bloomed (live entities) and aggregate (data only).
    pub aggregate_count: u16,
    pub leader_tile: (i32, i32),
    /// Unbiased centre of the herd's seasonal home range. Leader drift is
    /// constrained to a `HERD_RANGE_HALF` chebyshev box around this point;
    /// season cues bias the box (see `wild_herd_migration_system`).
    pub range_center: (i32, i32),
    pub bloomed: bool,
    /// Live entities currently spawned. Empty when collapsed.
    pub members: Vec<Entity>,
}

#[derive(Resource, Default, Debug)]
pub struct WildHerdRegistry {
    pub herds: ahash::AHashMap<u32, WildHerd>,
    pub next_id: u32,
}

impl WildHerdRegistry {
    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }
}

/// Phase 10: marker on entities spawned during a wild-herd bloom. Removed
/// implicitly when the entity despawns. Lets `wild_herd_collapse_system`
/// distinguish bloom-spawned animals from the original `spawn_animals`
/// cluster (which we never despawn).
#[derive(Component, Clone, Copy)]
pub struct WildHerdMember {
    pub herd_id: u32,
}

/// Seed wild herds at world-gen. Runs in OnEnter(Playing), after the per-
/// tile `spawn_animals` pass. Picks random grassland tiles to anchor each
/// herd's `range_center` and `leader_tile`; collapsed by default until a
/// camera comes near.
pub fn seed_wild_herds_system(
    chunk_map: Res<ChunkMap>,
    mut registry: ResMut<WildHerdRegistry>,
) {
    if !registry.herds.is_empty() {
        return; // idempotent
    }
    // Walk loaded chunks for grassland tiles.
    let grass_tiles: Vec<(i32, i32)> = collect_grassland(&chunk_map);
    if grass_tiles.is_empty() {
        warn!("seed_wild_herds_system: no grassland tiles loaded — no wild herds spawned");
        return;
    }

    // Pick `WILD_HERD_COUNT` deterministic anchors from the grass set,
    // alternating Horse / Horse / Cow (matches the user's "horses or cows"
    // emphasis from the design pass).
    let mut rng_seed: u32 = 0x9E37_79B9;
    for i in 0..WILD_HERD_COUNT {
        // Splitmix-ish step for deterministic indexing.
        rng_seed = rng_seed.wrapping_mul(0x85EB_CA6B).wrapping_add(i);
        let idx = (rng_seed as usize) % grass_tiles.len();
        let centre = grass_tiles[idx];
        let species = if i == 2 {
            WildHerdSpecies::Cow
        } else {
            WildHerdSpecies::Horse
        };
        let id = registry.alloc_id();
        registry.herds.insert(
            id,
            WildHerd {
                id,
                species,
                aggregate_count: WILD_HERD_AGGREGATE,
                leader_tile: centre,
                range_center: centre,
                bloomed: false,
                members: Vec::new(),
            },
        );
        info!(
            "Seeded wild herd #{id} ({:?}) at {:?} with {} aggregate",
            species, centre, WILD_HERD_AGGREGATE,
        );
    }
}

fn collect_grassland(chunk_map: &ChunkMap) -> Vec<(i32, i32)> {
    use crate::world::tile::TileKind;
    let mut tiles: Vec<(i32, i32)> = Vec::new();
    for (&coord, _chunk) in chunk_map.0.iter() {
        for ly in 0..CHUNK_SIZE {
            for lx in 0..CHUNK_SIZE {
                let tx = coord.0 * CHUNK_SIZE as i32 + lx as i32;
                let ty = coord.1 * CHUNK_SIZE as i32 + ly as i32;
                let Some(kind) = chunk_map.tile_kind_at(tx, ty) else {
                    continue;
                };
                if kind == TileKind::Grass {
                    tiles.push((tx, ty));
                }
            }
        }
    }
    tiles
}

/// Daily leader drift within the herd's seasonal range. Seasonal bias
/// shifts the range centre on Spring / Winter transitions so herds
/// migrate longitudinally over a game-year (4 seasons).
pub fn wild_herd_migration_system(
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    chunk_map: Res<ChunkMap>,
    mut registry: ResMut<WildHerdRegistry>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;
    for herd in registry.herds.values_mut() {
        // Seasonal bias on the range centre. Computed each tick rather than
        // stored — cheap, idempotent, and makes a season change immediately
        // visible.
        let (cx, cy) = herd.range_center;
        let biased_centre = match calendar.season {
            Season::Winter => (cx, cy - WINTER_SHIFT_TILES),
            Season::Spring => (cx, cy + SPRING_SHIFT_TILES),
            Season::Summer | Season::Autumn => (cx, cy),
        };

        // Random-ish drift from current leader_tile keyed on (id, day).
        let day = now / TICKS_PER_DAY;
        let seed = herd.id.wrapping_mul(0x85EB_CA6B).wrapping_add(day);
        let dx = ((seed & 0b11) as i32) - 1; // -1..=2
        let dy = (((seed >> 2) & 0b11) as i32) - 1;
        let mut nx = herd.leader_tile.0 + dx;
        let mut ny = herd.leader_tile.1 + dy;

        // Clamp to the seasonal bounding box around `biased_centre`.
        nx = nx.clamp(
            biased_centre.0 - HERD_RANGE_HALF,
            biased_centre.0 + HERD_RANGE_HALF,
        );
        ny = ny.clamp(
            biased_centre.1 - HERD_RANGE_HALF,
            biased_centre.1 + HERD_RANGE_HALF,
        );

        // Don't walk into impassable terrain — fall back to current tile.
        if chunk_map.is_passable(nx, ny) {
            herd.leader_tile = (nx, ny);
        }
    }
}

/// Bloom + collapse based on camera-focus proximity. Sequential pass — runs
/// every tick but is cheap (one chebyshev distance per herd × camera).
pub fn wild_herd_bloom_system(
    mut commands: Commands,
    focus: Res<SimulationFocus>,
    chunk_map: Res<ChunkMap>,
    mut registry: ResMut<WildHerdRegistry>,
    member_q: Query<&WildHerdMember>,
    mut bucket_slot: Local<u32>,
) {
    // Pull the camera focus tile (unique). Other focus points are settled-
    // region centres; we only bloom against the player camera so off-screen
    // herds stay quiet.
    let camera_tile = focus
        .points
        .iter()
        .find(|p| p.is_camera)
        .map(|p| {
            (
                (p.world_pos.x / TILE_SIZE).floor() as i32,
                (p.world_pos.y / TILE_SIZE).floor() as i32,
            )
        });

    for herd in registry.herds.values_mut() {
        let near = camera_tile
            .map(|t| chebyshev(t, herd.leader_tile))
            .unwrap_or(i32::MAX);

        if !herd.bloomed && near <= BLOOM_RADIUS_TILES {
            // Bloom. Spawn min(BLOOM_VISIBLE_CAP, aggregate) entities
            // clustered in radius 4 of leader_tile. Subtract from aggregate
            // so collapse can restore them.
            let target_count = herd.aggregate_count.min(BLOOM_VISIBLE_CAP);
            let spawned = spawn_herd_members(
                &mut commands,
                &chunk_map,
                herd.id,
                herd.species,
                herd.leader_tile,
                target_count as u32,
                &mut bucket_slot,
            );
            herd.aggregate_count = herd.aggregate_count.saturating_sub(spawned.len() as u16);
            herd.members = spawned;
            herd.bloomed = true;
            info!(
                "Wild herd #{} bloomed ({} members visible, {} remain in aggregate)",
                herd.id,
                herd.members.len(),
                herd.aggregate_count,
            );
        } else if herd.bloomed && near > COLLAPSE_RADIUS_TILES {
            // Collapse. Restore only members still alive to aggregate —
            // anything killed by combat / predation while bloomed stays
            // gone. Stale `members` entries (entities already despawned)
            // are skipped via the `member_q.get` check; double-despawning
            // them via Commands is harmless but the count would be wrong.
            let alive: Vec<Entity> = herd
                .members
                .iter()
                .copied()
                .filter(|&e| member_q.get(e).is_ok())
                .collect();
            let restored = alive.len() as u16;
            let lost = herd.members.len() as u16 - restored;
            for &entity in alive.iter() {
                commands.entity(entity).despawn_recursive();
            }
            herd.members.clear();
            herd.aggregate_count = herd.aggregate_count.saturating_add(restored);
            herd.bloomed = false;
            if lost > 0 {
                info!(
                    "Wild herd #{} collapsed (restored {restored}, lost {lost} to predation; total {} now)",
                    herd.id, herd.aggregate_count,
                );
            } else {
                info!(
                    "Wild herd #{} collapsed (restored {restored} to aggregate; total {} now)",
                    herd.id, herd.aggregate_count,
                );
            }
        }
    }
}

/// Spawn `count` herd members on passable tiles in chebyshev radius 4 of
/// `centre`, with the appropriate species component bundle. Returns the
/// list of spawned entities so the herd can despawn them on collapse.
fn spawn_herd_members(
    commands: &mut Commands,
    chunk_map: &ChunkMap,
    herd_id: u32,
    species: WildHerdSpecies,
    centre: (i32, i32),
    count: u32,
    bucket_slot: &mut u32,
) -> Vec<Entity> {
    use crate::world::tile::TileKind;
    let mut placed: Vec<Entity> = Vec::with_capacity(count as usize);
    let mut used: AHashSet<(i32, i32)> = AHashSet::new();

    for i in 0..count {
        let mut tile_opt: Option<(i32, i32)> = None;
        for radius in 0..=8i32 {
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if dx.abs().max(dy.abs()) != radius {
                        continue;
                    }
                    let candidate = (centre.0 + dx, centre.1 + dy);
                    if used.contains(&candidate) {
                        continue;
                    }
                    if !chunk_map.is_passable(candidate.0, candidate.1) {
                        continue;
                    }
                    if matches!(
                        chunk_map.tile_kind_at(candidate.0, candidate.1),
                        Some(TileKind::Wall) | Some(TileKind::Stone)
                    ) {
                        continue;
                    }
                    tile_opt = Some(candidate);
                    break;
                }
                if tile_opt.is_some() {
                    break;
                }
            }
            if tile_opt.is_some() {
                break;
            }
        }
        let Some(tile) = tile_opt else { continue };
        used.insert(tile);

        let pos = tile_to_world(tile.0, tile.1);
        let slot = *bucket_slot % 20;
        *bucket_slot = bucket_slot.wrapping_add(1);

        let ai = AnimalAI {
            target_tile: tile,
            wander_timer: (i % 60) as f32 * 0.05,
            ..Default::default()
        };
        let needs = AnimalNeeds {
            hunger: fastrand::f32() * 60.0,
            sleep: fastrand::f32() * 40.0,
            reproduction: fastrand::f32() * 80.0,
        };

        // Bevy bundles cap at 15 elements in a single tuple. Group the
        // species marker + WildHerdMember into a sub-tuple so we stay
        // under the limit.
        let entity = match species {
            WildHerdSpecies::Horse => commands
                .spawn((
                    (Horse, WildHerdMember { herd_id }),
                    Transform::from_xyz(pos.x, pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(40),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    needs,
                    AnimalReproductionCooldown(0),
                    BiologicalSex::random(),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Horse),
                ))
                .id(),
            WildHerdSpecies::Cow => commands
                .spawn((
                    (Cow, WildHerdMember { herd_id }),
                    Transform::from_xyz(pos.x, pos.y, 1.0),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    ai,
                    Health::new(35),
                    CombatTarget::default(),
                    CombatCooldown::default(),
                    LodLevel::Full,
                    BucketSlot(slot),
                    needs,
                    AnimalReproductionCooldown(0),
                    BiologicalSex::random(),
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Horse),
                ))
                .id(),
        };
        placed.push(entity);
    }
    placed
}

#[inline]
fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}
