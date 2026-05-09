//! Settlement lifecycle events (P3 of the capabilities/storage-parity refactor).
//!
//! Today three systems mutate world state directly to handle
//! settlement establishment, migration, and sedentarization:
//! `auto_found_default_settlements_system` (Establish settled),
//! `auto_found_default_camps_system` (Establish nomadic),
//! `nomad_migration_commit_system` (Migrate), `nomad_sedentarize_system`
//! (SwitchArchetype). The first two stay direct emitters in P3 — they're
//! already simple "spawn-if-missing" loops. This module collects the
//! richer two cases (Migrate / SwitchArchetype) into typed events
//! handled by `process_settlement_lifecycle_system`.
//!
//! P3 minimal scope: the `SwitchArchetype` handler ships the full
//! 7-step re-derivation (caps, land_policy, economic_policy, camp
//! structure cleanup, `culture_hash` bump, synchronous storage tile
//! bootstrap, activity log emit) — closing the policy carryover bug
//! that trace 2 caught. `Migrate` / `Establish` / `Abandon` get type
//! definitions for future migration but the processor leaves their
//! handling to existing systems for now.

use bevy::ecs::system::SystemState;
use bevy::prelude::*;

use crate::simulation::archetype::FactionCapabilities;
use crate::simulation::faction::FactionRegistry;

/// Event types for archetype lifecycle changes.
#[derive(Clone, Debug)]
pub enum SettlementLifecycleEvent {
    /// A new economic node should exist for this faction at `tile`.
    /// P3 scope: type only — `auto_found_default_settlements_system`
    /// and `auto_found_default_camps_system` continue to spawn nodes
    /// directly. Future phases route them through this event.
    Establish {
        faction: u32,
        tile: (i32, i32),
        archetype_key: String,
    },
    /// Tear down a node (camp/settlement). Refund-drop semantics
    /// follow `Deployable::compute_refund_drop`. P3 scope: type only.
    Abandon {
        faction: u32,
        tile: (i32, i32),
        refund_drops: bool,
    },
    /// Move a Camp from `from` to `to` (nomadic only). P3 scope:
    /// type only — `nomad_migration_commit_system` handles the
    /// teardown + re-seed inline.
    Migrate {
        faction: u32,
        from: (i32, i32),
        to: (i32, i32),
    },
    /// Re-apply a different archetype to an existing faction.
    /// **P3 minimal: this variant is fully wired**, with the 7-step
    /// re-derivation list in `process_settlement_lifecycle_system`.
    SwitchArchetype {
        faction: u32,
        new_archetype_key: String,
        at_tile: (i32, i32),
    },
}

/// Drain target for `process_settlement_lifecycle_system`.
#[derive(Resource, Default)]
pub struct LifecycleEventQueue {
    pub events: Vec<SettlementLifecycleEvent>,
}

impl LifecycleEventQueue {
    pub fn push(&mut self, ev: SettlementLifecycleEvent) {
        self.events.push(ev);
    }
}

/// Map a nomadic archetype key to its settled counterpart for the
/// sedentarize path: `nomadic_subsistence` → `settled_subsistence`,
/// etc. Returns the input unchanged if it doesn't begin with
/// `nomadic_` (defensive — caller has already decided to sedentarize).
pub fn settled_variant_of(archetype_key: &str) -> String {
    if let Some(rest) = archetype_key.strip_prefix("nomadic_") {
        format!("settled_{}", rest)
    } else {
        archetype_key.to_string()
    }
}

/// Recover the `EconomyPreset` from an archetype key suffix. Returns
/// `Subsistence` as a safe default for unknown keys (non-fatal: the
/// re-derived caps will still produce a coherent settled archetype).
fn preset_from_key(key: &str) -> crate::game_state::EconomyPreset {
    use crate::game_state::EconomyPreset;
    if key.ends_with("subsistence") {
        EconomyPreset::Subsistence
    } else if key.ends_with("mixed") {
        EconomyPreset::Mixed
    } else if key.ends_with("market") {
        EconomyPreset::Market
    } else {
        EconomyPreset::Subsistence
    }
}

