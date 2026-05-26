use ahash::AHashMap;
use bevy::prelude::*;

use crate::economy::agent::EconomicAgent;
use crate::economy::item::Item;
use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::simulation::carry::Carrier;
use crate::simulation::carve::{carve_tile, fill_tile};
use crate::simulation::construction::{Blueprint, BlueprintMap, BuildSiteKind};
use crate::simulation::faction::{FactionMember, SOLO};
use crate::simulation::goals::AgentGoal;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, Drafted, PersonAI, UNEMPLOYED_TASK_KIND};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::chunk_streaming::{TileCarvedEvent, TileChangedEvent};
use crate::world::globe::Globe;
use crate::world::terrain::{tile_to_world, WorldGen, TILE_SIZE};

pub const TERRAFORM_WORK_TICKS: u8 = 30;
const TERRAFORM_XP: u32 = 5;

/// Resource consumed when filling a tile back up. Stone today; resolved
/// from the catalog at first use because `ResourceId` isn't const.
fn terraform_fill_resource() -> crate::economy::resource_catalog::ResourceId {
    crate::economy::core_ids::stone()
}

/// Active terraform reservation on a single tile. The agent levels this
/// tile to `target_z` (digging down or filling up by one Z step at a time)
/// and the site is despawned once `surface_z_at(tile) == target_z`.
#[derive(Component)]
pub struct TerraformSite {
    pub faction_id: u32,
    pub target_z: i8,
}

/// Tile → TerraformSite entity. Used by plan dispatch to find work.
#[derive(Resource, Default)]
pub struct TerraformMap(pub AHashMap<(i32, i32), Entity>);

/// A footprint that's mid-terraform. Once every tile in `terraform_tiles`
/// drains from `TerraformMap`, `footprint_completion_system` spawns the
/// `wall_plan` blueprints (all sharing `target_z`) and removes the entry.
pub struct PendingFootprint {
    pub faction_id: u32,
    pub target_z: i8,
    pub terraform_tiles: Vec<(i32, i32)>,
    pub wall_plan: Vec<crate::simulation::construction::PlannedHouseTile>,
    /// sleepy-dove: who authored the deferred build, snapshotted at intent
    /// time. Carried so blueprints spawned by `footprint_completion_system`
    /// stamp the same `posted_by`/`design_techs` the immediate-spawn path
    /// would. `None` for legacy / seed callers.
    pub author: Option<crate::simulation::construction::BlueprintAuthor>,
}

#[derive(Resource, Default)]
pub struct PendingFootprints {
    pub queue: Vec<PendingFootprint>,
}

/// Faction-scoped count of in-flight TerraformSites (Debug panel).
pub fn count_terraform_sites_for(
    map: &TerraformMap,
    sites: &Query<&TerraformSite>,
    faction_id: u32,
) -> usize {
    map.0
        .values()
        .filter(|&&e| {
            sites
                .get(e)
                .map(|s| s.faction_id == faction_id)
                .unwrap_or(false)
        })
        .count()
}

