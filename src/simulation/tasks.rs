use super::faction::StorageTileMap;
use super::gather_claims::{release_gather_claim, GatherClaims};
use super::goals::AgentGoal;
use super::items::GroundItem;
use super::lod::LodLevel;
use super::memory::RelationshipMemory;
use super::needs::Needs;
use super::person::{AiState, Drafted, PersonAI};
use super::plants::{GrowthStage, Plant, PlantKind, PlantMap};

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::spatial::SpatialIndex;
use crate::world::terrain::TILE_SIZE;
use crate::world::tile::TileKind;
use bevy::prelude::*;

/// Represents the current active task an agent is performing.
/// Tasks are transient and managed by either the plan system or the goal dispatch system.
/// An agent is "unemployed" when they are between tasks or idling.
#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Idle = 0,
    Gather = 1,
    Trader = 2,
    Raid = 3,
    Defend = 4,
    Planter = 5,
    Hunter = 6,
    Scavenge = 7,
    Construct = 8,        // build wall tile
    ConstructBed = 9,     // spawn bed entity
    DepositResource = 10, // return to camp and deposit goods
    Socialize = 11,
    Explore = 13,
    Dig = 14, // dig down at surface or mine a wall tile
    Sleep = 15,
    Eat = 16,              // consume one food item from inventory over several ticks
    WithdrawFood = 17,     // pull one food item from a faction storage tile into inventory
    TameAnimal = 18,       // work adjacent to a wild horse for ~100 ticks to tame it
    Craft = 19,            // craft an item in-place using inventory ingredients
    Deconstruct = 20, // dismantle placed furniture (e.g. bed) and carry recovered wood to storage
    Lead = 21,        // tribal chief stations at faction home and issues build orders
    Terraform = 22,   // level a footprint tile to a target Z (dig down or fill up by one Z step)
    HaulMaterials = 23, // carry inventory goods to a blueprint and drop them into its deposit slots
    MilitaryMove = 24, // drafted unit walking to a player-issued destination, idles on arrival
    MilitaryAttack = 25, // drafted unit chasing a target entity to attack it adjacent
    Play = 26,        // recreation: refills willpower, optionally builds bonds with a partner
    WithdrawMaterial = 27, // pull one good currently needed by a faction blueprint from a storage tile
    WithdrawGood = 28, // pull one of a specific good (encoded in craft_recipe_id) from a faction storage tile; sentinel 255 = any entertainment-value good
    PlayPlant = 29, // recreational planting: consumes a Seed from inventory, spawns a Grain plant, awards Farming XP + activity, bursts willpower
    PlayThrow = 30, // recreational rock-throwing: consumes a Stone from inventory, awards Combat XP + activity, bursts willpower
    HaulToCraftOrder = 31, // carry inventory goods to a faction CraftOrder anchor and drop them into deposit slots
    WorkOnCraftOrder = 32, // adjacent to a satisfied CraftOrder anchor; advances work_progress until the recipe completes
    PickUpCorpse = 33, // walk to a fresh `Corpse` entity and attach it via PersonAI.carried_corpse
    HaulCorpse = 34, // walk a carried corpse to a butcher site (hearth / faction camp) and stand there
    Butcher = 35,    // adjacent to own carried corpse; work_ticks then yield Meat+Skin and despawn
    Equip = 36,      // instant: move a matching Item from inventory/Carrier into Equipment[slot]
    HuntPartyMuster = 37, // hunter waiting at hearth for the chief's hunting party to fill
    Read = 38, // study a tablet/book held in inventory; accumulates StudyProgress on its tech_payload
    Teach = 39, // adjacent to a target student, both stand still while teacher transfers progress
    HoldLecture = 40, // stand at lecture anchor and broadcast progress to nearby Attending students
    AttendLecture = 41, // student rooted near a Lecturing teacher, accumulating progress per tick
    /// P2b: nomadic / member-pool withdraw. Walks the actor adjacent
    /// to a fellow faction member (`PersonAI.target_entity`), then
    /// the executor moves `qty` units of `resource_id` from the
    /// target's `EconomicAgent.inventory` (or hands) into the actor's.
    /// Mirrors `WithdrawMaterial` for the FactionTile path; reaches
    /// the executor by `take_from_member_task_system`.
    TakeFromMember = 42,
    /// P1 (active migration): walking with the band toward the new
    /// camp tile after `nomad_migration_commit_system` flipped the
    /// faction's `home_tile`. Driven by `MigrationTarget` component
    /// + `nomad_migration_dispatch_system` (ParallelB) +
    /// `nomad_migration_arrival_system` (Sequential, after movement).
    Migrate = 43,
    /// Worker walks adjacent to a `ConstructionObstacle`-tagged entity
    /// inside a blueprint's footprint, accumulates work_progress, then
    /// despawns the entity (dropping any yields on the ground) and
    /// pops it from the blueprint's `pending_clear`. Distinct from
    /// `Gather` because the work is structure-prerequisite, not
    /// resource-acquisition; yields go to ground for haulers.
    ClearObstacle = 44,
    /// Part B: worker walks to a `Deployable` nomadic structure (Bed
    /// / TentShelter / Yurt / Campfire), ticks `work_progress` for
    /// `UNPITCH_WORK_TICKS`, then despawns the entity and drops its
    /// `packed_form` good (and / or `packed_bundles` entries; or
    /// `refund_resource`) as `GroundItem`s at the structure tile.
    UnpitchStructure = 45,
    /// Part B: worker walks to a target tile carrying a packed good,
    /// drops it as a `GroundItem`. Used by the Pitch slow-path to
    /// pre-stage cargo at the new camp before structures pitch.
    UnloadCampCargo = 46,
    /// Part B: worker walks to `anchor`, consumes the matching packed
    /// good from a co-located `GroundItem` (or inventory), and spawns
    /// the structure of the named `BuildSiteKind`.
    PitchStructureAt = 47,
    /// Heal-3: Healer walks adjacent to a target patient carrying an
    /// `Injury` and ticks down the patient's `Injury.severity` while
    /// in range. Grants Medicine XP to the Healer. Patient-side
    /// `AgentGoal::SeekCare` carries no typed task — patients use
    /// `Task::WalkTo` to reach the nearest same-faction Healer; the
    /// transfer fires from the Healer's `Task::Heal`.
    Heal = 48,
}

