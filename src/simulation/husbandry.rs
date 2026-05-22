//! Animal husbandry infrastructure (Phase 3).
//!
//! Provides Pen / Stable / FeedTrough / HitchingPost structure components,
//! their tile-indexed maps (mirror of `BedMap` / `GranaryMap` pattern), and
//! the runtime systems that match domestic animals to their preferred home
//! and refill feed troughs.
//!
//! Building blueprints live in `construction.rs` (`BuildSiteKind::{Pen,
//! Stable, FeedTrough, HitchingPost}`); this module owns the finalize-side
//! component shapes + the runtime queries that depend on them.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::simulation::animals::{DomesticAnimal, DomesticSpecies, Tamed};
use crate::simulation::faction::{FactionMember, FactionRegistry, SOLO};
use crate::simulation::schedule::SimClock;
use crate::world::seasons::TICKS_PER_DAY;
use crate::world::terrain::TILE_SIZE;

/// Species mask bitflags packed into `u8`. Pen accepts cattle/pig; Stable is
/// horse-only. Mirrors `DomesticSpecies` discriminants.
pub const SPECIES_HORSE: u8 = 1 << 0;
pub const SPECIES_CATTLE: u8 = 1 << 1;
pub const SPECIES_PIG: u8 = 1 << 2;
pub const SPECIES_DOG: u8 = 1 << 3;
pub const SPECIES_CAT: u8 = 1 << 4;

/// Convert a `DomesticSpecies` value to its bitmask position.
pub fn species_mask_bit(s: DomesticSpecies) -> u8 {
    match s {
        DomesticSpecies::Horse => SPECIES_HORSE,
        DomesticSpecies::Cattle => SPECIES_CATTLE,
        DomesticSpecies::Pig => SPECIES_PIG,
        DomesticSpecies::Dog => SPECIES_DOG,
        DomesticSpecies::Cat => SPECIES_CAT,
    }
}

/// Open-air pen housing cattle / pigs (and optionally dogs as guards).
/// `capacity` caps how many domestic animals can list this entity as
/// `preferred_home`.
#[derive(Component, Clone, Copy, Debug)]
pub struct Pen {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub capacity: u8,
    pub species_mask: u8,
}

/// Roofed stable for horses (and cattle as overflow). Capacity smaller than a
/// pen.
#[derive(Component, Clone, Copy, Debug)]
pub struct Stable {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub capacity: u8,
    pub species_mask: u8,
}

/// Feed trough placed near a Pen / Stable. Stores grain in grams; satisfies
/// adjacent housed animals' hunger when `feed_trough_consume_system` ticks.
#[derive(Component, Clone, Copy, Debug)]
pub struct FeedTrough {
    pub faction_id: u32,
    pub tile: (i32, i32),
    pub stock_g: u32,
    pub capacity_g: u32,
}

/// Hitching post — a parking + tethering spot for draft animals and plows.
/// (Vehicle parking moved to the `VehicleYard`; `parked_vehicle` survives as
/// a generic tether slot.)
#[derive(Component, Clone, Copy, Debug)]
pub struct HitchingPost {
    pub faction_id: u32,
    pub tile: (i32, i32),
    /// The vehicle entity currently parked here, if any.
    pub parked_vehicle: Option<Entity>,
    /// Worker that has reserved this post for a hitch/unhitch operation.
    pub reserved_by: Option<Entity>,
}

impl HitchingPost {
    pub fn new(faction_id: u32, tile: (i32, i32)) -> Self {
        HitchingPost {
            faction_id,
            tile,
            parked_vehicle: None,
            reserved_by: None,
        }
    }
}

/// Tile-indexed map of pens. Mirrors `BedMap` / `GranaryMap` pattern.
#[derive(Resource, Default)]
pub struct PenMap(pub AHashMap<(i32, i32), Entity>);

#[derive(Resource, Default)]
pub struct StableMap(pub AHashMap<(i32, i32), Entity>);

#[derive(Resource, Default)]
pub struct FeedTroughMap(pub AHashMap<(i32, i32), Entity>);

#[derive(Resource, Default)]
pub struct HitchingPostMap(pub AHashMap<(i32, i32), Entity>);