/// Exclusive-World drain of `LifecycleEventQueue`. Sequential schedule;
/// runs after the systems that emit (today: only
/// `nomad_sedentarize_system`).
pub fn process_settlement_lifecycle_system(world: &mut World) {
    // Pull the events out into a local Vec so we can release the
    // queue's borrow before running the per-event handler bodies
    // (which need `&mut World`).
    let pending: Vec<SettlementLifecycleEvent> = {
        let mut queue = world.resource_mut::<LifecycleEventQueue>();
        std::mem::take(&mut queue.events)
    };

    for ev in pending {
        match ev {
            SettlementLifecycleEvent::SwitchArchetype {
                faction,
                new_archetype_key,
                at_tile,
            } => {
                handle_switch_archetype(world, faction, new_archetype_key, at_tile);
            }
            // Establish / Migrate / Abandon are no-ops at the event
            // level today — existing systems handle them inline. Drop
            // the event to keep the queue from growing.
            _ => {}
        }
    }
}

/// 7-step re-derivation for the sedentarize / archetype-swap path,
/// per the unified plan's `SwitchArchetype` handler:
///
/// 1. Capability bundle swap (replaces `economic_policy`, `posting`,
///    `storage`, `land`, `income`, `inheritance`).
/// 2. `land_policy` re-derivation (separate field set once at spawn,
///    never re-derived elsewhere — confirmed by sedentarize trace).
/// 3. `economic_policy` re-derivation (mirror of caps').
/// 4. Camp structure cleanup at `at_tile`.
/// 5. `SettlementPlan.culture_hash` bump (forces re-carve on the
///    next planner tick).
/// 6. Synchronous `FactionStorageTile` bootstrap (closes the
///    bootstrap window — chief postings have somewhere to deposit
///    on tick +1).
/// 7. `ActivityEntryKind::Sedentarized` emit.
fn handle_switch_archetype(
    world: &mut World,
    faction_id: u32,
    new_archetype_key: String,
    at_tile: (i32, i32),
) {
    use crate::simulation::archetype::{
        derive_from_archetype_key, FactionArchetypeRegistry, StorageBackendKind,
    };
    use crate::simulation::faction::Lifestyle;

    let preset = preset_from_key(&new_archetype_key);

    // ── Steps 1-3: Faction-level state swap ────────────────────────
    {
        let catalog = world
            .resource::<crate::economy::resource_catalog::ResourceCatalog>()
            .clone();
        let archetype_registry = world.resource::<FactionArchetypeRegistry>().clone();
        let mut registry = world.resource_mut::<FactionRegistry>();
        let Some(faction) = registry.factions.get_mut(&faction_id) else {
            return;
        };

        // The destination lifestyle is implied by the prefix of the
        // new key: `settled_` → Settled, anything else → Nomadic.
        let new_lifestyle = if new_archetype_key.starts_with("settled_") {
            Lifestyle::Settled
        } else if new_archetype_key.starts_with("nomadic_") {
            Lifestyle::Nomadic
        } else {
            faction.lifestyle
        };
        faction.lifestyle = new_lifestyle;

        // Step 3 (rebuild map first; step 1's caps will mirror it).
        faction.economic_policy.clear();
        crate::economy::policy::apply_preset(
            &mut faction.economic_policy,
            preset,
            &catalog,
        );

        // Step 2: explicit re-derivation (set once at spawn, never
        // re-derived elsewhere — sedentarize trace finding).
        faction.land_policy = crate::economy::policy::land_policy_for(preset);

        // Step 1: full capability bundle swap. P5 routes through the
        // archetype registry; legacy fallback covers any key not yet
        // authored in the registry. The bundle mirrors the legacy
        // fields we just updated, so caps + legacy stay in lock-step
        // (P1a invariant).
        faction.caps = derive_from_archetype_key(
            &archetype_registry,
            &new_archetype_key,
            Some((new_lifestyle, preset, &catalog)),
        )
        .expect("derive_from_archetype_key with legacy fallback always returns Some");

        // For nomadic→settled the camp tile may differ from the
        // intended new home tile (today they're the same; future
        // phases may pick a fresh spot). Sync `home_tile` to
        // `at_tile`.
        faction.home_tile = at_tile;

        // Heads up: the storage backend kind has now changed. The
        // `compute_faction_storage_system` next-tick rollup will pick
        // up the new gate semantics automatically.
        let _ = StorageBackendKind::FactionTile; // anchor doc reference
    }

    // ── Step 4: Camp structure cleanup ─────────────────────────────
    despawn_camp_structures_at(world, at_tile);

    // ── Step 5: SettlementPlan.culture_hash bump ───────────────────
    bump_settlement_plan_culture_hash(world, faction_id);

    // ── Step 6: Synchronous storage tile bootstrap ─────────────────
    spawn_storage_tile_for(world, faction_id, at_tile);

    // ── Step 7: Activity log ───────────────────────────────────────
    emit_sedentarized_event(world, faction_id, at_tile);
}