/// Human-readable label for a `TaskKind` discriminant. Returns "Unemployed"
/// for unknown ids (notably `PersonAI::UNEMPLOYED == u16::MAX`).
pub fn task_kind_label(task_id: u16) -> &'static str {
    match task_id {
        x if x == TaskKind::Idle as u16 => "Idle",
        x if x == TaskKind::Gather as u16 => "Gatherer",
        x if x == TaskKind::Trader as u16 => "Trader",
        x if x == TaskKind::Raid as u16 => "Raider",
        x if x == TaskKind::Defend as u16 => "Defender",
        x if x == TaskKind::Planter as u16 => "Planter",
        x if x == TaskKind::Hunter as u16 => "Hunter",
        x if x == TaskKind::Scavenge as u16 => "Scavenger",
        x if x == TaskKind::Construct as u16 => "Builder",
        x if x == TaskKind::ConstructBed as u16 => "Building Bed",
        x if x == TaskKind::HaulMaterials as u16 => "Hauling Materials",
        x if x == TaskKind::Deconstruct as u16 => "Deconstructing",
        x if x == TaskKind::DepositResource as u16 => "Depositing Resources",
        x if x == TaskKind::Socialize as u16 => "Socializing",
        x if x == TaskKind::Explore as u16 => "Explorer",
        x if x == TaskKind::Dig as u16 => "Digger",
        x if x == TaskKind::Sleep as u16 => "Sleeper",
        x if x == TaskKind::Eat as u16 => "Eating",
        x if x == TaskKind::WithdrawFood as u16 => "Withdrawing Food",
        x if x == TaskKind::WithdrawMaterial as u16 => "Withdrawing Material",
        x if x == TaskKind::TakeFromMember as u16 => "Taking From Teammate",
        x if x == TaskKind::TameAnimal as u16 => "Taming",
        x if x == TaskKind::Craft as u16 => "Crafter",
        x if x == TaskKind::Lead as u16 => "Leading",
        x if x == TaskKind::Terraform as u16 => "Levelling Ground",
        x if x == TaskKind::MilitaryMove as u16 => "Military Move",
        x if x == TaskKind::MilitaryAttack as u16 => "Military Attack",
        x if x == TaskKind::Play as u16 => "Playing",
        x if x == TaskKind::WithdrawGood as u16 => "Withdrawing",
        x if x == TaskKind::PlayPlant as u16 => "Play-Planting",
        x if x == TaskKind::PlayThrow as u16 => "Play-Throwing",
        x if x == TaskKind::HaulToCraftOrder as u16 => "Hauling to Craft Order",
        x if x == TaskKind::WorkOnCraftOrder as u16 => "Working on Craft Order",
        x if x == TaskKind::PickUpCorpse as u16 => "Picking Up Corpse",
        x if x == TaskKind::HaulCorpse as u16 => "Hauling Corpse",
        x if x == TaskKind::Butcher as u16 => "Butchering",
        x if x == TaskKind::Equip as u16 => "Equipping",
        x if x == TaskKind::HuntPartyMuster as u16 => "Mustering for Hunt",
        x if x == TaskKind::Read as u16 => "Reading",
        x if x == TaskKind::Teach as u16 => "Teaching",
        x if x == TaskKind::HoldLecture as u16 => "Lecturing",
        x if x == TaskKind::AttendLecture as u16 => "Attending Lecture",
        x if x == TaskKind::Migrate as u16 => "Migrating to Camp",
        x if x == TaskKind::ClearObstacle as u16 => "Clearing Obstacle",
        x if x == TaskKind::UnpitchStructure as u16 => "Packing Camp",
        x if x == TaskKind::UnloadCampCargo as u16 => "Unloading Cargo",
        x if x == TaskKind::PitchStructureAt as u16 => "Pitching Structure",
        x if x == TaskKind::Heal as u16 => "Healing",
        _ => "Unemployed",
    }
}

/// How many free hands the task requires the agent to have before they can begin
/// (or continue) work. Hauling and gathering tasks are EXEMPT — the load is the
/// whole point. Tasks like Sleep/Socialize return 0 because they don't
/// "use" hands, but we drop hand-held loads at task-entry separately (see
/// `goal_dispatch_system`).
pub fn task_requires_free_hands(task_id: u16) -> u8 {
    match task_id {
        x if x == TaskKind::Craft as u16 || x == TaskKind::WorkOnCraftOrder as u16 => 2,
        x if x == TaskKind::Construct as u16
            || x == TaskKind::ConstructBed as u16
            || x == TaskKind::Dig as u16
            || x == TaskKind::Terraform as u16
            || x == TaskKind::Deconstruct as u16
            || x == TaskKind::Gather as u16
            || x == TaskKind::Planter as u16
            || x == TaskKind::Hunter as u16
            || x == TaskKind::Raid as u16
            || x == TaskKind::Defend as u16
            || x == TaskKind::MilitaryAttack as u16
            || x == TaskKind::TameAnimal as u16
            || x == TaskKind::ClearObstacle as u16 =>
        {
            1
        }
        _ => 0,
    }
}

/// Tasks that should drop hand-held loads at entry (the activity is incompatible
/// with carrying things). Stacks become GroundItems at the agent's tile.
pub fn task_drops_hand_load(task_id: u16) -> bool {
    task_id == TaskKind::Sleep as u16 || task_id == TaskKind::Socialize as u16
}

/// Returns true for tasks where the agent works from an adjacent tile rather than
/// stepping onto the resource tile itself.
pub fn task_interacts_from_adjacent(task_id: u16) -> bool {
    task_id == TaskKind::Gather as u16
        || task_id == TaskKind::Dig as u16
        || task_id == TaskKind::Planter as u16
        || task_id == TaskKind::Construct as u16
        || task_id == TaskKind::ConstructBed as u16
        || task_id == TaskKind::ClearObstacle as u16
        || task_id == TaskKind::HaulMaterials as u16
        || task_id == TaskKind::DepositResource as u16
        || task_id == TaskKind::TameAnimal as u16
        || task_id == TaskKind::Deconstruct as u16
        || task_id == TaskKind::Terraform as u16
        || task_id == TaskKind::Scavenge as u16
        || task_id == TaskKind::WithdrawFood as u16
        || task_id == TaskKind::WithdrawMaterial as u16
        || task_id == TaskKind::Socialize as u16
        || task_id == TaskKind::Raid as u16
        || task_id == TaskKind::Defend as u16
        || task_id == TaskKind::Lead as u16
        || task_id == TaskKind::MilitaryAttack as u16
        || task_id == TaskKind::Play as u16
        || task_id == TaskKind::WithdrawGood as u16
        || task_id == TaskKind::PlayPlant as u16
        || task_id == TaskKind::HaulToCraftOrder as u16
        || task_id == TaskKind::WorkOnCraftOrder as u16
        || task_id == TaskKind::PickUpCorpse as u16
        || task_id == TaskKind::Butcher as u16
        || task_id == TaskKind::UnpitchStructure as u16
        || task_id == TaskKind::PitchStructureAt as u16
        || task_id == TaskKind::Heal as u16
}

/// Tasks that count as productive labor — these drain willpower over time
/// (see `tick_needs_system`). Recreational, recovery, social, and combat
/// tasks are excluded; they don't tire the mind in the same way.
pub fn task_is_labor(task_id: u16) -> bool {
    task_id == TaskKind::Gather as u16
        || task_id == TaskKind::Dig as u16
        || task_id == TaskKind::Construct as u16
        || task_id == TaskKind::ConstructBed as u16
        || task_id == TaskKind::Deconstruct as u16
        || task_id == TaskKind::HaulMaterials as u16
        || task_id == TaskKind::DepositResource as u16
        || task_id == TaskKind::Planter as u16
        || task_id == TaskKind::Hunter as u16
        || task_id == TaskKind::Scavenge as u16
        || task_id == TaskKind::Craft as u16
        || task_id == TaskKind::Terraform as u16
        || task_id == TaskKind::TameAnimal as u16
        || task_id == TaskKind::WithdrawFood as u16
        || task_id == TaskKind::WithdrawMaterial as u16
        || task_id == TaskKind::HaulToCraftOrder as u16
        || task_id == TaskKind::WorkOnCraftOrder as u16
        || task_id == TaskKind::HaulCorpse as u16
        || task_id == TaskKind::Butcher as u16
}