/// Bundle of husbandry maps for ergonomic system access.
#[derive(bevy::ecs::system::SystemParam)]
pub struct HusbandryMaps<'w> {
    pub pen_map: ResMut<'w, PenMap>,
    pub stable_map: ResMut<'w, StableMap>,
    pub feed_trough_map: ResMut<'w, FeedTroughMap>,
    pub hitching_post_map: ResMut<'w, HitchingPostMap>,
}

/// Assigns `DomesticAnimal.preferred_home` for any animal without one.
/// Runs every `TICKS_PER_DAY / 4` ticks (Sequential). For each unhoused
/// domestic animal, picks the nearest same-faction Pen or Stable with free
/// capacity and a matching species mask. Over-capacity housing leaves the
/// excess animals with `preferred_home = None` (still alive, just falling
/// back to home tile).
pub fn assign_preferred_home_system(
    clock: Res<SimClock>,
    pen_q: Query<(Entity, &Pen)>,
    stable_q: Query<(Entity, &Stable)>,
    members_q: Query<&FactionMember>,
    mut domestic_q: Query<(Entity, &Tamed, &mut DomesticAnimal, &Transform)>,
) {
    let cadence = (TICKS_PER_DAY as u64 / 4).max(60);
    if clock.tick % cadence != 0 {
        return;
    }
    // Count current housed-by-home before scheduling new assignments so we
    // can enforce capacity.
    let mut housed_count: AHashMap<Entity, u8> = AHashMap::new();
    for (_, _, da, _) in domestic_q.iter() {
        if let Some(home) = da.preferred_home {
            *housed_count.entry(home).or_insert(0) += 1;
        }
    }
    // Collect housing entries grouped by faction for fast lookup.
    // (Stables and Pens both implement an `AnimalHousing`-style facade — but
    // we don't need a trait; we inline both queries.)
    for (e, tamed, mut da, transform) in domestic_q.iter_mut() {
        if da.preferred_home.is_some() {
            continue;
        }
        let species_bit = species_mask_bit(da.species);
        let pos = (
            (transform.translation.x / TILE_SIZE).floor() as i32,
            (transform.translation.y / TILE_SIZE).floor() as i32,
        );
        // Resolve owner via Tamed; FactionMember on the animal is unusual but
        // try it as a fallback (existing taming path inserts Tamed only).
        let owner = tamed.owner_faction;
        if owner == SOLO {
            continue;
        }
        // Optional: keep FactionMember loop placeholder unused warnings down.
        let _ = members_q.get(e);
        // Find best housing — chebyshev distance.
        let mut best: Option<(Entity, i32)> = None;
        let mut consider =
            |home_e: Entity, home_owner: u32, home_tile: (i32, i32), cap: u8, mask: u8| {
                if home_owner != owner {
                    return;
                }
                if mask & species_bit == 0 {
                    return;
                }
                let used = housed_count.get(&home_e).copied().unwrap_or(0);
                if used >= cap {
                    return;
                }
                let d = (pos.0 - home_tile.0).abs().max((pos.1 - home_tile.1).abs());
                if best.map_or(true, |(_, bd)| d < bd) {
                    best = Some((home_e, d));
                }
            };
        for (pe, pen) in pen_q.iter() {
            consider(pe, pen.faction_id, pen.tile, pen.capacity, pen.species_mask);
        }
        for (se, st) in stable_q.iter() {
            consider(se, st.faction_id, st.tile, st.capacity, st.species_mask);
        }
        if let Some((home_e, _)) = best {
            da.preferred_home = Some(home_e);
            *housed_count.entry(home_e).or_insert(0) += 1;
        }
    }
}

