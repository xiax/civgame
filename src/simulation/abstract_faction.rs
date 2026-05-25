//! Abstract world-map factions (Phase 2 of `plans/world-map-abstract-factions.md`).
//!
//! Only the player + a few near rivals spawn as full entity groups (see
//! `person::spawn_population`). Every other faction slot is seeded here as an
//! **abstract faction**: a `faction_id` stamped on a Globe `WorldCell`, with
//! population/food ticked abstractly by `world_sim_system`. Abstract factions
//! carry no `Person` / `Settlement` entities — `FactionData.materialized` is
//! `false` — until the player travels near their home region and a later phase
//! materialises them.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::simulation::faction::FactionRegistry;
use crate::simulation::person::{GROUP_SIZE, INITIAL_POPULATION, NEARBY_RIVAL_COUNT};
use crate::simulation::region::MegaChunkCoord;
use crate::world::chunk::CHUNK_SIZE;
use crate::world::globe::{Globe, GLOBE_CELL_CHUNKS, GLOBE_HEIGHT, GLOBE_WIDTH};
use crate::world::terrain::WORLD_CHUNKS_X;

/// A faction that exists only as world-map data: a `faction_id` on one Globe
/// climate-cell. It has no entities until materialised.
#[derive(Clone, Copy, Debug)]
pub struct AbstractFaction {
    pub faction_id: u32,
    /// Home tile (world-tile coords) — the future settlement anchor.
    pub home_tile: (i32, i32),
    /// Globe climate-cell coords carrying this faction's `faction_id`.
    pub home_cell: (i32, i32),
    /// Mega-chunk containing `home_tile` — the materialisation trigger unit.
    pub home_megachunk: (i32, i32),
}

/// Queue populated by `materialize_abstract_faction_system` and drained the
/// following tick by `materialize_seed_relationships_system`. Bevy applies
/// `commands` between systems, so the freshly-spawned members are queryable
/// only after this hop. Without it, every materialised faction would skip the
/// kin-household + reciprocal-affinity seeding the OnEnter path runs (see
/// `settlement_bootstrap::seed_starting_relationships_system`).
#[derive(Resource, Default)]
pub struct PendingMaterializationKinSeed {
    pub entries: Vec<(u32, (i32, i32), Vec<Entity>)>,
}

/// Registry of every abstract (un-materialised) faction. Drained entry-by-entry
/// by the materialisation system as the player approaches each home region.
#[derive(Resource, Default)]
pub struct AbstractFactions {
    pub by_id: AHashMap<u32, AbstractFaction>,
    /// `home_megachunk → faction_id` reverse index for the approach trigger.
    pub by_megachunk: AHashMap<(i32, i32), u32>,
}

/// Number of abstract factions to seed = total faction slots minus the near
/// factions `spawn_population` already spawned (player + `NEARBY_RIVAL_COUNT`).
pub(crate) fn abstract_faction_count() -> u32 {
    let num_groups = INITIAL_POPULATION / GROUP_SIZE;
    let near = (NEARBY_RIVAL_COUNT + 1).min(num_groups);
    num_groups.saturating_sub(near)
}

/// Chebyshev keep-out radius (in Globe cells) around the player's pre-gen
/// window — abstract factions seed well clear of the playable region. The
/// window is `WORLD_CHUNKS_X` chunks wide = `WORLD_CHUNKS_X / GLOBE_CELL_CHUNKS`
/// cells; half that, plus a margin.
fn player_keepout_radius_cells() -> i32 {
    (WORLD_CHUNKS_X / GLOBE_CELL_CHUNKS) / 2 + 4
}

/// Farthest-point score for a candidate Globe cell: the minimum Chebyshev
/// cell-distance to the player window centre and to every already-placed
/// abstract home. Larger = better spread. Mirrors `person::faction_spacing_score`
/// at Globe-cell granularity.
fn cell_spacing_score(gx: i32, gy: i32, player_center: (i32, i32), placed: &[(i32, i32)]) -> i32 {
    let cheb = |a: (i32, i32), b: (i32, i32)| (a.0 - b.0).abs().max((a.1 - b.1).abs());
    let mut best = cheb((gx, gy), player_center);
    for &p in placed {
        best = best.min(cheb((gx, gy), p));
    }
    best
}