/// Despawn `Camp` (if any), Beds/Bedrolls/Campfires/TentShelters and
/// stray Deployables within `OLD_CAMP_RADIUS` of `tile`. Mirrors
/// `nomad_migration_commit_system`'s teardown pass but driven by
/// archetype switch instead of migration.
fn despawn_camp_structures_at(world: &mut World, tile: (i32, i32)) {
    use crate::simulation::construction::{BedMap, CampfireMap};
    use crate::simulation::nomad::OLD_CAMP_RADIUS;
    use crate::simulation::pack_deploy::Deployable;

    fn cheb(a: (i32, i32), b: (i32, i32)) -> i32 {
        (a.0 - b.0).abs().max((a.1 - b.1).abs())
    }
    fn transform_to_tile(t: &Transform) -> (i32, i32) {
        let tx = (t.translation.x / crate::world::terrain::TILE_SIZE).floor() as i32;
        let ty = (t.translation.y / crate::world::terrain::TILE_SIZE).floor() as i32;
        (tx, ty)
    }

    let mut state: SystemState<(
        Commands,
        ResMut<BedMap>,
        ResMut<CampfireMap>,
        Query<(Entity, &Transform), With<Deployable>>,
        Query<(Entity, &Transform), With<crate::simulation::construction::TentShelter>>,
        ResMut<crate::simulation::camp::CampMap>,
        Query<&crate::simulation::camp::Camp>,
    )> = SystemState::new(world);
    let (
        mut commands,
        mut bed_map,
        mut campfire_map,
        deployable_q,
        tent_q,
        mut camp_map,
        camp_q,
    ) = state.get_mut(world);

    let mut despawned: ahash::AHashSet<Entity> = ahash::AHashSet::new();

    let bed_tiles: Vec<(i32, i32)> = bed_map
        .0
        .keys()
        .copied()
        .filter(|t| cheb(*t, tile) <= OLD_CAMP_RADIUS)
        .collect();
    for t in bed_tiles {
        if let Some(e) = bed_map.0.remove(&t) {
            commands.entity(e).despawn_recursive();
            despawned.insert(e);
        }
    }
    let fire_tiles: Vec<(i32, i32)> = campfire_map
        .0
        .keys()
        .copied()
        .filter(|t| cheb(*t, tile) <= OLD_CAMP_RADIUS)
        .collect();
    for t in fire_tiles {
        if let Some(e) = campfire_map.0.remove(&t) {
            commands.entity(e).despawn_recursive();
            despawned.insert(e);
        }
    }
    for (e, tr) in tent_q.iter() {
        if despawned.contains(&e) {
            continue;
        }
        if cheb(transform_to_tile(tr), tile) <= OLD_CAMP_RADIUS {
            commands.entity(e).despawn_recursive();
            despawned.insert(e);
        }
    }
    for (e, tr) in deployable_q.iter() {
        if despawned.contains(&e) {
            continue;
        }
        if cheb(transform_to_tile(tr), tile) <= OLD_CAMP_RADIUS {
            commands.entity(e).despawn_recursive();
        }
    }

    // Despawn the Camp entity owned by any faction whose camp tile is
    // within radius. (Today the Camp is per-faction-singleton at the
    // faction's home_tile; the radius check is defensive.)
    let mut camps_to_remove: Vec<u32> = Vec::new();
    for (fid, &entity) in camp_map.by_faction.iter() {
        if let Ok(camp) = camp_q.get(entity) {
            if cheb(camp.home_tile, tile) <= OLD_CAMP_RADIUS {
                commands.entity(entity).despawn_recursive();
                camps_to_remove.push(*fid);
            }
        }
    }
    for fid in camps_to_remove {
        camp_map.by_faction.remove(&fid);
    }

    state.apply(world);
}