/// Emits one housing `Blueprint` per faction per cadence when owned animals
/// outgrow their housing. The census is split by class: horses count against
/// Stable capacity (a Stable is emitted when short), cattle / pig / dog count
/// against Pen capacity (a Pen is emitted when short). Stable need wins ties —
/// a horse with no stable is unhouseable. Cats have no housing and are ignored.
/// Uses the existing chief-build pipeline by inserting a `Blueprint` directly
/// (skipping `BuildIntent` so no `pressure_to_intent` plumbing needs to change).
pub fn husbandry_intent_emitter_system(
    mut commands: Commands,
    clock: Res<SimClock>,
    faction_registry: Res<FactionRegistry>,
    pens: Query<&Pen>,
    stables: Query<&Stable>,
    domestic_q: Query<(&Tamed, &DomesticAnimal)>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    bp_q: Query<&crate::simulation::construction::Blueprint>,
    chunk_map: Res<crate::world::chunk::ChunkMap>,
) {
    // Run daily, faction-staggered.
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    // Per-faction census, split by housing class. Horses can only live in a
    // Stable; cattle / pig / dog live in a Pen. (Cattle may overflow into a
    // Stable in `assign_preferred_home_system`, but the emitter keeps the two
    // pools separate — a faction that owns horses still needs its own
    // Stable.) Cats have no housing structure, so they're ignored.
    let mut horses: AHashMap<u32, u32> = AHashMap::new();
    let mut pen_species: AHashMap<u32, u32> = AHashMap::new();
    for (tamed, da) in domestic_q.iter() {
        match da.species {
            DomesticSpecies::Horse => *horses.entry(tamed.owner_faction).or_insert(0) += 1,
            DomesticSpecies::Cattle | DomesticSpecies::Pig | DomesticSpecies::Dog => {
                *pen_species.entry(tamed.owner_faction).or_insert(0) += 1;
            }
            DomesticSpecies::Cat => {}
        }
    }
    let mut pen_cap: AHashMap<u32, u32> = AHashMap::new();
    let mut stable_cap: AHashMap<u32, u32> = AHashMap::new();
    for p in pens.iter() {
        *pen_cap.entry(p.faction_id).or_insert(0) += p.capacity as u32;
    }
    for s in stables.iter() {
        *stable_cap.entry(s.faction_id).or_insert(0) += s.capacity as u32;
    }
    let pending_bp = |fid: u32, want: crate::simulation::construction::BuildSiteKind| -> bool {
        bp_map.0.values().any(|&e| {
            bp_q.get(e)
                .map(|bp| bp.faction_id == fid && bp.kind == want)
                .unwrap_or(false)
        })
    };
    // Deterministic iteration over the union of factions owning any
    // housing-relevant animal.
    let mut fids: Vec<u32> = horses.keys().chain(pen_species.keys()).copied().collect();
    fids.sort_unstable();
    fids.dedup();
    for fid in fids {
        if fid == SOLO {
            continue;
        }
        use crate::simulation::construction::BuildSiteKind;
        let horse_n = horses.get(&fid).copied().unwrap_or(0);
        let pen_n = pen_species.get(&fid).copied().unwrap_or(0);
        let stable_n = stable_cap.get(&fid).copied().unwrap_or(0);
        let pen_c = pen_cap.get(&fid).copied().unwrap_or(0);
        // Stable takes precedence: a horse with no stable is unhouseable,
        // whereas pen animals at least tolerate over-capacity.
        let want = if horse_n > stable_n && !pending_bp(fid, BuildSiteKind::Stable) {
            BuildSiteKind::Stable
        } else if pen_n > pen_c && !pending_bp(fid, BuildSiteKind::Pen) {
            BuildSiteKind::Pen
        } else {
            continue;
        };
        let Some(faction) = faction_registry.factions.get(&fid) else {
            continue;
        };
        // Don't place pens for nomadic factions in v1 — they migrate.
        if faction.caps.home.is_mobile() {
            continue;
        }
        let home = faction.home_tile;
        // Find a tile within radius 8 of home that's passable grass/scrub.
        let mut placement: Option<(i32, i32)> = None;
        'outer: for r in 4i32..=10 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue;
                    }
                    let t = (home.0 + dx, home.1 + dy);
                    let Some(k) = chunk_map.tile_kind_at(t.0, t.1) else {
                        continue;
                    };
                    if matches!(
                        k,
                        crate::world::tile::TileKind::Grass | crate::world::tile::TileKind::Scrub
                    ) && bp_map.0.get(&t).is_none()
                    {
                        placement = Some(t);
                        break 'outer;
                    }
                }
            }
        }
        let Some(tile) = placement else {
            continue;
        };
        let z = chunk_map.surface_z_at(tile.0, tile.1) as i8;
        let bp = crate::simulation::construction::Blueprint::new(fid, None, want, tile, z);
        let world_pos = crate::world::terrain::tile_to_world(tile.0, tile.1);
        let e = commands
            .spawn((
                bp,
                Transform::from_xyz(world_pos.x, world_pos.y, 0.2),
                GlobalTransform::default(),
                Visibility::Visible,
                InheritedVisibility::default(),
            ))
            .id();
        // BlueprintMap update is normally done by the spawn path inside
        // `construction.rs`; this system uses the same insertion (Blueprint's
        // on_add hook handles index registration).
        let _ = e;
    }
}