/// Spiral search outward from `target` for the closest tile that is passable
/// at its surface Z and reachable (via `ChunkConnectivity`) from
/// `agent_origin`. Used as a "wander toward target" fallback when the strict
/// adjacency pick in `assign_task_with_routing` finds no usable tile — the
/// agent walks toward the goal so the next dispatch tick can retry adjacency
/// from a closer position.
pub fn nearest_reachable_tile_near(
    chunk_map: &ChunkMap,
    chunk_connectivity: &ChunkConnectivity,
    target: (i32, i32),
    agent_origin: (ChunkCoord, i8),
    radius: i32,
) -> Option<(i32, i32)> {
    let csz = CHUNK_SIZE as i32;
    let mut best: Option<(i32, i32)> = None;
    let mut best_dist = i32::MAX;
    for r in 1..=radius {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // ring-only iteration
                }
                let tx = target.0 + dx;
                let ty = target.1 + dy;
                let tz = chunk_map.surface_z_at(tx, ty);
                if !chunk_map.passable_at(tx, ty, tz) {
                    continue;
                }
                let n_chunk = ChunkCoord(tx.div_euclid(csz), ty.div_euclid(csz));
                if !chunk_connectivity.is_reachable(agent_origin, (n_chunk, tz as i8)) {
                    continue;
                }
                let dist = dx.abs() + dy.abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx as i32, ty as i32));
                }
            }
        }
        if best.is_some() {
            return best;
        }
    }
    best
}

/// Spiral outward from `origin_xy` looking for a passable tile whose surface
/// Z is strictly higher than the agent's current Z AND is in the same
/// connectivity component as the agent. Used by the `ReturnToSurface`
/// recovery to give a stuck-underground agent a concrete reachable goal so
/// they walk back up via whatever ramp/staircase actually connects their
/// current cavern to higher ground. Returns `None` if no higher reachable
/// tile is within `max_radius` — agent is genuinely sealed in.
pub fn nearest_reachable_higher_tile(
    chunk_map: &ChunkMap,
    chunk_connectivity: &ChunkConnectivity,
    origin_xy: (i32, i32),
    agent_origin: (ChunkCoord, i8),
    max_radius: i32,
) -> Option<(i32, i32)> {
    let csz = CHUNK_SIZE as i32;
    let agent_z = agent_origin.1;
    for r in 1..=max_radius {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs() != r && dy.abs() != r {
                    continue; // ring-only iteration
                }
                let tx = origin_xy.0 as i32 + dx;
                let ty = origin_xy.1 as i32 + dy;
                let surf_z = chunk_map.surface_z_at(tx, ty);
                if surf_z as i8 <= agent_z {
                    continue;
                }
                if !chunk_map.passable_at(tx, ty, surf_z) {
                    continue;
                }
                let n_chunk = ChunkCoord(tx.div_euclid(csz), ty.div_euclid(csz));
                if !chunk_connectivity.is_reachable(agent_origin, (n_chunk, surf_z as i8)) {
                    continue;
                }
                return Some((tx as i32, ty as i32));
            }
        }
    }
    None
}

pub fn find_nearest_tile(
    chunk_map: &ChunkMap,
    from: (i32, i32),
    radius: i32,
    kinds: &[TileKind],
) -> Option<(i32, i32)> {
    let mut best: Option<(i32, i32)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if let Some(kind) = chunk_map.tile_kind_at(tx, ty) {
                if kinds.contains(&kind) {
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((tx as i32, ty as i32));
                    }
                }
            }
        }
    }
    best
}

pub fn find_nearest_plant(
    plant_map: &PlantMap,
    from: (i32, i32),
    radius: i32,
    plant_query: &Query<&Plant>,
    mature_only: bool,
    kind_filter: Option<PlantKind>,
) -> Option<(Entity, i32, i32)> {
    let mut best: Option<(Entity, i32, i32)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if let Some(&entity) = plant_map.0.get(&(tx, ty)) {
                if let Ok(plant) = plant_query.get(entity) {
                    if mature_only && plant.stage != GrowthStage::Mature {
                        continue;
                    }
                    if let Some(k) = kind_filter {
                        if plant.kind != k {
                            continue;
                        }
                    }
                    let dist = dx.abs() + dy.abs();
                    if dist < best_dist {
                        best_dist = dist;
                        best = Some((entity, tx as i32, ty as i32));
                    }
                }
            }
        }
    }
    best
}

// Bug 2 fix: filter by `good` so agents don't target the wrong item type.
//
// Faction storage tiles are unconditionally excluded: this helper exists to
// find wild ground items, and plans that want stored food / goods use the
// dedicated `StepTarget::NearestFactionStorage*` variants instead.
pub fn find_nearest_edible(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    item_query: &Query<&GroundItem>,
    storage_tile_map: &StorageTileMap,
) -> Option<(Entity, i32, i32)> {
    let mut best: Option<(Entity, i32, i32)> = None;
    let mut best_dist = i32::MAX;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;

            if storage_tile_map.tiles.contains_key(&(tx as i32, ty as i32)) {
                continue;
            }

            for &e in spatial.get(tx, ty) {
                if let Ok(item) = item_query.get(e) {
                    if item.item.resource_id.is_edible() {
                        let dist = dx.abs() + dy.abs();
                        if dist < best_dist {
                            best_dist = dist;
                            best = Some((e, tx as i32, ty as i32));
                        }
                    }
                }
            }
        }
    }
    best
}

// See `find_nearest_edible` — faction storage tiles are unconditionally
// excluded; storage-aware searches use `StepTarget::NearestFactionStorage*`.
pub fn find_nearest_item(
    spatial: &SpatialIndex,
    from: (i32, i32),
    radius: i32,
    resource_id: crate::economy::resource_catalog::ResourceId,
    item_query: &Query<&GroundItem>,
    storage_tile_map: &StorageTileMap,
) -> Option<(Entity, i32, i32)> {
    let mut best: Option<(Entity, i32, i32)> = None;
    let mut best_dist = i32::MAX;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;

            if storage_tile_map.tiles.contains_key(&(tx as i32, ty as i32)) {
                continue;
            }

            for &e in spatial.get(tx, ty) {
                if let Ok(item) = item_query.get(e) {
                    if item.item.resource_id == resource_id {
                        let dist = dx.abs() + dy.abs();
                        if dist < best_dist {
                            best_dist = dist;
                            best = Some((e, tx as i32, ty as i32));
                        }
                    }
                }
            }
        }
    }
    best
}

/// Find the nearest plant-able tile within `radius` of `from`. A tile is
/// plant-able when it's `Grass` or any of the four soil variants (Loam, Silt,
/// Clay, SandySoil) and has no plant on it yet. Replaces the legacy
/// Farmland-only check now that crops grow on natural soil.
pub fn find_nearest_unplanted_farmland(
    chunk_map: &ChunkMap,
    plant_map: &PlantMap,
    from: (i32, i32),
    radius: i32,
) -> Option<(i32, i32)> {
    let mut best: Option<(i32, i32)> = None;
    let mut best_dist = i32::MAX;

    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let tx = from.0 + dx;
            let ty = from.1 + dy;
            if plant_map.0.contains_key(&(tx, ty)) {
                continue;
            }
            let plantable = match chunk_map.tile_kind_at(tx, ty) {
                Some(TileKind::Grass) => true,
                Some(k) if k.is_soil_like() => true,
                _ => false,
            };
            if plantable {
                let dist = dx.abs() + dy.abs();
                if dist < best_dist {
                    best_dist = dist;
                    best = Some((tx as i32, ty as i32));
                }
            }
        }
    }
    best
}