/// Bump `SettlementPlan.culture_hash` for `faction_id` so the next
/// `settlement_planner_system` tick sees a hash mismatch and forces
/// `carve_plots_system` to re-carve plots under the freshly-applied
/// `LandPolicy`.
fn bump_settlement_plan_culture_hash(world: &mut World, faction_id: u32) {
    use crate::simulation::settlement::SettlementPlans;
    let mut plans = world.resource_mut::<SettlementPlans>();
    if let Some(plan) = plans.0.get_mut(&faction_id) {
        plan.culture_hash = plan.culture_hash.wrapping_add(1);
    }
    // If the plan doesn't exist yet, the next planner tick will
    // create it from scratch — no carryover to invalidate.
}

/// Spawn a `FactionStorageTile` at `tile` for `faction_id` if the
/// faction doesn't already have one indexed there. Synchronous so
/// chief postings posted on tick +1 have somewhere to deposit.
fn spawn_storage_tile_for(world: &mut World, faction_id: u32, tile: (i32, i32)) {
    use crate::simulation::faction::{FactionStorageTile, StorageTileMap};
    let already_present = {
        let map = world.resource::<StorageTileMap>();
        map.tiles.get(&tile).copied() == Some(faction_id)
    };
    if already_present {
        return;
    }
    let world_pos = crate::world::terrain::tile_to_world(tile.0, tile.1);
    world.spawn((
        FactionStorageTile { faction_id },
        Transform::from_xyz(world_pos.x, world_pos.y, 0.5),
        GlobalTransform::default(),
        Visibility::Hidden,
        InheritedVisibility::default(),
    ));
    // `StorageTileMap` is maintained by an indexing system; for the
    // synchronous bootstrap we also nudge the map directly so the
    // first chief posting on the next tick can see the tile without
    // waiting a frame.
    let mut map = world.resource_mut::<StorageTileMap>();
    map.tiles.insert(tile, faction_id);
    map.by_faction.entry(faction_id).or_default().push(tile);
}

/// Emit `ActivityEntryKind::Sedentarized` for `faction_id`'s sedentarize
/// completion.
fn emit_sedentarized_event(world: &mut World, faction_id: u32, camp: (i32, i32)) {
    use crate::simulation::faction::FactionMember;
    use crate::ui::activity_log::{ActivityEntryKind, ActivityLogEvent};

    // Pick any member of the faction as the "actor" for the event.
    // If the faction has no members (test fixture edge case) the
    // event is skipped — same fail-safe as the existing
    // `nomad_sedentarize_system`.
    let actor = {
        let mut state: SystemState<Query<(Entity, &FactionMember)>> = SystemState::new(world);
        let q = state.get(world);
        q.iter()
            .find(|(_, fm)| fm.faction_id == faction_id)
            .map(|(e, _)| e)
    };
    let Some(actor) = actor else { return };

    let tick = world.resource::<crate::simulation::schedule::SimClock>().tick;
    let mut events = world.resource_mut::<bevy::ecs::event::Events<ActivityLogEvent>>();
    events.send(ActivityLogEvent {
        tick,
        actor,
        faction_id,
        kind: ActivityEntryKind::Sedentarized { camp },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_variant_swaps_prefix() {
        assert_eq!(settled_variant_of("nomadic_subsistence"), "settled_subsistence");
        assert_eq!(settled_variant_of("nomadic_market"), "settled_market");
        // Non-nomadic input passes through unchanged.
        assert_eq!(settled_variant_of("settled_mixed"), "settled_mixed");
    }

    #[test]
    fn preset_from_key_recovers_suffix() {
        use crate::game_state::EconomyPreset;
        assert_eq!(
            preset_from_key("nomadic_subsistence"),
            EconomyPreset::Subsistence
        );
        assert_eq!(preset_from_key("settled_mixed"), EconomyPreset::Mixed);
        assert_eq!(preset_from_key("settled_market"), EconomyPreset::Market);
    }
}
