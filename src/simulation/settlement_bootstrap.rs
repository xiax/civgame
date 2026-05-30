//! Bootstrap-time settlement scaffolding (P2 onward of the bootstrap plan).
//!
//! Today this module owns one piece: `seed_starting_relationships_system`,
//! which forms communal households + reciprocal kin affinities deterministically
//! at `OnEnter(Playing)` for Subsistence/Mixed factions. Without this, those
//! factions waste a game-week of `CoSleepTracker` accumulation before any
//! `HouseholdMember` exists, and every founder starts with zero
//! `RelationshipMemory` entries (no spouse, no siblings) — every social bond
//! has to be forged from scratch.
//!
//! Market-preset factions keep the existing one-person-per-adult path
//! (`person::seed_market_households`), gated by
//! `caps.inheritance.seed_storage_tile`. Nomadic factions are also skipped —
//! their household model is undecided in P2 scope. The plan's P4 atomic
//! pipeline will fold every household type into a single
//! `plan.households` list; for P2 the two branches coexist.
//!
//! ## Kin partition
//!
//! Adults of a communal faction are sorted by entity id and chunked into
//! kin groups of size ≤4:
//!
//! - 2 adults → spouse pair (one `HouseholdMember`).
//! - 3 adults → spouse pair + 1 sibling of the head.
//! - 4 adults → spouse pair + 2 siblings (one sibling of each spouse).
//! - 5–7 adults → one group of 4 + a remainder group sized 1–3.
//! - 1 adult (bachelor) → solo household; no relationships seeded.
//!
//! ## Affinity seeds (`memory::RelationshipMemory`)
//!
//! - Spouse pair: bidirectional +79 (above `PARTNER_AFFINITY_THRESHOLD = 60`
//!   so the homeless-pass treats them as paired and clusters their beds,
//!   but **strictly below** `REASSIGN_AFFINITY_THRESHOLD = 80` so the
//!   already-housed-woman migration pass doesn't fire on tick 1 and queue a
//!   bed adjacent to the husband — every founder already has a seeded bed,
//!   and a forced re-housing would emit a `Single(Bed)` blueprint outside
//!   any Hut/Longhouse footprint).
//! - Sibling: bidirectional +60 to its primary partner-in-household
//!   (= `PARTNER_AFFINITY_THRESHOLD`, just enough to read as "kin" without
//!   crossing into spouse-style cohabitation).

use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceCatalog;
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::memory::RelationshipMemory;
use crate::simulation::person::Person;
use crate::simulation::reproduction::{BiologicalSex, HouseholdMember};

/// Reciprocal affinity seeded for a spouse pair. Above
/// `PARTNER_AFFINITY_THRESHOLD = 60` (the homeless-pass partner-bed
/// pairing read) but **strictly below** `REASSIGN_AFFINITY_THRESHOLD = 80`
/// (the already-housed-woman migration trigger). 79 is the highest value
/// that gets us spouse-grade pairing without churning seeded beds.
pub(crate) const SPOUSE_AFFINITY: i8 = 79;

/// Reciprocal affinity seeded between a sibling and its kin partner. Meets
/// `PARTNER_AFFINITY_THRESHOLD = 60` (kin reads as familiar) but stays below
/// `REASSIGN = 80` so the runtime doesn't treat siblings as spouses.
const SIBLING_AFFINITY: i8 = 60;

/// Max kin-group size. Larger founder pools chunk into multiple groups.
pub(crate) const MAX_KIN_GROUP: usize = 4;

