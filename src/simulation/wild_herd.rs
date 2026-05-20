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
use crate::world::seasons::{Calendar, Season, TICKS_PER_DAY, TICKS_PER_SEASON};
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

/// P6: chebyshev radius around the leader within which a Wolf or armed
/// hunter triggers a `flee_until_tick = now + TICKS_PER_DAY` reaction.
pub const HERD_FLEE_RADIUS: i32 = 8;
/// P6: chebyshev radius from any nomadic camp tile that the herd will
/// drift away from (encouraging predators-vs-camps spatial pressure).
pub const HERD_CAMP_AVOID_RADIUS: i32 = 10;
/// P6: per-season replenishment, capped by `WILD_HERD_AGGREGATE_CAP`.
/// Births skip Winter (food scarce) — Spring/Summer/Autumn all add.
pub const WILD_HERD_BIRTH_PER_SEASON: u16 = 12;
pub const WILD_HERD_AGGREGATE_CAP: u16 = 200;
/// P6: water probe radius used during the daily water-seek bias. Beyond
/// this distance the herd doesn't sense water and falls back to the
/// season-biased random drift.
pub const HERD_WATER_PROBE_RADIUS: i32 = 12;

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
    /// P6: tick when the herd last fled from a predator/hunter sighting.
    /// While `now < flee_until_tick`, water-seek + camp-avoid biases are
    /// suppressed in favour of the flee direction.
    pub flee_until_tick: u32,
    /// P6: tick of the last seasonal birth bump. Throttles
    /// `aggregate_count` growth to once per `TICKS_PER_SEASON` so a
    /// summer-long camera focus doesn't rapidly overflow `WILD_HERD_AGGREGATE_CAP`.
    pub last_birthed_tick: u32,
    /// Flow-field cluster id allocated from `HerdClusterGen` at seed time.
    /// Stamped onto every bloomed member's `HerdMember` so the animal_paths
    /// cohesion/repulsion fields can group them.
    pub cluster_id: u32,
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

    /// Removes `entity` from herd `herd_id`'s `members` list and decrements
    /// `aggregate_count` so the bloom/collapse churn doesn't restore or
    /// despawn it. Called by `tame_task_system` when a wild-herd member is
    /// successfully tamed.
    pub fn remove_member(&mut self, herd_id: u32, entity: Entity) {
        if let Some(herd) = self.herds.get_mut(&herd_id) {
            herd.members.retain(|&e| e != entity);
            herd.aggregate_count = herd.aggregate_count.saturating_sub(1);
        }
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
    mut herd_gen: ResMut<crate::simulation::animals::HerdClusterGen>,
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
        let cluster_id = herd_gen.next;
        herd_gen.next = herd_gen.next.wrapping_add(1);
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
                flee_until_tick: 0,
                last_birthed_tick: 0,
                cluster_id,
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
/// migrate longitudinally over a game-year (4 seasons). P6 layered:
/// (a) flee from nearby Wolves / armed hunters; (b) bias toward the
/// nearest water tile when none is adjacent; (c) avoid nomadic camp
/// tiles within `HERD_CAMP_AVOID_RADIUS`; (d) per-season birth bumps to
/// `aggregate_count` (capped at `WILD_HERD_AGGREGATE_CAP`).
pub fn wild_herd_migration_system(
    clock: Res<SimClock>,
    calendar: Res<Calendar>,
    chunk_map: Res<ChunkMap>,
    registry_factions: Res<crate::simulation::faction::FactionRegistry>,
    wolf_q: Query<&Transform, With<crate::simulation::animals::Wolf>>,
    mut registry: ResMut<WildHerdRegistry>,
) {
    if clock.tick % TICKS_PER_DAY as u64 != 0 {
        return;
    }
    let now = clock.tick as u32;
    let day = now / TICKS_PER_DAY;
    for herd in registry.herds.values_mut() {
        let (cx, cy) = herd.range_center;
        let biased_centre = match calendar.season {
            Season::Winter => (cx, cy - WINTER_SHIFT_TILES),
            Season::Spring => (cx, cy + SPRING_SHIFT_TILES),
            Season::Summer | Season::Autumn => (cx, cy),
        };

        // P6 (a): predator flee. Scan nearby Wolves; if any within
        // `HERD_FLEE_RADIUS` of leader, set flee_until and bias drift
        // away from the average wolf direction.
        let mut fleeing_dir: Option<(i32, i32)> = None;
        if now >= herd.flee_until_tick {
            let mut sum_dx = 0i32;
            let mut sum_dy = 0i32;
            let mut count = 0i32;
            for w_t in wolf_q.iter() {
                let wx = (w_t.translation.x / TILE_SIZE).floor() as i32;
                let wy = (w_t.translation.y / TILE_SIZE).floor() as i32;
                if chebyshev((wx, wy), herd.leader_tile) <= HERD_FLEE_RADIUS {
                    sum_dx += herd.leader_tile.0 - wx;
                    sum_dy += herd.leader_tile.1 - wy;
                    count += 1;
                }
            }
            if count > 0 {
                let avg_dx = sum_dx.signum().max(-1).min(1);
                let avg_dy = sum_dy.signum().max(-1).min(1);
                fleeing_dir = Some((avg_dx * 3, avg_dy * 3));
                herd.flee_until_tick = now + TICKS_PER_DAY;
            }
        } else {
            // Already in flee state — keep walking the same way.
            fleeing_dir = Some((0, 0));
        }

        let (mut dx, mut dy);
        if let Some((fx, fy)) = fleeing_dir {
            // Flee dominates other biases. If the flee direction is zero
            // (lingering flee state), fall through to the random drift
            // inside the seasonal box.
            if fx != 0 || fy != 0 {
                dx = fx;
                dy = fy;
            } else {
                let seed = herd.id.wrapping_mul(0x85EB_CA6B).wrapping_add(day);
                dx = ((seed & 0b11) as i32) - 1;
                dy = (((seed >> 2) & 0b11) as i32) - 1;
            }
        } else {
            // P6 (b): water-seek when no water nearby. If no water tile
            // within chebyshev 4 of leader, override drift to step toward
            // the nearest water tile within HERD_WATER_PROBE_RADIUS.
            let immediate_water_present = water_within(&chunk_map, herd.leader_tile, 4);
            if !immediate_water_present {
                if let Some(target) =
                    nearest_water(&chunk_map, herd.leader_tile, HERD_WATER_PROBE_RADIUS)
                {
                    dx = (target.0 - herd.leader_tile.0).signum();
                    dy = (target.1 - herd.leader_tile.1).signum();
                } else {
                    let seed = herd.id.wrapping_mul(0x85EB_CA6B).wrapping_add(day);
                    dx = ((seed & 0b11) as i32) - 1;
                    dy = (((seed >> 2) & 0b11) as i32) - 1;
                }
            } else {
                let seed = herd.id.wrapping_mul(0x85EB_CA6B).wrapping_add(day);
                dx = ((seed & 0b11) as i32) - 1;
                dy = (((seed >> 2) & 0b11) as i32) - 1;
            }

            // P6 (c): camp avoidance — push away from any nomadic faction
            // home tile within HERD_CAMP_AVOID_RADIUS. Layered on top of
            // the water/random drift; flee already short-circuited.
            for faction in registry_factions.factions.values() {
                if !faction.caps.home.is_mobile() {
                    continue;
                }
                let d = chebyshev(faction.home_tile, herd.leader_tile);
                if d <= HERD_CAMP_AVOID_RADIUS {
                    dx += (herd.leader_tile.0 - faction.home_tile.0).signum() * 4;
                    dy += (herd.leader_tile.1 - faction.home_tile.1).signum() * 4;
                }
            }
        }

        let mut nx = herd.leader_tile.0 + dx;
        let mut ny = herd.leader_tile.1 + dy;
        nx = nx.clamp(
            biased_centre.0 - HERD_RANGE_HALF,
            biased_centre.0 + HERD_RANGE_HALF,
        );
        ny = ny.clamp(
            biased_centre.1 - HERD_RANGE_HALF,
            biased_centre.1 + HERD_RANGE_HALF,
        );
        if chunk_map.is_passable(nx, ny) {
            herd.leader_tile = (nx, ny);
        }

        // P6 (d): seasonal birth. Throttled to once per `TICKS_PER_SEASON`
        // (skips Winter — predators dominate, food scarce).
        if !matches!(calendar.season, Season::Winter)
            && now.saturating_sub(herd.last_birthed_tick) >= TICKS_PER_SEASON
            && herd.aggregate_count < WILD_HERD_AGGREGATE_CAP
        {
            herd.aggregate_count = herd
                .aggregate_count
                .saturating_add(WILD_HERD_BIRTH_PER_SEASON)
                .min(WILD_HERD_AGGREGATE_CAP);
            herd.last_birthed_tick = now;
        }
    }
}

fn water_within(chunk_map: &ChunkMap, tile: (i32, i32), radius: i32) -> bool {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if let Some(kind) = chunk_map.tile_kind_at(tile.0 + dx, tile.1 + dy) {
                if kind.is_water_like() {
                    return true;
                }
            }
        }
    }
    false
}