/// Routes idle Build-goal faction members to the nearest TerraformSite of
/// their own faction. Runs in ParallelB before plan_execution so once a
/// task is assigned, plan_execution skips this agent (it's no longer Idle).
/// Bypasses the plan system intentionally — terraform is a transient
/// pre-build phase, not a goal-bearing activity.
pub fn terraform_dispatch_system(
    chunk_map: Res<ChunkMap>,
    chunk_graph: Res<ChunkGraph>,
    chunk_router: Res<ChunkRouter>,
    chunk_connectivity: Res<ChunkConnectivity>,
    spatial_index: Res<crate::world::spatial::SpatialIndex>,
    stand_reservations: Res<crate::simulation::stand_reservation::StandTileReservations>,
    clock: Res<crate::simulation::SimClock>,
    terraform_map: Res<TerraformMap>,
    site_query: Query<&TerraformSite>,
    mut agent_query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &AgentGoal,
            &FactionMember,
            &Transform,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    if terraform_map.0.is_empty() {
        return;
    }
    let now = clock.tick;
    for (actor, mut ai, mut aq, goal, member, transform, lod) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if member.faction_id == SOLO {
            continue;
        }
        if !matches!(goal, AgentGoal::Build) {
            continue;
        }
        if ai.state != AiState::Idle || aq.current_task_kind() != UNEMPLOYED_TASK_KIND {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );

        let mut best: Option<((i32, i32), i32)> = None;
        for (&tile, &e) in &terraform_map.0 {
            let Ok(site) = site_query.get(e) else {
                continue;
            };
            if site.faction_id != member.faction_id {
                continue;
            }
            let dist = (tile.0 as i32 - cur_tx).abs() + (tile.1 as i32 - cur_ty).abs();
            if best.map(|(_, bd)| dist < bd).unwrap_or(true) {
                best = Some((tile, dist));
            }
        }
        if let Some((tile, _)) = best {
            let routed = assign_task_with_routing(
                &mut ai,
                (cur_tx as i32, cur_ty as i32),
                cur_chunk,
                tile,
                TaskKind::Terraform,
                None,
                None,
                &chunk_graph,
                &chunk_router,
                &chunk_map,
                &chunk_connectivity,
                &spatial_index,
                &stand_reservations,
                actor,
                now,
                );
            if routed {
                aq.dispatch(crate::simulation::typed_task::Task::Terraform { tile });
            }
        }
    }
}

/// Each tick that an agent reaches `Working` state on a Terraform task,
/// step the surface one block toward `target_z`. Carves stone if too high,
/// fills with stone (consumed from inventory) if too low.
pub fn terraform_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    mut tile_carved: EventWriter<TileCarvedEvent>,
    mut terraform_map: ResMut<TerraformMap>,
    site_query: Query<&TerraformSite>,
    clock: Res<SimClock>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    mut agent_query: Query<(
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut EconomicAgent,
        &mut Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut aq, mut agent, mut carrier, mut skills, slot, lod) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Terraform as u16 {
            continue;
        }
        if ai.work_progress < TERRAFORM_WORK_TICKS {
            continue;
        }
        ai.work_progress = 0;

        let dest = ai.dest_tile;
        let Some(&site_entity) = terraform_map.0.get(&dest) else {
            aq.cancel_chain(&mut ai);
            continue;
        };
        let Ok(site) = site_query.get(site_entity) else {
            terraform_map.0.remove(&dest);
            aq.cancel_chain(&mut ai);
            continue;
        };

        let tx = dest.0 as i32;
        let ty = dest.1 as i32;
        let surf = chunk_map.surface_z_at(tx, ty);
        let target = site.target_z as i32;

        if surf > target {
            let target_floor = surf - 1;
            let drops = carve_tile(
                &mut chunk_map,
                &gen,
                &globe,
                tx,
                ty,
                target_floor,
                &mut tile_changed,
            );
            // Signal a real excavation so `aquifer_seep_emitter_system` can
            // bootstrap groundwater seep when a dig (e.g. a well shaft) drops
            // below the water table. The seep emitter gates on the per-cell
            // table, so above-table footprint leveling stays a no-op.
            tile_carved.send(TileCarvedEvent {
                tx,
                ty,
                new_floor_z: target_floor,
            });
            for (resource_id, qty) in drops {
                if qty == 0 {
                    continue;
                }
                let item = Item::new_commodity(resource_id);
                let leftover = carrier.try_pick_up(item, qty);
                if leftover > 0 {
                    let pos = tile_to_world(tx, ty);
                    commands.spawn((
                        GroundItem {
                            item,
                            qty: leftover,
                            owner_household: None,
                        },
                        Transform::from_xyz(pos.x, pos.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                        crate::world::spatial::Indexed::new(
                            crate::world::spatial::IndexedKind::GroundItem,
                        ),
                    ));
                }
            }
            skills.gain_xp(SkillKind::Mining, TERRAFORM_XP);
        } else if surf < target {
            // Filler can come from hands first (just-mined stone) or personal inventory.
            let fill_id = terraform_fill_resource();
            let in_hand = carrier.quantity_of_resource(fill_id);
            let in_inv = agent.quantity_of_resource(fill_id);
            if in_hand + in_inv < 1 {
                aq.cancel_chain(&mut ai);
                continue;
            }
            let target_floor = surf + 1;
            let filled = fill_tile(&mut chunk_map, tx, ty, target_floor, &mut tile_changed);
            if filled > 0 {
                if in_hand > 0 {
                    carrier.remove_resource(fill_id, 1);
                } else {
                    agent.remove_resource(fill_id, 1);
                }
                skills.gain_xp(SkillKind::Building, TERRAFORM_XP);
            }
        }

        let new_surf = chunk_map.surface_z_at(tx, ty);
        if new_surf == target {
            commands.entity(site_entity).despawn();
            terraform_map.0.remove(&dest);
        }

        // One unit of terraform work done. Exit the typed slot so the next
        // dispatcher tick re-plans (another tile may still need carving/filling).
        aq.finish_task(&mut ai);
    }
}