pub fn assign_task_with_routing(
    ai: &mut PersonAI,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    target: (i32, i32),
    task: TaskKind,
    target_entity: Option<Entity>,
    chunk_graph: &ChunkGraph,
    chunk_router: &ChunkRouter,
    chunk_map: &ChunkMap,
    chunk_connectivity: &ChunkConnectivity,
) -> bool {
    // Resource's standable Z — used to gate which adjacent tiles can actually
    // reach the work tile. The agent's `target_z` is set below to the route
    // tile's Z (where they'll stand), not this — otherwise flow-field seeds
    // and Working-transition checks fire at the wrong Z on sloped ground.
    let resource_z =
        chunk_map.nearest_standable_z(target.0 as i32, target.1 as i32, ai.current_z as i32) as i8;

    // For tasks where the agent works from beside the target (not on it), route to
    // the nearest passable adjacent tile within ±1 Z of the resource so the agent
    // can actually interact once they arrive. Also require the candidate tile to
    // share a connectivity component with the agent — otherwise the path worker
    // will reject every request to that tile and the agent will loop forever on
    // the same unreachable resource.
    let route_target = if task_interacts_from_adjacent(task as u16) {
        let (tx, ty) = (target.0 as i32, target.1 as i32);
        let (ax, ay) = (cur_tile.0 as i32, cur_tile.1 as i32);
        const ADJ: [(i32, i32); 8] = [
            (-1, 0),
            (1, 0),
            (0, -1),
            (0, 1),
            (-1, -1),
            (1, -1),
            (-1, 1),
            (1, 1),
        ];
        let agent_z = ai.current_z;
        let csz = CHUNK_SIZE as i32;
        let picked = ADJ
            .iter()
            .map(|&(dx, dy)| (tx + dx, ty + dy))
            .filter(|&(ntx, nty)| {
                let nz = chunk_map.surface_z_at(ntx, nty);
                if !chunk_map.passable_at(ntx, nty, nz) {
                    return false;
                }
                let dz = nz as i32 - resource_z as i32;
                if !(-1..=1).contains(&dz) {
                    return false;
                }
                let n_chunk = ChunkCoord(ntx.div_euclid(csz), nty.div_euclid(csz));
                chunk_connectivity.is_reachable((cur_chunk, agent_z), (n_chunk, nz as i8))
            })
            .min_by_key(|&(ntx, nty)| (ntx - ax).abs() + (nty - ay).abs())
            .map(|(ntx, nty)| (ntx as i32, nty as i32));
        match picked {
            Some(t) => t,
            None => {
                // No reachable adjacent tile (camp perimeter blocked, target
                // sealed in by walls / blueprints, etc.). Fall back to the
                // nearest passable, reachable tile in a small radius around
                // the target — agent walks toward it and the next dispatch
                // tick re-tries adjacency from a closer position. Without
                // this fallback the agent loops Idle forever on a goal whose
                // adjacency happens to be temporarily blocked.
                match nearest_reachable_tile_near(
                    chunk_map,
                    chunk_connectivity,
                    (tx, ty),
                    (cur_chunk, agent_z),
                    8,
                ) {
                    Some(t) => t,
                    None => {
                        // Truly unreachable from this connectivity component.
                        // Clear target so the inspector reflects reality and
                        // the caller can mark the goal failed.
                        ai.target_tile = cur_tile;
                        ai.dest_tile = cur_tile;
                        ai.target_entity = None;
                        return false;
                    }
                }
            }
        }
    } else {
        target
    };

    ai.task_id = task as u16;
    ai.dest_tile = target;
    ai.target_entity = target_entity;

    ai.target_z = chunk_map.nearest_standable_z(
        route_target.0 as i32,
        route_target.1 as i32,
        ai.current_z as i32,
    ) as i8;

    let route_chunk = ChunkCoord(
        (route_target.0 as i32).div_euclid(CHUNK_SIZE as i32),
        (route_target.1 as i32).div_euclid(CHUNK_SIZE as i32),
    );
    if route_chunk == cur_chunk {
        ai.state = AiState::Seeking;
        ai.target_tile = route_target;
    } else if let Some(wp) =
        chunk_router.first_waypoint(chunk_graph, cur_chunk, route_chunk, ai.current_z)
    {
        ai.state = AiState::Routing;
        ai.target_tile = wp;
    } else {
        ai.state = AiState::Seeking;
        ai.target_tile = route_target;
    }
    true
}