/// Feed trough: refills sated animals adjacent within chebyshev 1. Runs
/// daily. v1 stub — drains 50g grain per adjacent housed animal.
pub fn feed_trough_consume_system(
    clock: Res<SimClock>,
    mut troughs: Query<(&mut FeedTrough, &Transform)>,
    mut domestic_q: Query<
        (
            &Tamed,
            &DomesticAnimal,
            &mut crate::simulation::animals::AnimalNeeds,
            &Transform,
        ),
        With<DomesticAnimal>,
    >,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    for (mut trough, t_tf) in troughs.iter_mut() {
        if trough.stock_g == 0 {
            continue;
        }
        let tx = (t_tf.translation.x / TILE_SIZE).floor() as i32;
        let ty = (t_tf.translation.y / TILE_SIZE).floor() as i32;
        for (tamed, _da, mut needs, a_tf) in domestic_q.iter_mut() {
            if tamed.owner_faction != trough.faction_id {
                continue;
            }
            let ax = (a_tf.translation.x / TILE_SIZE).floor() as i32;
            let ay = (a_tf.translation.y / TILE_SIZE).floor() as i32;
            if (ax - tx).abs() > 1 || (ay - ty).abs() > 1 {
                continue;
            }
            if needs.hunger < 30.0 {
                continue;
            }
            let take_g = 50u32.min(trough.stock_g);
            trough.stock_g -= take_g;
            needs.hunger = (needs.hunger - 30.0).max(0.0);
            if trough.stock_g == 0 {
                break;
            }
        }
    }
}

/// Hook for on_add: register a Pen in PenMap.
pub fn on_pen_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(pen) = world.get::<Pen>(entity).copied() else {
        return;
    };
    world.resource_mut::<PenMap>().0.insert(pen.tile, entity);
}

pub fn on_pen_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(pen) = world.get::<Pen>(entity).copied() else {
        return;
    };
    let mut map = world.resource_mut::<PenMap>();
    if map.0.get(&pen.tile).copied() == Some(entity) {
        map.0.remove(&pen.tile);
    }
}

pub fn on_stable_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(stable) = world.get::<Stable>(entity).copied() else {
        return;
    };
    world
        .resource_mut::<StableMap>()
        .0
        .insert(stable.tile, entity);
}

pub fn on_stable_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(stable) = world.get::<Stable>(entity).copied() else {
        return;
    };
    let mut map = world.resource_mut::<StableMap>();
    if map.0.get(&stable.tile).copied() == Some(entity) {
        map.0.remove(&stable.tile);
    }
}

pub fn on_feed_trough_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(t) = world.get::<FeedTrough>(entity).copied() else {
        return;
    };
    world
        .resource_mut::<FeedTroughMap>()
        .0
        .insert(t.tile, entity);
}

pub fn on_feed_trough_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(t) = world.get::<FeedTrough>(entity).copied() else {
        return;
    };
    let mut map = world.resource_mut::<FeedTroughMap>();
    if map.0.get(&t.tile).copied() == Some(entity) {
        map.0.remove(&t.tile);
    }
}

pub fn on_hitching_post_add(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(p) = world.get::<HitchingPost>(entity).copied() else {
        return;
    };
    world
        .resource_mut::<HitchingPostMap>()
        .0
        .insert(p.tile, entity);
}

pub fn on_hitching_post_remove(
    mut world: bevy::ecs::world::DeferredWorld<'_>,
    entity: Entity,
    _: bevy::ecs::component::ComponentId,
) {
    let Some(p) = world.get::<HitchingPost>(entity).copied() else {
        return;
    };
    let mut map = world.resource_mut::<HitchingPostMap>();
    if map.0.get(&p.tile).copied() == Some(entity) {
        map.0.remove(&p.tile);
    }
}