/// Drains `PendingFootprints` whose terraform tiles have all cleared,
/// clearing any remaining obstacles (interior plants, loose rocks) in
/// the footprint synchronously, then spawning the wall blueprints with
/// the shared `target_z`. Per-blueprint obstacle clearing for the
/// perimeter walls is handled by `populate_pending_clear_system` once
/// the new blueprints are spawned; this pass catches interior tiles
/// that have no individual blueprint of their own.
pub fn footprint_completion_system(
    mut commands: Commands,
    mut bp_map: ResMut<BlueprintMap>,
    terraform_map: Res<TerraformMap>,
    mut pending: ResMut<PendingFootprints>,
    spatial: Res<crate::world::spatial::SpatialIndex>,
    chunk_map: Res<ChunkMap>,
    obstacles: Query<(
        &crate::world::spatial::Indexed,
        &crate::simulation::obstacle::ConstructionObstacle,
    )>,
    plants: Query<&crate::simulation::plants::Plant>,
    mut plant_map: ResMut<crate::simulation::plants::PlantMap>,
    mut plant_sprite_index: ResMut<crate::simulation::plants::PlantSpriteIndex>,
    mut transforms: Query<&mut Transform>,
) {
    let mut i = 0;
    while i < pending.queue.len() {
        let still_pending = pending.queue[i]
            .terraform_tiles
            .iter()
            .any(|t| terraform_map.0.contains_key(t));
        if still_pending {
            i += 1;
            continue;
        }
        let p = pending.queue.swap_remove(i);
        let footprint_tiles: Vec<(i32, i32)> = p.terraform_tiles.clone();
        let bp_map_ref = &bp_map;
        crate::simulation::obstacle::resolve_footprint_sync(
            &footprint_tiles,
            p.target_z,
            &mut commands,
            &spatial,
            &chunk_map,
            &obstacles,
            &plants,
            &mut plant_map,
            &mut plant_sprite_index,
            &mut transforms,
            &|tile| bp_map_ref.0.contains_key(&tile),
            &mut |c, tx, ty, rid, qty| {
                crate::simulation::gather::spawn_ground_drop(c, tx, ty, rid, qty);
            },
        );
        for entry in &p.wall_plan {
            if bp_map.0.contains_key(&entry.tile) {
                continue;
            }
            let wp = tile_to_world(entry.tile.0 as i32, entry.tile.1 as i32);
            let mut bp = Blueprint::new(p.faction_id, None, entry.kind, entry.tile, p.target_z)
                .with_author(p.author);
            if let Some(e) = entry.door_edge {
                bp = bp.with_door_dir(e);
            }
            if let Some(role) = entry.hearth_role {
                bp = bp.with_hearth_role(role);
            }
            let e = commands
                .spawn((
                    bp,
                    Transform::from_xyz(wp.x, wp.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                ))
                .id();
            bp_map.0.insert(entry.tile, e);
        }
    }
}