/// Stale-task reset + Explore cleanup. The Sleep arm that used to live here
/// moved to `htn::htn_dispatch_system` in Phase 5a-ii. With `plan_execution_system`
/// retired in Phase 7, every goal is HTN-driven, so this system just handles
/// the two pieces that don't belong anywhere else:
///
/// 1. **Stale-task reset.** A leftover `task_id` from an abandoned dispatch
///    is cleared (and `aq.cancel()` drops the prefetched queue) unless the
///    goal legitimately keeps the task alive (e.g. Sleep keeps
///    `TaskKind::Sleep`, Survive keeps an in-progress `Eat`).
/// 2. **Explore cleanup.** Gather/Survive/Build share the Explore method, so
///    when one of those goals flips and a stale `Explore` task is still
///    Working, drop it back to Idle.
pub fn goal_dispatch_system(
    gather_claims: Res<GatherClaims>,
    mut query: Query<
        (
            Entity,
            &mut PersonAI,
            &mut crate::simulation::typed_task::ActionQueue,
            &AgentGoal,
            &LodLevel,
        ),
        Without<Drafted>,
    >,
) {
    query
        .par_iter_mut()
        .for_each(|(actor, mut ai, mut aq, goal, lod)| {
            if *lod == LodLevel::Dormant {
                return;
            }

            // Player command authority: skip stale-task reset entirely.
            // `dispatch_player_command_system` owns the task chain and
            // `player_command_lifecycle_system` owns teardown. Resetting
            // here would clobber the dispatch routed this tick.
            if *goal == AgentGoal::FollowingPlayerCommand {
                return;
            }

            if ai.task_id != PersonAI::UNEMPLOYED {
                // Sleep dispatches via `htn_dispatch_system`, not a plan, so its
                // task is expected to outlive the plan reset. Everything else
                // is plan-driven — when an agent has no `ActivePlan`, any
                // lingering task is stale and gets cleared.
                let expected_task = match *goal {
                    AgentGoal::Sleep => Some(TaskKind::Sleep as u16),
                    AgentGoal::Survive if ai.task_id == TaskKind::Eat as u16 => {
                        Some(TaskKind::Eat as u16)
                    }
                    AgentGoal::Survive if ai.task_id == TaskKind::WithdrawFood as u16 => {
                        Some(TaskKind::WithdrawFood as u16)
                    }
                    // Phase 5c-ii-d-iii-ii: AcquireFood scavenge chain runs
                    // without an ActivePlan under Survive. The Scavenge head
                    // and trailing Eat both need to survive across
                    // goal-dispatch ticks until completion or external
                    // preempt.
                    AgentGoal::Survive if ai.task_id == TaskKind::Scavenge as u16 => {
                        Some(TaskKind::Scavenge as u16)
                    }
                    // Phase 5c-ii-b: AcquireGood haul chain runs without an
                    // ActivePlan. Both legs (storage withdraw + haul to
                    // blueprint) need to survive across goal-dispatch ticks
                    // until either completion or external preempt.
                    AgentGoal::Haul if ai.task_id == TaskKind::WithdrawMaterial as u16 => {
                        Some(TaskKind::WithdrawMaterial as u16)
                    }
                    AgentGoal::Haul if ai.task_id == TaskKind::HaulMaterials as u16 => {
                        Some(TaskKind::HaulMaterials as u16)
                    }
                    // Phase 5e-ii: hunter-arm chain (`htn_equip_hunting_spear_dispatch_system`)
                    // runs without an ActivePlan; the dispatcher is now
                    // goal-agnostic (a Hunter under HuntOrder::Hunt may have
                    // any goal — Lead / Defend / Socialize / Survive / etc.).
                    // Preserve both legs of the chain across goal flips by
                    // keying on the reserved-resource (WithdrawMaterial leg)
                    // and the `Task::Equip { resource_id }` payload (Equip
                    // leg, by which point the reservation has been released
                    // in `finish_withdraw_material`). Weapon-specific so
                    // non-hunter WithdrawMaterial/Equip chains still
                    // respect their own goal preserve-arms below.
                    _ if ai.task_id == TaskKind::WithdrawMaterial as u16
                        && ai.reserved_resource
                            == Some(crate::economy::core_ids::weapon()) =>
                    {
                        Some(TaskKind::WithdrawMaterial as u16)
                    }
                    _ if ai.task_id == TaskKind::Equip as u16
                        && aq.current.as_equip().map(|(_slot, rid)| rid)
                            == Some(crate::economy::core_ids::weapon()) =>
                    {
                        Some(TaskKind::Equip as u16)
                    }
                    // Phase 5e-iii: HTN-driven ReturnSurplus chain runs without
                    // an ActivePlan under ReturnCamp. The DepositResource walk
                    // needs to survive across goal-dispatch ticks until
                    // `drop_items_at_destination_system` fires.
                    AgentGoal::ReturnCamp if ai.task_id == TaskKind::DepositResource as u16 => {
                        Some(TaskKind::DepositResource as u16)
                    }
                    // Phase 5e-iv: HTN-driven TameWildHorse chain runs without
                    // an ActivePlan under TameHorse. The TameAnimal walk +
                    // 100-tick adjacency work need to survive across
                    // goal-dispatch ticks until `tame_task_system` finalises.
                    AgentGoal::TameHorse if ai.task_id == TaskKind::TameAnimal as u16 => {
                        Some(TaskKind::TameAnimal as u16)
                    }
                    // Phase 5e-v: HTN-driven PlantFromStorage chain runs without
                    // an ActivePlan under Farm. Both legs (WithdrawMaterial +
                    // Planter) survive across goal-dispatch ticks until
                    // completion or external preempt — mirrors the method's
                    // `MF_UNINTERRUPTIBLE` and the dead legacy plans'
                    // `PF_NONE` (the legacy plans were never reachable
                    // anyway, so the new HTN path defines the contract).
                    AgentGoal::Farm if ai.task_id == TaskKind::WithdrawMaterial as u16 => {
                        Some(TaskKind::WithdrawMaterial as u16)
                    }
                    AgentGoal::Farm if ai.task_id == TaskKind::Planter as u16 => {
                        Some(TaskKind::Planter as u16)
                    }
                    // FarmFood (PlanId 1) → HTN migration: harvest chain
                    // (`HarvestMaturePlantForStorageMethod` emits `[Gather,
                    // DepositToFactionStorage]`) runs without an ActivePlan
                    // under Farm. Both legs need to survive across goal-dispatch
                    // ticks until completion or external preempt.
                    AgentGoal::Farm if ai.task_id == TaskKind::Gather as u16 => {
                        Some(TaskKind::Gather as u16)
                    }
                    AgentGoal::Farm if ai.task_id == TaskKind::DepositResource as u16 => {
                        Some(TaskKind::DepositResource as u16)
                    }
                    // Phase 5e-vi: HTN-driven ConstructBlueprint chain runs without
                    // an ActivePlan under Build. The Construct walk + on-site
                    // labor must survive across goal-dispatch ticks until
                    // `construction_system`'s pass-3 cleanup fires `aq.advance()`
                    // — mirrors the legacy `ClaimedBuild` plan's `PF_UNINTERRUPTIBLE`.
                    AgentGoal::Build if ai.task_id == TaskKind::Construct as u16 => {
                        Some(TaskKind::Construct as u16)
                    }
                    AgentGoal::Build if ai.task_id == TaskKind::ConstructBed as u16 => {
                        Some(TaskKind::ConstructBed as u16)
                    }
                    // ClearObstacle is a Build-prerequisite chain — preserve
                    // across goal-dispatch ticks the same way as Construct,
                    // so the worker stays committed until the obstacle is
                    // cleared (or the executor naturally advances).
                    AgentGoal::Build if ai.task_id == TaskKind::ClearObstacle as u16 => {
                        Some(TaskKind::ClearObstacle as u16)
                    }
                    // Pack labor: a worker dispatched to dismantle a
                    // shelter must stay on the task across goal flips.
                    // `goal_update_system` may re-evaluate while the
                    // worker walks (hunger / sleep / mobile-gate
                    // demote) and `mobile_state_goal_gate_system`
                    // doesn't include `FollowingPlayerCommand` /
                    // anything Build-shaped for Packed bands, so we
                    // pin the task here regardless of which goal the
                    // worker carries.
                    _ if ai.task_id == TaskKind::UnpitchStructure as u16 => {
                        Some(TaskKind::UnpitchStructure as u16)
                    }
                    // Phase 5e-xiii-a: HTN-driven personal-blueprint chain
                    // (`WithdrawAndHaulToPersonalBlueprintMethod`) runs without
                    // an ActivePlan under Build. Both legs (WithdrawMaterial +
                    // HaulMaterials) must survive across goal-dispatch ticks
                    // until completion or external preempt — mirrors the
                    // method's `MF_UNINTERRUPTIBLE` and the legacy
                    // `HaulFromStorageAndBuild` plan's `PF_UNINTERRUPTIBLE`.
                    AgentGoal::Build if ai.task_id == TaskKind::WithdrawMaterial as u16 => {
                        Some(TaskKind::WithdrawMaterial as u16)
                    }
                    AgentGoal::Build if ai.task_id == TaskKind::HaulMaterials as u16 => {
                        Some(TaskKind::HaulMaterials as u16)
                    }
                    // Phase 5c-ii-c-ii: AcquireGood gather chain runs without
                    // an ActivePlan. Both legs (gather + deposit at faction
                    // storage) need to survive across goal-dispatch ticks
                    // until either completion or external preempt.
                    AgentGoal::GatherWood | AgentGoal::GatherStone
                        if ai.task_id == TaskKind::Gather as u16 =>
                    {
                        Some(TaskKind::Gather as u16)
                    }
                    AgentGoal::GatherWood | AgentGoal::GatherStone
                        if ai.task_id == TaskKind::DepositResource as u16 =>
                    {
                        Some(TaskKind::DepositResource as u16)
                    }
                    // Phase 5c-ii-d-ii-a: AcquireGood scavenge chain runs
                    // without an ActivePlan. Same lifecycle as the gather
                    // chain — Scavenge head, DepositResource tail.
                    AgentGoal::GatherWood | AgentGoal::GatherStone
                        if ai.task_id == TaskKind::Scavenge as u16 =>
                    {
                        Some(TaskKind::Scavenge as u16)
                    }
                    // Phase 5e-xiv: Stockpile chain — scavenge ambient ground
                    // items of any catalog resource the chief has posted
                    // (Skin etc.) and deposit at faction storage. Mirrors
                    // GatherWood/GatherStone preserve arms.
                    AgentGoal::Stockpile if ai.task_id == TaskKind::Scavenge as u16 => {
                        Some(TaskKind::Scavenge as u16)
                    }
                    AgentGoal::Stockpile if ai.task_id == TaskKind::Gather as u16 => {
                        Some(TaskKind::Gather as u16)
                    }
                    AgentGoal::Stockpile if ai.task_id == TaskKind::DepositResource as u16 => {
                        Some(TaskKind::DepositResource as u16)
                    }
                    AgentGoal::Stockpile if ai.task_id == TaskKind::Explore as u16 => {
                        Some(TaskKind::Explore as u16)
                    }
                    // Phase 5c-ii-d-iv-ii: HTN Explore fallback (`ExploreForFoodMethod`
                    // / `ExploreForMaterialMethod`) runs without an ActivePlan.
                    // The walk-to-random-tile leg shouldn't be reset every
                    // tick by the absence of a plan; the catch-all below
                    // handles arrival-then-Idle once `state == Working`.
                    AgentGoal::Survive if ai.task_id == TaskKind::Explore as u16 => {
                        Some(TaskKind::Explore as u16)
                    }
                    AgentGoal::GatherWood | AgentGoal::GatherStone
                        if ai.task_id == TaskKind::Explore as u16 =>
                    {
                        Some(TaskKind::Explore as u16)
                    }
                    // Phase 5c-ii-d-vi: HTN-driven StockpileFood chain runs
                    // without an ActivePlan under GatherFood. Scavenge head,
                    // DepositResource tail, Explore fallback — all need to
                    // survive across goal-dispatch ticks. Mirrors the
                    // GatherWood/GatherStone arms above.
                    AgentGoal::GatherFood if ai.task_id == TaskKind::Scavenge as u16 => {
                        Some(TaskKind::Scavenge as u16)
                    }
                    AgentGoal::GatherFood if ai.task_id == TaskKind::DepositResource as u16 => {
                        Some(TaskKind::DepositResource as u16)
                    }
                    AgentGoal::GatherFood if ai.task_id == TaskKind::Explore as u16 => {
                        Some(TaskKind::Explore as u16)
                    }
                    // Phase 5e-viii-a: HTN-driven DeliverHuntKill chain runs
                    // without an ActivePlan after the truncated `HuntFood`
                    // plan completes at PickUp. Both legs (HaulCorpse +
                    // Butcher) survive across goal-dispatch ticks regardless
                    // of the agent's goal — `Carrying` is a per-agent
                    // obligation that takes precedence over need-driven goal
                    // flips. Mirrors the method's `MF_UNINTERRUPTIBLE` flag.
                    _ if ai.task_id == TaskKind::HaulCorpse as u16 => {
                        Some(TaskKind::HaulCorpse as u16)
                    }
                    _ if ai.task_id == TaskKind::Butcher as u16 => Some(TaskKind::Butcher as u16),
                    // Phase 5e-viii-b: HTN-driven EngagePrey runs without an
                    // ActivePlan after the truncated `HuntFood` plan completes
                    // at Travel. Hunt walks to prey + engages combat;
                    // PickUpCorpse retrieves a fresh kill. Both survive across
                    // dispatch ticks regardless of `AgentGoal` — chief's
                    // `HuntOrder::Hunt` is the standing obligation that
                    // overrides need-driven goal flips. Mirrors the methods'
                    // `MF_UNINTERRUPTIBLE` flag.
                    _ if ai.task_id == TaskKind::Hunter as u16 => Some(TaskKind::Hunter as u16),
                    _ if ai.task_id == TaskKind::PickUpCorpse as u16 => {
                        Some(TaskKind::PickUpCorpse as u16)
                    }
                    // Phase 5e-viii-c: HTN-driven JoinHuntParty's Muster leg
                    // runs without an ActivePlan. The Travel leg uses
                    // TaskKind::Explore which is already preserved by the
                    // existing Survive / GatherFood arms above (and the
                    // catch-all below flips Idle on arrival). Goal-agnostic
                    // because the chief's HuntOrder::Hunt is a faction-level
                    // obligation that overrides need-driven goal flips.
                    _ if ai.task_id == TaskKind::HuntPartyMuster as u16 => {
                        Some(TaskKind::HuntPartyMuster as u16)
                    }
                    // Phase 5e-ix: HTN-driven Socialize runs without an
                    // ActivePlan under the Socialize goal. The single
                    // Socialize task survives across goal-dispatch ticks
                    // until `goal_update_system` flips the agent off
                    // `AgentGoal::Socialize` (typically when needs.social
                    // has dropped enough to defuse the trigger).
                    // Phase 5e-xi-a: HTN-driven DeliverMaterialToCraftOrder
                    // chain runs without an ActivePlan under Craft. Both legs
                    // (WithdrawMaterial + HaulToCraftOrder) survive across
                    // goal-dispatch ticks until completion or external preempt
                    // — mirrors the legacy plan's `PF_UNINTERRUPTIBLE`.
                    AgentGoal::Craft if ai.task_id == TaskKind::WithdrawMaterial as u16 => {
                        Some(TaskKind::WithdrawMaterial as u16)
                    }
                    AgentGoal::Craft if ai.task_id == TaskKind::HaulToCraftOrder as u16 => {
                        Some(TaskKind::HaulToCraftOrder as u16)
                    }
                    // Phase 5e-xi-b: HTN-driven WorkOnCraftOrder chain runs
                    // without an ActivePlan under Craft. WorkOnCraftOrder labors
                    // in place; the trailing DepositResource walks to faction
                    // storage and is finalised by drop_items_at_destination_system.
                    // Both must survive across goal-dispatch ticks until
                    // completion or external preempt — mirrors the legacy plan's
                    // `PF_UNINTERRUPTIBLE`.
                    AgentGoal::Craft if ai.task_id == TaskKind::WorkOnCraftOrder as u16 => {
                        Some(TaskKind::WorkOnCraftOrder as u16)
                    }
                    AgentGoal::Craft if ai.task_id == TaskKind::DepositResource as u16 => {
                        Some(TaskKind::DepositResource as u16)
                    }
                    // Phase 5e-xi-c: HTN-driven HarvestGrainForCraftOrder
                    // chain runs without an ActivePlan under Craft. Gather +
                    // HaulToCraftOrder must survive across goal-dispatch ticks.
                    AgentGoal::Craft if ai.task_id == TaskKind::Gather as u16 => {
                        Some(TaskKind::Gather as u16)
                    }
                    // Phase 5e-xii-a: HTN-driven Play (PlaySocial / PlaySolo)
                    // chain runs without an ActivePlan under the Play goal.
                    // Single Task::Play survives across goal-dispatch ticks
                    // until `play_system` finalises on duration / willpower
                    // or the goal flips off Play.
                    AgentGoal::Play if ai.task_id == TaskKind::Play as u16 => {
                        Some(TaskKind::Play as u16)
                    }
                    // Phase 5e-xii-b: HTN-driven `WithdrawAndThrowStonesAsPlayMethod`
                    // chain runs without an ActivePlan under Play. Both legs
                    // (WithdrawMaterial + PlayThrow) survive across
                    // goal-dispatch ticks until completion or external
                    // preempt — mirrors the method's `MF_UNINTERRUPTIBLE`.
                    AgentGoal::Play if ai.task_id == TaskKind::WithdrawMaterial as u16 => {
                        Some(TaskKind::WithdrawMaterial as u16)
                    }
                    AgentGoal::Play if ai.task_id == TaskKind::PlayThrow as u16 => {
                        Some(TaskKind::PlayThrow as u16)
                    }
                    // Phase 5e-xii-d: HTN-driven `WithdrawAndPlantGrainSeedAsPlayMethod`
                    // / `WithdrawAndPlantBerrySeedAsPlayMethod` chains run
                    // without an ActivePlan under Play. The trailing PlayPlant
                    // leg walks to the destination grass tile and plants;
                    // survives across goal-dispatch ticks until completion or
                    // external preempt — mirrors the methods' `MF_UNINTERRUPTIBLE`.
                    AgentGoal::Play if ai.task_id == TaskKind::PlayPlant as u16 => {
                        Some(TaskKind::PlayPlant as u16)
                    }
                    AgentGoal::Socialize if ai.task_id == TaskKind::Socialize as u16 => {
                        Some(TaskKind::Socialize as u16)
                    }
                    // Phase 5e-x: HTN-driven combat/faction tasks run
                    // without an ActivePlan. Each survives across
                    // goal-dispatch ticks while the goal stays the same;
                    // `goal_update_system` is what eventually peels the
                    // agent off (faction stops being under raid /
                    // raid_target clears / RescueTarget timeout / chief
                    // hunger).
                    AgentGoal::Raid if ai.task_id == TaskKind::Raid as u16 => {
                        Some(TaskKind::Raid as u16)
                    }
                    AgentGoal::Defend if ai.task_id == TaskKind::Defend as u16 => {
                        Some(TaskKind::Defend as u16)
                    }
                    AgentGoal::Lead if ai.task_id == TaskKind::Lead as u16 => {
                        Some(TaskKind::Lead as u16)
                    }
                    // RescueAlly's HTN method dispatches with
                    // TaskKind::Defend (mirrors the legacy step's
                    // task field) so the same arm shape covers it.
                    AgentGoal::Rescue if ai.task_id == TaskKind::Defend as u16 => {
                        Some(TaskKind::Defend as u16)
                    }
                    // P1: MigrateToCamp dispatcher emits a single
                    // Task::WalkTo + TaskKind::Migrate that survives
                    // across goal-dispatch ticks until
                    // `nomad_migration_arrival_system` strips the
                    // `MigrationTarget` component on chebyshev arrival
                    // or timeout. Mirrors the Lead/Defend long-walk shape.
                    AgentGoal::MigrateToCamp if ai.task_id == TaskKind::Migrate as u16 => {
                        Some(TaskKind::Migrate as u16)
                    }
                    // Phase D scout: preserve the Explore chain so the
                    // agent walks the full survey leg without
                    // goal_dispatch tearing it down between goal-update
                    // ticks (200 ticks = 10 s, scout walks may be 30-60s).
                    AgentGoal::Scout if ai.task_id == TaskKind::Explore as u16 => {
                        Some(TaskKind::Explore as u16)
                    }
                    _ => None,
                };

                if Some(ai.task_id) != expected_task {
                    // Phase 5 contract note: this branch fires both for
                    // genuine goal flips *and* for working chains whose
                    // (goal, task) pair is missing a preserve arm above
                    // (HTN re-dispatches the same chain next tick, so the
                    // chain still progresses). Recording `Abandoned` here
                    // would push every fire-and-redispatch cycle of a
                    // long-running chain into `MethodHistory` — a 600-
                    // tick walk would saturate the failure ring within a
                    // handful of ticks and the agent would bias against
                    // its own working method. The `Abandoned` outcome is
                    // therefore recorded by `goal_update_system` (the
                    // actual goal-flip site), not here.
                    // Goal changed or task is done; drop the current task.
                    // A pending gather claim must release here too: a goal
                    // flip preempts the chain before `finish_gather` runs.
                    release_gather_claim(&gather_claims, &mut ai, actor);
                    ai.state = AiState::Idle;
                    ai.task_id = PersonAI::UNEMPLOYED;
                    // Phase 4b-ii: a goal flip is an external preempt — any
                    // prefetched tasks belong to the abandoned plan. `cancel()`
                    // drops both `current` and the queue so executors with the
                    // inconsistent-state guard see clean state and chained
                    // follow-ups don't outlive their plan.
                    aq.cancel();
                }
            }

            // Catch-all for stale Explore tasks. Gather/Survive/Build share
            // the Explore plan, so when the goal flips and an Explore task is
            // still Working, drop it back to Idle so plan_execution_system
            // can pick a fresh plan next tick.
            //
            // Phase 5c-ii-d-iv-ii: HTN-dispatched Explore (`ExploreForFoodMethod`
            // / `ExploreForMaterialMethod`) lands here too on arrival. Calling
            // `aq.advance()` flips the typed channel to `Task::Idle` so the
            // next HTN dispatch tick re-evaluates with a fresh ctx (memory
            // populated en route may now reveal a concrete target).
            if !matches!(*goal, AgentGoal::Sleep)
                && ai.task_id == TaskKind::Explore as u16
                && ai.state == AiState::Working
            {
                ai.state = AiState::Idle;
                ai.task_id = PersonAI::UNEMPLOYED;
                aq.advance();
            }
        });
}