/// Walks chebyshev rings from `from`, preferring fresh water (rivers) over
/// salt (ocean/lake). At each radius: if any ring tile is `River`, return it
/// immediately; otherwise remember the first salt-water tile in this ring and
/// keep looking for fresh in larger rings. Falls back to the closest salt if
/// no fresh appears within `max_radius`.
fn nearest_water(chunk_map: &ChunkMap, from: (i32, i32), max_radius: i32) -> Option<(i32, i32)> {
    let mut closest_salt: Option<(i32, i32)> = None;
    for r in 1..=max_radius {
        let mut ring_salt: Option<(i32, i32)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue;
                }
                let tile = (from.0 + dx, from.1 + dy);
                let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) else {
                    continue;
                };
                if kind.is_freshwater() {
                    return Some(tile);
                }
                if ring_salt.is_none() && kind.is_water_like() {
                    ring_salt = Some(tile);
                }
            }
        }
        if closest_salt.is_none() {
            closest_salt = ring_salt;
        }
    }
    closest_salt
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
    let camera_tile = focus.points.iter().find(|p| p.is_camera).map(|p| {
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
                herd.cluster_id,
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
    cluster_id: u32,
    species: WildHerdSpecies,
    centre: (i32, i32),
    count: u32,
    bucket_slot: &mut u32,
) -> Vec<Entity> {
    use crate::simulation::animals::HerdMember;
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
            thirst: fastrand::f32() * 50.0,
            sickness: 0.0,
        };

        // Bevy bundles cap at 15 elements in a single tuple. Group the
        // species marker + WildHerdMember into a sub-tuple so we stay
        // under the limit.
        let entity = match species {
            WildHerdSpecies::Horse => commands
                .spawn((
                    (
                        Horse,
                        WildHerdMember { herd_id },
                        HerdMember { cluster_id },
                    ),
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
                    (
                        Cow,
                        WildHerdMember { herd_id },
                        HerdMember { cluster_id },
                    ),
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
                    crate::world::spatial::Indexed::new(crate::world::spatial::IndexedKind::Cow),
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