/// P2 of the bootstrap plan: form deterministic kin households + seed
/// reciprocal `RelationshipMemory` affinities for every Subsistence/Mixed
/// founder. Market factions are handled by `person::seed_market_households`
/// at spawn time and skipped here; SOLO and nomadic factions are also
/// skipped. Runs `OnEnter(Playing)` after `seed_starting_buildings_system`
/// so the bed pool exists when the runtime's bed-assignment loop later
/// reads the seeded affinities.
pub fn seed_starting_relationships_system(
    mut commands: Commands,
    mut registry: ResMut<FactionRegistry>,
    catalog: Res<ResourceCatalog>,
    members_q: Query<(Entity, &FactionMember, Option<&HouseholdMember>), With<Person>>,
    sex_q: Query<&BiologicalSex>,
    mut relationships: Query<&mut RelationshipMemory>,
) {
    // Bucket members by their root faction id (skip households that may
    // have already been formed by other paths — Market preset).
    use crate::collections::AHashMap;
    let mut by_faction: AHashMap<u32, Vec<Entity>> = AHashMap::default();
    for (entity, member, household) in members_q.iter() {
        if household.is_some() {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        by_faction
            .entry(member.faction_id)
            .or_default()
            .push(entity);
    }

    for (faction_id, mut members) in by_faction.into_iter() {
        // Gate: Subsistence/Mixed only. Market (`seed_storage_tile`) already
        // formed one-person households at spawn. Nomadic factions skipped —
        // their household model is P4 scope.
        let Some(faction) = registry.factions.get(&faction_id) else {
            continue;
        };
        if faction.caps.inheritance.seed_storage_tile {
            continue;
        }
        if faction.caps.home.is_mobile() {
            continue;
        }
        let home_tile = faction.home_tile;

        // Deterministic ordering: stable on entity bits across re-spawns
        // with the same seed (Bevy entity allocation is deterministic
        // within a single OnEnter pass).
        members.sort_unstable_by_key(|e| e.to_bits());

        // Chunk into kin groups of up to MAX_KIN_GROUP. Smaller remainder
        // groups (1-3 trailing adults) form their own household.
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

/// Form one kin household from `group` and seed its internal affinities.
/// Shared by the OnEnter seed pass and runtime materialisation of abstract
/// factions (`abstract_faction::materialize_abstract_faction_system`).
pub(crate) fn seed_kin_partition(
    commands: &mut Commands,
    registry: &mut FactionRegistry,
    catalog: &ResourceCatalog,
    sex_q: &Query<&BiologicalSex>,
    relationships: &mut Query<&mut RelationshipMemory>,
    parent_faction_id: u32,
    home_tile: (i32, i32),
    group: &[Entity],
) {
    seed_kin_group(
        commands,
        registry,
        catalog,
        sex_q,
        relationships,
        parent_faction_id,
        home_tile,
        group,
    );
}

/// Form one kin household from `group` and seed its internal affinities.
/// `group` is non-empty (caller guarantees via `chunks(_)`).
fn seed_kin_group(
    commands: &mut Commands,
    registry: &mut FactionRegistry,
    catalog: &ResourceCatalog,
    sex_q: &Query<&BiologicalSex>,
    relationships: &mut Query<&mut RelationshipMemory>,
    parent_faction_id: u32,
    home_tile: (i32, i32),
    group: &[Entity],
) {
    if group.is_empty() {
        return;
    }
    let head = group[0];
    let household_id = registry.spawn_household(parent_faction_id, home_tile, head, catalog);
    if let Some(hh) = registry.factions.get_mut(&household_id) {
        hh.member_count = group.len() as u32;
    }
    for &member in group {
        commands
            .entity(member)
            .insert(HouseholdMember { household_id });
    }

    // Bachelor household — nothing to seed.
    if group.len() < 2 {
        return;
    }

    // Spouse pair: head + group[1]. Defensive against future drift: only set
    // SPOUSE_AFFINITY when the pair is opposite-sex; same-sex fallback to
    // SIBLING_AFFINITY so the bed-pairing pass doesn't try to cohabit a
    // same-sex pair. With `person::pair_chief_sex` + roster wiring this
    // should always be opposite-sex for kin slot 0/1, but the gate keeps
    // the contract honest if a fixture/sandbox skips the roster.
    let spouse_a = group[0];
    let spouse_b = group[1];
    let opposite_sex = matches!(
        (sex_q.get(spouse_a).ok().copied(), sex_q.get(spouse_b).ok().copied()),
        (Some(a), Some(b)) if a != b
    );
    if opposite_sex {
        set_reciprocal_affinity(relationships, spouse_a, spouse_b, SPOUSE_AFFINITY);
    } else {
        set_reciprocal_affinity(relationships, spouse_a, spouse_b, SIBLING_AFFINITY);
    }

    // Siblings: each remaining member binds to one of the spouses (round-
    // robin) at SIBLING_AFFINITY. A 4-adult group thus has spouse + sibling-
    // of-A + sibling-of-B; a 3-adult group has spouse + sibling-of-A.
    for (idx, &sibling) in group.iter().enumerate().skip(2) {
        let kin_partner = if idx % 2 == 0 { spouse_a } else { spouse_b };
        set_reciprocal_affinity(relationships, sibling, kin_partner, SIBLING_AFFINITY);
    }
}

/// Write `delta` as the **set** affinity for the pair (not an additive
/// bump). `RelationshipMemory::update` is additive — we'd need to clamp
/// after — so we drop into the entry list directly. New entries fall into
/// the first empty slot; existing entries are overwritten to keep the
/// signal deterministic across re-runs.
fn set_reciprocal_affinity(
    relationships: &mut Query<&mut RelationshipMemory>,
    a: Entity,
    b: Entity,
    affinity: i8,
) {
    set_affinity_for(relationships, a, b, affinity);
    set_affinity_for(relationships, b, a, affinity);
}

/// Bootstrap P4: after the seed pass stamps walls/beds/doors/roads/yards
/// and `SeedReservation` is populated, walk every Person and relocate any
/// agent whose tile collides with a stamped structure (`StructureIndex`),
/// a planned road / doormat / ag plot (`SeedReservation`), or a wall tile
/// directly (`WallMap` — palisade tiles aren't in `StructureIndex` until
/// the structure-label hook fires). Reaches for the nearest non-conflicting
/// reachable-from-home tile via `placement_reachability::spawn_tiles_from`.
///
/// This is the **observable** P4 invariant from `plans/settlement-bootstrap.md`
/// ("members never land on a tile a stamp later writes over"). The full
/// `SettlementBootstrapPlan` struct + validator + spawn reorder are deferred
/// — the chief-tech-priming flow (`sync_faction_techs_from_chief_system →
/// derive_tech_adoption_system → refresh_construction_poster_pool_system`)
/// reads chief `PersonKnowledge` and would require either a synthetic
/// `FounderSpec` or a chief-only pre-spawn pass to refactor cleanly. The
/// stranded-relocation pass achieves the same end state with a single
/// system and no chain reorder.
pub fn relocate_stranded_members_system(
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    structure_index: Res<crate::simulation::construction::StructureIndex>,
    wall_map: Res<crate::simulation::construction::WallMap>,
    bed_map: Res<crate::simulation::construction::BedMap>,
    doormat: Res<crate::simulation::doormat::DoormatReservations>,
    blueprint_map: Res<crate::simulation::construction::BlueprintMap>,
    seed_reservation: Res<crate::simulation::seed_reservation::SeedReservation>,
    plot_index: Res<crate::simulation::land::PlotIndex>,
    registry: Res<FactionRegistry>,
    mut members_q: Query<
        (
            &FactionMember,
            &mut Transform,
            &mut crate::simulation::person::PersonAI,
        ),
        With<Person>,
    >,
) {
    use crate::world::terrain::{tile_to_world, TILE_SIZE};
    use crate::world::tile::TileKind;
    use crate::collections::AHashSet;

    // Cache reachable-from-home pools per faction so we don't re-BFS for
    // each stranded member.
    let mut pools_by_faction: crate::collections::AHashMap<u32, Vec<(i32, i32)>> = crate::collections::AHashMap::default();
    let mut used: AHashSet<(i32, i32)> = AHashSet::default();

    // Seed `used` with every tile any in-place Person already occupies so
    // we don't relocate two strays to the same destination.
    for (_, transform, _) in members_q.iter() {
        let tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        used.insert((tx, ty));
    }

    let is_conflict = |tile: (i32, i32)| -> bool {
        if structure_index.0.contains_key(&tile) {
            return true;
        }
        if wall_map.0.contains_key(&tile) {
            return true;
        }
        if bed_map.0.contains_key(&tile) {
            return true;
        }
        if doormat.is_reserved(tile) {
            return true;
        }
        if blueprint_map.0.contains_key(&tile) {
            return true;
        }
        if seed_reservation.is_reserved(tile) {
            return true;
        }
        // PlotIndex.ag_tiles is folded into SeedReservation but stays a
        // direct check too for the runtime survey path (ag plots carved
        // after OnEnter wouldn't land in SeedReservation).
        if plot_index.ag_tiles.contains(&tile) {
            return true;
        }
        if let Some(kind) = chunk_map.tile_kind_at(tile.0, tile.1) {
            if !kind.is_passable() || matches!(kind, TileKind::Wall | TileKind::Road) {
                return true;
            }
        } else {
            return true;
        }
        false
    };

    for (member, mut transform, mut ai) in members_q.iter_mut() {
        let tile_x = (transform.translation.x / TILE_SIZE).floor() as i32;
        let tile_y = (transform.translation.y / TILE_SIZE).floor() as i32;
        let tile = (tile_x, tile_y);
        if !is_conflict(tile) {
            continue;
        }
        let Some(faction) = registry.factions.get(&member.faction_id) else {
            continue;
        };
        // SOLO agents have no faction home; skip the relocation (no plan
        // touches their footprint anyway).
        if member.faction_id == crate::simulation::faction::SOLO {
            continue;
        }
        let home = faction.home_tile;
        let pool = pools_by_faction
            .entry(member.faction_id)
            .or_insert_with(|| {
                crate::simulation::placement_reachability::spawn_tiles_from(&chunk_map, home, 256)
            });
        let Some(&new_tile) = pool
            .iter()
            .find(|&&t| !is_conflict(t) && !used.contains(&t))
        else {
            continue; // no safe tile; leave the member where they are (defensive)
        };
        used.remove(&tile);
        used.insert(new_tile);
        let world = tile_to_world(new_tile.0, new_tile.1);
        transform.translation.x = world.x;
        transform.translation.y = world.y;
        let new_z = chunk_map.surface_z_at(new_tile.0, new_tile.1) as i8;
        ai.target_tile = new_tile;
        ai.dest_tile = new_tile;
        ai.current_z = new_z;
        ai.target_z = new_z;
    }
}

fn set_affinity_for(
    relationships: &mut Query<&mut RelationshipMemory>,
    owner: Entity,
    target: Entity,
    affinity: i8,
) {
    let Ok(mut rel) = relationships.get_mut(owner) else {
        return;
    };
    // Overwrite if already present.
    for slot in rel.entries.iter_mut() {
        if let Some(entry) = slot {
            if entry.entity == target {
                entry.affinity = affinity;
                entry.age = 0;
                return;
            }
        }
    }
    // Else find the first empty slot.
    for slot in rel.entries.iter_mut() {
        if slot.is_none() {
            *slot = Some(crate::simulation::memory::RelEntry {
                entity: target,
                affinity,
                age: 0,
            });
            return;
        }
    }
    // Ring full (16 entries) — overwrite lowest |affinity|. Founder seeding
    // shouldn't hit this, but keep symmetry with `RelationshipMemory::update`.
    let mut min_idx = 0usize;
    let mut min_abs = u8::MAX;
    for (i, slot) in rel.entries.iter().enumerate() {
        if let Some(e) = slot {
            let abs = e.affinity.unsigned_abs();
            if abs < min_abs {
                min_abs = abs;
                min_idx = i;
            }
        }
    }
    rel.entries[min_idx] = Some(crate::simulation::memory::RelEntry {
        entity: target,
        affinity,
        age: 0,
    });
}