/// Centre world-tile of a Globe climate-cell. A cell spans `GLOBE_CELL_CHUNKS`
/// chunks per axis; the centre tile is half a cell in.
fn cell_center_tile(gx: i32, gy: i32) -> (i32, i32) {
    let cell_tiles = GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
    (
        gx * cell_tiles + cell_tiles / 2,
        gy * cell_tiles + cell_tiles / 2,
    )
}

/// `OnEnter(Playing)`, after `spawn_population`: seed the remaining faction
/// slots as abstract world-map factions spread across the Globe.
pub fn seed_abstract_factions_system(
    mut globe: ResMut<Globe>,
    mut registry: ResMut<FactionRegistry>,
    pending: Res<crate::PendingSpawn>,
    world_seed: Res<crate::WorldSeed>,
    options: Res<crate::GameStartOptions>,
    catalog: Res<crate::economy::resource_catalog::ResourceCatalog>,
    archetype_registry: Res<crate::simulation::archetype::FactionArchetypeRegistry>,
    mut abstract_factions: ResMut<AbstractFactions>,
) {
    let count = abstract_faction_count();
    if count == 0 {
        return;
    }

    // Player pre-gen window centre, in Globe-cell coords (mirrors the centre
    // `spawn_population` computes for the near-faction window).
    use crate::world::globe::MEGACHUNK_SIZE_CHUNKS;
    let (center_cx, center_cy) = match pending.0 {
        Some((mx, my)) => (
            mx * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2,
            my * MEGACHUNK_SIZE_CHUNKS + MEGACHUNK_SIZE_CHUNKS / 2,
        ),
        None => (
            (GLOBE_WIDTH / 2) * GLOBE_CELL_CHUNKS,
            (GLOBE_HEIGHT / 2) * GLOBE_CELL_CHUNKS,
        ),
    };
    let player_center = (
        center_cx.div_euclid(GLOBE_CELL_CHUNKS),
        center_cy.div_euclid(GLOBE_CELL_CHUNKS),
    );
    let keepout = player_keepout_radius_cells();

    let mut rng = fastrand::Rng::with_seed(world_seed.0 ^ 0xAB57_FAC7);
    let mut placed: Vec<(i32, i32)> = Vec::new();
    let mut seeded = 0u32;

    for _ in 0..count {
        // Best-of-N farthest-point pick over habitable Globe cells outside
        // the player keep-out, deterministic via the world seed.
        let mut best: Option<((i32, i32), i32)> = None;
        for _ in 0..512 {
            let gx = rng.i32(0..GLOBE_WIDTH);
            let gy = rng.i32(0..GLOBE_HEIGHT);
            // Inside the player's playable region — reserved for near factions.
            let cheb_to_player =
                (gx - player_center.0).abs().max((gy - player_center.1).abs());
            if cheb_to_player <= keepout {
                continue;
            }
            match globe.cell(gx, gy) {
                Some(c) if c.biome.is_habitable() && c.faction_id == 0 => {}
                _ => continue,
            }
            let score = cell_spacing_score(gx, gy, player_center, &placed);
            if best.as_ref().map_or(true, |(_, s)| score > *s) {
                best = Some(((gx, gy), score));
            }
        }
        let Some(((gx, gy), _)) = best else {
            // No habitable cell sampled this round — skip; the world simply
            // has one fewer abstract faction.
            continue;
        };

        let home_tile = cell_center_tile(gx, gy);
        let faction_id = registry.create_faction(home_tile);

        if let Some(fd) = registry.factions.get_mut(&faction_id) {
            // Settled archetype + the game's economy preset — identical to the
            // AI near factions, so a materialised abstract faction behaves
            // exactly like a faction that spawned near the player.
            crate::economy::policy::apply_preset(
                &mut fd.economic_policy,
                options.economy,
                &catalog,
            );
            fd.land_policy = crate::economy::policy::land_policy_for(options.economy);
            let key = crate::simulation::archetype::legacy_archetype_key(
                fd.lifestyle,
                options.economy,
            );
            fd.caps = crate::simulation::archetype::derive_from_archetype_key(
                &archetype_registry,
                key,
                Some((fd.lifestyle, options.economy, &catalog)),
            )
            .expect("derive_from_archetype_key with legacy fallback always returns Some");
            // The defining flag: no entities until the player travels near.
            fd.materialized = false;
        }

        // Stamp the Globe cell so `world_sim_system` ticks this faction's
        // population / food / raids abstractly while it is off-screen.
        if let Some(cell) = globe.cell_mut(gx, gy) {
            cell.faction_id = faction_id;
            cell.population = GROUP_SIZE as u16;
            cell.food_stock = GROUP_SIZE as f32 * 10.0;
        }

        let home_megachunk = MegaChunkCoord::from_tile(home_tile.0, home_tile.1);
        abstract_factions.by_id.insert(
            faction_id,
            AbstractFaction {
                faction_id,
                home_tile,
                home_cell: (gx, gy),
                home_megachunk,
            },
        );
        abstract_factions
            .by_megachunk
            .insert(home_megachunk, faction_id);
        placed.push((gx, gy));
        seeded += 1;
    }

    info!(
        "Seeded {seeded} abstract world-map factions ({} requested)",
        count
    );
}