// ── Play task system ──────────────────────────────────────────────────────────
// Refills willpower for agents in the Play task. Two flavors, picked by the
// plan that dispatched the task:
//
// - Social play: target_entity is a Person. Both agents (initiator + partner)
//   gain willpower from this loop iteration when their partner is adjacent and
//   also in Play. Affinity goes both ways via a deferred RelationshipMemory pass
//   so the partner doesn't need to be playing for the bond to form.
//
// - Solo play: target_entity is None or a non-Person. Willpower scales by the
//   `entertainment_value` of the highest-rated good held in hands (or, fallback,
//   any adjacent ground item).
//
// The task ends when work_progress hits PLAY_DURATION_TICKS or willpower is
// near full. plan_execution_system observes the resulting Idle+UNEMPLOYED
// state and advances the plan.

use super::carry::Carrier;
use super::goals::Personality;
use super::person::Person;
use super::schedule::SimClock;

const PLAY_DURATION_TICKS: u32 = 100;
const WILLPOWER_PLAY_GAIN_PER_ENT: f32 = 0.5;
const SOCIAL_PLAY_PARTNER_VALUE: f32 = 30.0;
const SOCIAL_PLAY_FILL_RATE: f32 = 0.4; // social need drop per sec for both parties
const PLAY_FULL_WILLPOWER: f32 = 230.0;