/// Cap on members spawned when an abstract faction materialises. The abstract
/// `population` is the whole off-map civilization (`world_sim` lets it grow to
/// 1000); only a settlement's worth of it becomes live entities.
const MAX_MATERIALIZED_MEMBERS: u32 = 40;

/// Chunk containing a world tile.
fn tile_chunk(tile: (i32, i32)) -> (i32, i32) {
    (
        tile.0.div_euclid(CHUNK_SIZE as i32),
        tile.1.div_euclid(CHUNK_SIZE as i32),
    )
}

/// Spiral outward from `origin` for the nearest passable, non-stone tile
/// (chebyshev rings, capped at `max_radius`). The abstract home tile is a
/// climate-cell centre and may land on water/stone; the band must spawn on
/// real ground.
fn nearest_passable_tile(
    chunk_map: &crate::world::chunk::ChunkMap,
    origin: (i32, i32),
    max_radius: i32,
) -> Option<(i32, i32)> {
    use crate::world::tile::TileKind;
    let ok = |tx: i32, ty: i32| {
        chunk_map.is_passable(tx, ty)
            && !matches!(chunk_map.tile_kind_at(tx, ty), Some(TileKind::Stone))
    };
    if ok(origin.0, origin.1) {
        return Some(origin);
    }
    for r in 1..=max_radius {
        for dx in -r..=r {
            for dy in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // ring perimeter only
                }
                let (tx, ty) = (origin.0 + dx, origin.1 + dy);
                if ok(tx, ty) {
                    return Some((tx, ty));
                }
            }
        }
    }
    None
}

/// FixedUpdate, after `chunk_streaming_system`: when chunk data for an abstract
/// faction's home tile streams in (the player has travelled near), spawn the
/// faction's band as full entities and drop it from `AbstractFactions`. The
/// shared `person::spawn_faction_band` makes it identical to a faction that
/// spawned beside the player; `auto_found_default_settlements_system` then
/// founds its settlement on the next Economy tick (`materialized` is now true).
pub fn materialize_abstract_faction_system(
    mut commands: Commands,
    mut chunk_loaded: EventReader<crate::world::chunk_streaming::ChunkLoadedEvent>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    mut registry: ResMut<FactionRegistry>,
    mut clock: ResMut<crate::simulation::schedule::SimClock>,
    mut globe: ResMut<Globe>,
    options: Res<crate::GameStartOptions>,
    mut abstract_factions: ResMut<AbstractFactions>,
    mut pending_kin: ResMut<PendingMaterializationKinSeed>,
) {
    // Match loaded chunks against abstract faction home tiles.
    let mut to_materialize: Vec<AbstractFaction> = Vec::new();
    for ev in chunk_loaded.read() {
        let chunk = (ev.coord.0, ev.coord.1);
        if let Some(af) = abstract_factions
            .by_id
            .values()
            .find(|af| tile_chunk(af.home_tile) == chunk)
        {
            if !to_materialize.iter().any(|m| m.faction_id == af.faction_id) {
                to_materialize.push(*af);
            }
        }
    }

    for af in to_materialize {
        // The cell-centre home tile may be water/stone — find real ground.
        let Some(anchor) = nearest_passable_tile(&chunk_map, af.home_tile, 48) else {
            warn!(
                "materialize: no passable tile near abstract faction {} home {:?}; staying abstract",
                af.faction_id, af.home_tile
            );
            continue;
        };

        // Member count = abstract population, capped (the abstract pop is the
        // whole civilization; a band's worth materialises).
        let population = globe
            .cell(af.home_cell.0, af.home_cell.1)
            .map(|c| c.population as u32)
            .unwrap_or(GROUP_SIZE);
        let member_count = population.clamp(1, MAX_MATERIALIZED_MEMBERS);

        // Re-anchor the faction home onto real ground so the settlement
        // founds at a passable tile.
        if let Some(fd) = registry.factions.get_mut(&af.faction_id) {
            fd.home_tile = anchor;
        }

        let band = crate::simulation::person::spawn_faction_band(
            &mut commands,
            &chunk_map,
            &mut registry,
            &mut clock,
            af.faction_id,
            anchor,
            member_count,
            options.era,
        );

        if let Some(fd) = registry.factions.get_mut(&af.faction_id) {
            fd.materialized = true;
        }

        // Defer kin-household + spouse-affinity seeding to next tick — the
        // members were just spawned via `commands` and are not yet queryable
        // here. `materialize_seed_relationships_system` drains this queue
        // after Bevy applies the deferred spawn commands.
        pending_kin
            .entries
            .push((af.faction_id, anchor, band.members.clone()));

        // Hand the cell back: clear its faction fields so `world_sim_system`
        // stops ticking it — the live entities now own this faction. Phase 4
        // (dematerialisation) re-stamps the cell when the player leaves.
        if let Some(cell) = globe.cell_mut(af.home_cell.0, af.home_cell.1) {
            cell.faction_id = 0;
            cell.population = 0;
            cell.food_stock = 0.0;
        }

        abstract_factions.by_id.remove(&af.faction_id);
        abstract_factions.by_megachunk.remove(&af.home_megachunk);

        info!(
            "Materialised abstract faction {} at {:?} with {} members",
            af.faction_id,
            anchor,
            band.members.len()
        );
    }
}