fn highest_held_entertainment(carrier: &Carrier) -> u8 {
    let l = carrier
        .left
        .map(|s| s.item.resource_id.entertainment_value())
        .unwrap_or(0);
    let r = carrier
        .right
        .map(|s| s.item.resource_id.entertainment_value())
        .unwrap_or(0);
    l.max(r)
}

fn adjacent_ground_entertainment(
    spatial: &SpatialIndex,
    item_query: &Query<&GroundItem>,
    cur_tx: i32,
    cur_ty: i32,
) -> u8 {
    let mut best: u8 = 0;
    for dy in -1..=1i32 {
        for dx in -1..=1i32 {
            for &e in spatial.get(cur_tx + dx, cur_ty + dy) {
                if let Ok(item) = item_query.get(e) {
                    let v = item.item.resource_id.entertainment_value();
                    if v > best {
                        best = v;
                    }
                }
            }
        }
    }
    best
}

pub fn play_system(
    time: Res<Time>,
    clock: Res<SimClock>,
    spatial: Res<SpatialIndex>,
    person_query: Query<(), With<Person>>,
    transform_query: Query<&Transform>,
    item_query: Query<&GroundItem>,
    mut rel_query: Query<&mut RelationshipMemory>,
    mut query: Query<(
        Entity,
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut Needs,
        &Personality,
        &Carrier,
        &Transform,
        &LodLevel,
    )>,
) {
    let dt = time.delta_secs() * clock.scale_factor();

    // Pairs of (self, partner) where social play happened this tick. Applied
    // as a separate pass to update RelationshipMemory on both sides without
    // borrow-checker conflicts.
    let mut affinity_pairs: Vec<(Entity, Entity)> = Vec::new();

    for (entity, mut ai, mut aq, mut needs, personality, carrier, transform, lod) in
        query.iter_mut()
    {
        if *lod == LodLevel::Dormant {
            continue;
        }
        if ai.task_id != TaskKind::Play as u16 {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }

        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;

        let mut did_social = false;

        if let Some(partner) = ai.target_entity {
            if person_query.get(partner).is_ok() {
                if let Ok(pt) = transform_query.get(partner) {
                    let ptx = (pt.translation.x / TILE_SIZE).floor() as i32;
                    let pty = (pt.translation.y / TILE_SIZE).floor() as i32;
                    let dist = (ptx - cur_tx).abs() + (pty - cur_ty).abs();
                    if dist <= 1 {
                        let multiplier = if *personality == Personality::Loner {
                            0.3
                        } else {
                            1.0
                        };
                        let gain = SOCIAL_PLAY_PARTNER_VALUE
                            * WILLPOWER_PLAY_GAIN_PER_ENT
                            * dt
                            * multiplier;
                        needs.willpower = (needs.willpower + gain).clamp(0.0, 255.0);
                        needs.social =
                            (needs.social - SOCIAL_PLAY_FILL_RATE * 100.0 * dt).clamp(0.0, 255.0);
                        affinity_pairs.push((entity, partner));
                        did_social = true;
                    }
                }
            }
        }

        if !did_social {
            // Solo play: held item first, then any adjacent ground item.
            let mut ent_value = highest_held_entertainment(carrier);
            if ent_value == 0 {
                ent_value = adjacent_ground_entertainment(&spatial, &item_query, cur_tx, cur_ty);
            }
            if ent_value > 0 {
                let multiplier = if *personality == Personality::Loner {
                    1.5
                } else {
                    1.0
                };
                let gain = (ent_value as f32) * WILLPOWER_PLAY_GAIN_PER_ENT * dt * multiplier;
                needs.willpower = (needs.willpower + gain).clamp(0.0, 255.0);
            }
            // If neither held nor adjacent items exist, the agent stands here
            // and the task ends via the duration cap below — no infinite loop.
        }

        ai.work_progress = ai.work_progress.saturating_add(1);
        if ai.work_progress as u32 >= PLAY_DURATION_TICKS || needs.willpower >= PLAY_FULL_WILLPOWER
        {
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            ai.target_entity = None;
            ai.work_progress = 0;
            // Phase 5e-xii-a: drain the typed channel so HTN-driven Play
            // chains complete cleanly. Legacy plan-driven flows leave
            // `aq.current = Idle`, so this is a benign no-op there.
            aq.advance();
        }
    }

    // Bilateral affinity gain — applies even if the partner isn't playing, so
    // playing with someone who happens to be standing there still strengthens
    // the bond from both directions over time.
    for (a, b) in affinity_pairs {
        if let Ok(mut rel) = rel_query.get_mut(a) {
            rel.update(b, 1);
        }
        if let Ok(mut rel) = rel_query.get_mut(b) {
            rel.update(a, 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathfinding::chunk_graph::ChunkGraph;
    use crate::world::chunk::{Chunk, ChunkCoord, ChunkMap, CHUNK_SIZE};
    use crate::world::tile::{TileData, TileKind};

    fn flat_map_with_chunk(coord: ChunkCoord, surf_z: i8) -> ChunkMap {
        let mut map = ChunkMap::default();
        let surface_z = Box::new([[surf_z; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_kind = Box::new([[TileKind::Grass; CHUNK_SIZE]; CHUNK_SIZE]);
        let surface_fertility = Box::new([[0u8; CHUNK_SIZE]; CHUNK_SIZE]);
        map.0.insert(
            coord,
            Chunk::new(surface_z, surface_kind, surface_fertility),
        );
        map
    }

    #[test]
    fn assign_task_uses_underground_target_z_when_agent_is_below_surface() {
        // Hill chunk, surface_z = 5. Carve a tunnel cell at (10, 10, 0):
        // floor Dirt at z=0, headspace Air at z=1.
        let coord = ChunkCoord(0, 0);
        let mut map = flat_map_with_chunk(coord, 5);
        map.set_tile(
            10,
            10,
            0,
            TileData {
                kind: TileKind::Dirt,
                ..Default::default()
            },
        );
        map.set_tile(
            10,
            10,
            1,
            TileData {
                kind: TileKind::Air,
                ..Default::default()
            },
        );
        // Agent already standing in the tunnel.
        let mut ai = PersonAI::default();
        ai.current_z = 0;

        let graph = ChunkGraph::default();
        let router = ChunkRouter::default();
        let conn = ChunkConnectivity::default();
        assign_task_with_routing(
            &mut ai,
            (10, 10),
            coord,
            (10, 10),
            TaskKind::Idle,
            None,
            &graph,
            &router,
            &map,
            &conn,
        );

        // target_z must follow the agent's Z (the tunnel floor at z=0),
        // not the surface above (z=5). Without the fix, this would be 5
        // and the flow field would route at the surface, stranding the
        // agent in the tunnel.
        assert_eq!(ai.target_z, 0);
    }
}