/// Drain `PendingMaterializationKinSeed` and run the same kin-partition logic
/// `seed_starting_relationships_system` runs at game start, scoped to one
/// freshly-materialised faction's members. Without this, the materialised
/// faction would never form households or seed reciprocal spouse/sibling
/// affinities — the same conception-starvation bug seeded factions had before
/// `seed_starting_relationships_system` existed, just on a different timeline.
pub fn materialize_seed_relationships_system(
    mut commands: Commands,
    mut pending: ResMut<PendingMaterializationKinSeed>,
    mut registry: ResMut<FactionRegistry>,
    catalog: Res<crate::economy::resource_catalog::ResourceCatalog>,
    sex_q: Query<&crate::simulation::reproduction::BiologicalSex>,
    mut relationships: Query<&mut crate::simulation::memory::RelationshipMemory>,
) {
    use crate::simulation::settlement_bootstrap::{seed_kin_partition, MAX_KIN_GROUP};

    if pending.entries.is_empty() {
        return;
    }
    let entries: Vec<_> = pending.entries.drain(..).collect();
    for (faction_id, home_tile, mut members) in entries {
        // Mirror `seed_starting_relationships_system` gates exactly.
        let Some(faction) = registry.factions.get(&faction_id) else {
            continue;
        };
        if faction.caps.inheritance.seed_storage_tile {
            continue;
        }
        if faction.caps.home.is_mobile() {
            continue;
        }
        // Deterministic ordering (matches the OnEnter seed pass).
        members.sort_unstable_by_key(|e| e.to_bits());
        for group in members.chunks(MAX_KIN_GROUP) {
            seed_kin_partition(
                &mut commands,
                &mut registry,
                &catalog,
                &sex_q,
                &mut relationships,
                faction_id,
                home_tile,
                group,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abstract_count_is_total_minus_near() {
        // 200 / 20 = 10 slots; player + 3 rivals near → 6 abstract.
        assert_eq!(abstract_faction_count(), 6);
    }

    #[test]
    fn cell_spacing_score_empty_is_player_distance() {
        // No placed homes: the score is purely distance from the player.
        assert_eq!(cell_spacing_score(100, 100, (0, 0), &[]), 100);
    }

    #[test]
    fn cell_spacing_score_takes_nearest_anchor() {
        // Player at (0,0) is 200 away; a placed home at (60,0) is 40 away —
        // the nearest anchor (40) wins.
        let s = cell_spacing_score(100, 0, (0, 0), &[(60, 0), (500, 500)]);
        assert_eq!(s, 40);
    }

    #[test]
    fn cell_center_tile_is_half_a_cell_in() {
        let cell_tiles = GLOBE_CELL_CHUNKS * CHUNK_SIZE as i32;
        assert_eq!(
            cell_center_tile(3, 5),
            (3 * cell_tiles + cell_tiles / 2, 5 * cell_tiles + cell_tiles / 2)
        );
    }
}
