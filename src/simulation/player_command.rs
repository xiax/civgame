//! Player-command authority layer.
//!
//! Commit 1 (this file) — plumbing only, no behavior change. Coexists with the
//! legacy `PlayerOrder` path. Subsequent commits route the UI and HTN through
//! these types and retire `PlayerOrder` + the 28 scattered `Without<PlayerOrder>`
//! filters in favor of goal forcing.
//!
//! Design rationale lives in `~/.claude/plans/player-orders-and-drafting-resilient-key.md`.
//! Mirrors `MigrationTarget` (in `nomad.rs`) — sim-owned marker, dedicated
//! dispatcher, lifecycle system, preserve-arms in `goal_dispatch_system`.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;

use crate::pathfinding::chunk_graph::ChunkGraph;
use crate::pathfinding::chunk_router::ChunkRouter;
use crate::pathfinding::connectivity::ChunkConnectivity;
use crate::pathfinding::hotspots::{HotspotFlowFields, HotspotKind};
use crate::simulation::combat::CombatTarget;
use crate::simulation::construction::{Blueprint, BlueprintMap, BuildSiteKind};
use crate::simulation::corpse::Carrying;
use crate::simulation::faction::{FactionMember, SOLO};
use crate::simulation::items::TargetItem;
use crate::simulation::military::{MilitaryFormationSlot, PendingFormationSlots};
use crate::simulation::person::{AiState, Drafted, PersonAI, UNEMPLOYED_TASK_KIND};
use crate::simulation::tasks::{assign_task_with_routing, TaskKind};
use crate::simulation::technology::TechId;
use crate::simulation::typed_task::{ActionQueue, Task, WalkReason};
use crate::world::chunk::{ChunkCoord, ChunkMap, CHUNK_SIZE};
use crate::world::terrain::{tile_to_world, TILE_SIZE};

/// One UI-issued command targeting one or more actors. UI systems emit these;
/// the sim owns translation to typed tasks. UI never mutates `PersonAI` /
/// `ActionQueue` / `Commanded` directly.
///
/// Multi-actor commands (drag-select muster, group move) carry every actor in
/// `actors`; the sim attaches a `Commanded` to each.
#[derive(Event, Debug, Clone)]
pub struct PlayerCommandEvent {
    pub actors: Vec<Entity>,
    pub command: PlayerCommand,
}

/// What the player wants done. Variants embed all spatial / target data so the
/// dispatcher doesn't need to look anything up at translation time.
///
/// Adding a new order is a four-step change:
///   1. New variant here.
///   2. Match arm in `dispatch_player_command_system`.
///   3. Match arm in `player_command_lifecycle_system` (terminal-state check).
///   4. UI button emits the event.
#[derive(Debug, Clone)]
pub enum PlayerCommand {
    Move {
        tile: (i32, i32),
        z: i8,
    },
    Gather {
        tile: (i32, i32),
        z: i8,
    },
    Mine {
        tile: (i32, i32),
        z: i8,
    },
    Build {
        kind: BuildSiteKind,
        tile: (i32, i32),
        z: i8,
    },
    Deconstruct {
        tile: (i32, i32),
        z: i8,
    },
    DigDown {
        tile: (i32, i32),
        z: i8,
    },
    PickUpItem {
        item: Entity,
        tile: (i32, i32),
        z: i8,
    },
    PickUpCorpse {
        corpse: Entity,
        tile: (i32, i32),
        z: i8,
    },
    AttackEntity {
        foe: Entity,
        tile: (i32, i32),
        z: i8,
    },
    Teach {
        student: Entity,
        tile: (i32, i32),
        z: i8,
    },
    HoldLecture {
        tech: TechId,
    },
    ReadItem {
        tech: TechId,
    },
    EncodeTablet {
        tech: TechId,
    },
    /// Promote `actors` to military mode (`Drafted`). UI enumerates which
    /// agents to draft (e.g. all player-faction Hunters for the HUD button,
    /// or the current selection for the R-key toggle).
    Muster,
    /// Remove the `Drafted` marker from `actors` and idle their tasks.
    Disband,
    /// Group route order for already-drafted units.
    MilitaryMove {
        tile: (i32, i32),
        z: i8,
    },
    MilitaryAttack {
        foe: Entity,
        tile: (i32, i32),
        z: i8,
    },
    /// Tear down the actor's faction's camp shelters at the current
    /// `home_tile`, drop refunds, pack `Deployable` shelters into pack
    /// animals / member inventories, and flip the faction to
    /// `CampState::Packed`. `home_tile` is **not** changed — the band
    /// becomes mobile at its current location. The chief is the
    /// canonical actor.
    PackCamp,
    /// Validate `tile` (passable, reachable from band centroid),
    /// re-seed nomadic camp at the new tile, flip faction to
    /// `CampState::Pitched`, update `home_tile = tile`, and stamp
    /// `ForceGoalReevaluate` on every faction member so they re-pick
    /// goals against the fresh home next tick.
    PitchCamp {
        tile: (i32, i32),
        z: i8,
    },
    /// Phase 2: dispatch a single member of the actor's faction as a
    /// manual scout. `direction` is one of 8 cardinals (0=N, 1=NE, 2=E,
    /// ...). The scout walks `range` chebyshev tiles toward that
    /// cardinal and folds the local cluster summary into
    /// `FactionData.candidate_sites` on arrival.
    SendScout {
        direction: u8,
        range: u32,
    },
    /// Phase 3: set the active migration intent for the actor's
    /// faction. Reweights `pick_migration_target`'s component scores
    /// on the next survey / candidate refresh.
    SetMigrationIntent {
        intent: crate::simulation::faction::MigrationIntent,
    },
    /// Player-locked migration: set the packed-autonomy mode for the
    /// actor's faction. `Hold` keeps workers idle ("Awaiting Orders");
    /// `Forage` releases them to the existing `allowed_while_packed`
    /// autonomous behaviour. `PackCamp` resets the field to `Hold`.
    SetPackedAutonomy {
        mode: crate::simulation::faction::PackedMigrationAutonomy,
    },
    /// Faction-level: queue a vehicle of the given design for assembly at
    /// the player faction's `VehicleYard`. Applied directly in
    /// `drain_player_command_events_system` (empty `actors`) — the
    /// `vehicle_assembly_system` drains the queue.
    QueueVehicle {
        design_id: u32,
    },
    /// Faction-level: register a freeform design built in the vehicle
    /// designer, then queue it for assembly. `drain_player_command_events_system`
    /// inserts the design into `VehicleDesignRegistry` (assigning a fresh id)
    /// and pushes it onto `VehicleAssemblyQueue` for the player faction.
    QueueCustomVehicle {
        name: String,
        grid: crate::simulation::vehicle::VehicleGrid,
        purpose: crate::simulation::vehicle::VehiclePurpose,
        required_animals: u8,
    },
    /// Faction-level: a player right-click order targeting one spawned
    /// `Vehicle` entity (move / load / unload / right / crew / hitch /
    /// deconstruct). `drain_player_command_events_system` pushes it onto
    /// `PendingVehicleOps`; `vehicle_player_command_system` applies it.
    /// Empty `actors` — the vehicle, not a worker, is the subject.
    VehicleOrder {
        vehicle: Entity,
        kind: crate::simulation::vehicle::VehicleOrderKind,
    },
    /// Stand on a tile and hold; extends vision to
    /// `LOOKOUT_VIEW_RADIUS` via `ActiveLookout`. Manual / indefinite —
    /// the worker holds until the player issues a new command or
    /// movement drifts off the anchor. See `plans/lookout-base.md`.
    Lookout {
        tile: (i32, i32),
        z: i8,
    },
}

/// Per-actor authority marker. Replaces `PlayerOrder` once Commit 3 lands.
///
/// Lifecycle: `Pending` (event drained, dispatch pending this tick) →
/// `Active` (routed, executing) → terminal (`Completed` / `Failed` /
/// `Superseded`). One tick after terminal, `reap_terminal_commands_system`
/// strips the component and the agent re-enters autonomy.
#[derive(Component, Debug)]
pub struct Commanded {
    pub command: PlayerCommand,
    pub status: CommandStatus,
    pub issued_tick: u32,
    /// Monotonic id stamped at drain. UI can match HUD feedback to issuance,
    /// and supersession can identify "is this the same order I sent?".
    pub command_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    Pending,
    Active,
    Completed,
    Failed(CommandFailure),
    Superseded,
}

impl CommandStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            CommandStatus::Completed | CommandStatus::Failed(_) | CommandStatus::Superseded
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandFailure {
    /// Routing rejected the target — no reachable adjacent tile, or the
    /// target sits in a different connectivity component than the actor.
    Unreachable,
    /// The target entity (item / corpse / foe / blueprint) was already gone
    /// when the dispatcher attempted to act.
    TargetGone,
    /// The actor isn't eligible for this command kind (e.g. `Muster` on a
    /// non-Hunter; `Teach` with no teachable techs).
    Ineligible,
}

/// Monotonic command id source. Wraps at u32::MAX (~4B commands — never).
#[derive(Resource, Default)]
pub struct PlayerCommandIdGen {
    pub next: u32,
}

impl PlayerCommandIdGen {
    pub fn allocate(&mut self) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }
}

/// Drain `PlayerCommandEvent`s and stamp `Commanded { status: Pending }` on
/// every actor. Supersedes any prior `Commanded` on the same actor (status
/// flips to `Superseded`; `reap_terminal_commands_system` strips it next tick).
///
/// Faction-level commands with an empty `actors` list (currently only
/// `EncodeTablet`) bypass the per-actor stamping and apply directly here.
///
/// Runs in `SimulationSet::Input` so the dispatcher (ParallelB) sees fresh
/// `Pending` markers the same FixedUpdate tick the event was emitted in.
pub fn drain_player_command_events_system(
    mut commands: Commands,
    mut reader: EventReader<PlayerCommandEvent>,
    clock: Res<crate::simulation::schedule::SimClock>,
    mut id_gen: ResMut<PlayerCommandIdGen>,
    mut existing: Query<&mut Commanded>,
    mut player_craft: ResMut<crate::simulation::jobs::PlayerCraftRequest>,
    player_faction: Res<crate::simulation::faction::PlayerFaction>,
    mut vehicle_queue: ResMut<crate::simulation::vehicle::VehicleAssemblyQueue>,
    mut vehicle_registry: ResMut<crate::simulation::vehicle::VehicleDesignRegistry>,
    vehicle_data: Res<crate::simulation::vehicle::VehicleData>,
    mut pending_vehicle_ops: ResMut<crate::simulation::vehicle::PendingVehicleOps>,
) {
    let now = clock.tick as u32;
    for ev in reader.read() {
        // Faction-level applies (no actors needed).
        if ev.actors.is_empty() {
            match ev.command {
                PlayerCommand::EncodeTablet { tech } => {
                    if player_craft.0.is_none() {
                        player_craft.0 =
                            Some((crate::simulation::crafting::RECIPE_CLAY_TABLET, Some(tech)));
                    }
                }
                PlayerCommand::QueueVehicle { design_id } => {
                    vehicle_queue.entries.push((
                        player_faction.faction_id,
                        crate::simulation::vehicle::VehicleDesignId(design_id),
                    ));
                }
                PlayerCommand::QueueCustomVehicle {
                    ref name,
                    ref grid,
                    purpose,
                    required_animals,
                } => {
                    use crate::simulation::vehicle::{
                        collect_design_tech_gates, VehicleDesign, VehicleDesignId,
                    };
                    // Tech gates are derived from every placed variant /
                    // module / part — a custom design is gated like a stock
                    // template so `vehicle_assembly_system` can enforce them.
                    let tech_gates = collect_design_tech_gates(
                        grid,
                        std::iter::empty(),
                        &vehicle_data,
                    );
                    let id = vehicle_registry.insert(VehicleDesign {
                        id: VehicleDesignId(0), // reassigned by `insert`
                        name: name.clone(),
                        grid: grid.clone(),
                        allowed_purpose: purpose,
                        required_animals,
                        tech_gates,
                        author_faction: Some(player_faction.faction_id),
                        revision: 0,
                    });
                    vehicle_queue
                        .entries
                        .push((player_faction.faction_id, id));
                }
                PlayerCommand::VehicleOrder { vehicle, kind } => {
                    pending_vehicle_ops.ops.push((vehicle, kind));
                }
                _ => {
                    // Other commands need actors; skip a malformed event.
                }
            }
            continue;
        }
        for &actor in &ev.actors {
            // Mark any existing command on this actor as superseded so the
            // reap pass strips it next tick. The dispatcher won't act on a
            // superseded command; the new `Pending` (inserted below) wins.
            if let Ok(mut prior) = existing.get_mut(actor) {
                if !prior.status.is_terminal() {
                    prior.status = CommandStatus::Superseded;
                }
            }
            let id = id_gen.allocate();
            commands.entity(actor).insert(Commanded {
                command: ev.command.clone(),
                status: CommandStatus::Pending,
                issued_tick: now,
                command_id: id,
            });
        }
    }
}

/// Removes `Commanded` one tick after it reaches terminal status. UI
/// consumers read `RemovedComponents<Commanded>` for HUD feedback ("Order
/// complete" / "Couldn't reach target").
///
/// Runs in `SimulationSet::Sequential`, late, so executors have a full tick
/// at terminal status.
pub fn reap_terminal_commands_system(mut commands: Commands, query: Query<(Entity, &Commanded)>) {
    for (entity, c) in query.iter() {
        if c.status.is_terminal() {
            commands.entity(entity).remove::<Commanded>();
            // Formation slot is transient — only meaningful while a
            // MilitaryMove order is active.
            if matches!(c.command, PlayerCommand::MilitaryMove { .. }) {
                commands.entity(entity).remove::<MilitaryFormationSlot>();
            }
        }
    }
}

/// Per-kind terminal-state detector. Runs after executors in Sequential. The
/// heuristic completion we used to live with (`state == Idle && task_id ==
/// UNEMPLOYED`) is replaced by explicit per-kind detection — Move arrival is
/// distinct from Gather completion is distinct from Build done. Each variant's
/// terminal condition lives in one place.
///
/// Adding a new order's completion = add a match arm here.
pub fn player_command_lifecycle_system(
    chunk_map: Res<crate::world::chunk::ChunkMap>,
    bp_query: Query<&crate::simulation::construction::Blueprint>,
    plant_query: Query<&crate::simulation::plants::Plant>,
    item_query: Query<(), With<crate::simulation::items::GroundItem>>,
    corpse_query: Query<&crate::simulation::corpse::Corpse>,
    health_query: Query<&crate::simulation::combat::Health>,
    bp_map: Res<crate::simulation::construction::BlueprintMap>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    mut q: Query<(
        Entity,
        &mut Commanded,
        &PersonAI,
        &crate::simulation::typed_task::ActionQueue,
        &Transform,
        Option<&FactionMember>,
    )>,
) {
    for (entity, mut cmd, ai, aq, transform, member) in q.iter_mut() {
        if cmd.status != CommandStatus::Active {
            continue;
        }
        let _ = entity;
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur = (cur_tx, cur_ty);

        let outcome = match cmd.command {
            PlayerCommand::Move { tile, .. } => {
                // Arrival within chebyshev 0 = same tile.
                if chebyshev(cur, tile) <= 0 {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::Gather { tile, .. } => {
                completion_when_gather_target_gone(tile, ai, aq, &plant_query, &chunk_map)
            }
            PlayerCommand::Mine { tile, .. } => {
                // Walls turn into floor / loose rock when mined. The mine
                // target is done when the tile is no longer a Wall/Stone.
                use crate::world::tile::TileKind;
                let kind = chunk_map.tile_kind_at(tile.0, tile.1);
                if !matches!(kind, Some(TileKind::Wall) | Some(TileKind::Stone)) {
                    Some(CommandStatus::Completed)
                } else {
                    completion_when_agent_idle(ai, aq)
                }
            }
            PlayerCommand::DigDown { tile, .. } => {
                // DigDown creates a Dirt floor (cave carved out). When the
                // dig finishes, the executor sets the agent Idle+UNEMPLOYED.
                let _ = tile;
                completion_when_agent_idle(ai, aq)
            }
            PlayerCommand::Deconstruct { tile, .. } => {
                // Deconstruction completes when the structure at the tile is
                // gone (StructureIndex entry would be removed). Fall back to
                // idle check.
                let _ = tile;
                completion_when_agent_idle(ai, aq)
            }
            PlayerCommand::Build { tile, .. } => {
                // Build completes when the blueprint at this tile is gone
                // (construction_system despawns it on completion).
                match bp_map.0.get(&tile) {
                    None => Some(CommandStatus::Completed),
                    Some(&bp_e) => {
                        if bp_query.get(bp_e).is_err() {
                            Some(CommandStatus::Completed)
                        } else {
                            None
                        }
                    }
                }
            }
            PlayerCommand::PickUpItem { item, .. } => {
                if item_query.get(item).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::PickUpCorpse { corpse, .. } => {
                if corpse_query.get(corpse).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::AttackEntity { foe, .. } => {
                if health_query.get(foe).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::Teach { .. }
            | PlayerCommand::HoldLecture { .. }
            | PlayerCommand::ReadItem { .. }
            | PlayerCommand::EncodeTablet { .. } => {
                // Knowledge orders have their own teardown via dedicated
                // teaching/encoding systems that strip the legacy
                // `PlayerOrder` marker. We mirror by completing when the
                // agent is idle and unemployed (the legacy completion rule).
                completion_when_agent_idle(ai, aq)
            }
            PlayerCommand::Muster | PlayerCommand::Disband => {
                // Muster / Disband are stamp-and-done: after the dispatcher
                // inserts/removes Drafted, the command's job is finished.
                Some(CommandStatus::Completed)
            }
            PlayerCommand::MilitaryMove { tile, .. } => {
                // Multi-actor formation moves route each unit to its own
                // slot tile; `ai.dest_tile` holds that slot (or the anchor
                // for single-actor moves). Completing on slot arrival
                // keeps the group spread out instead of letting one unit
                // mark the whole order done by stepping on the anchor.
                let _ = tile;
                if chebyshev(cur, ai.dest_tile) <= 1 {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::MilitaryAttack { foe, .. } => {
                if health_query.get(foe).is_err() {
                    Some(CommandStatus::Completed)
                } else {
                    None
                }
            }
            PlayerCommand::PackCamp => {
                let fid = member
                    .map(|m| registry.root_faction(m.faction_id))
                    .unwrap_or(SOLO);
                match registry.factions.get(&fid) {
                    Some(f)
                        if matches!(
                            f.camp_state,
                            crate::simulation::faction::CampState::Packed { .. }
                        ) =>
                    {
                        Some(CommandStatus::Completed)
                    }
                    _ => None,
                }
            }
            PlayerCommand::PitchCamp { tile, .. } => {
                let fid = member
                    .map(|m| registry.root_faction(m.faction_id))
                    .unwrap_or(SOLO);
                match registry.factions.get(&fid) {
                    Some(f)
                        if matches!(
                            f.camp_state,
                            crate::simulation::faction::CampState::Pitched
                        ) && f.home_tile == tile =>
                    {
                        Some(CommandStatus::Completed)
                    }
                    _ => None,
                }
            }
            // Phase 2/3: stamp-and-done faction commands.
            PlayerCommand::SendScout { .. }
            | PlayerCommand::SetMigrationIntent { .. }
            | PlayerCommand::SetPackedAutonomy { .. }
            | PlayerCommand::QueueVehicle { .. }
            | PlayerCommand::QueueCustomVehicle { .. }
            | PlayerCommand::VehicleOrder { .. } => Some(CommandStatus::Completed),
            // Lookout never auto-completes — the worker holds the anchor
            // until the player supersedes the command or `aq.cancel`
            // drops the chain via another dispatch.
            PlayerCommand::Lookout { .. } => None,
        };
        if let Some(new_status) = outcome {
            cmd.status = new_status;
        }
    }
}

fn completion_when_agent_idle(
    ai: &PersonAI,
    aq: &crate::simulation::typed_task::ActionQueue,
) -> Option<CommandStatus> {
    if ai.state == crate::simulation::person::AiState::Idle
        && aq.current_task_kind() == UNEMPLOYED_TASK_KIND
    {
        Some(CommandStatus::Completed)
    } else {
        None
    }
}

fn completion_when_gather_target_gone(
    tile: (i32, i32),
    ai: &PersonAI,
    aq: &crate::simulation::typed_task::ActionQueue,
    plant_query: &Query<&crate::simulation::plants::Plant>,
    chunk_map: &crate::world::chunk::ChunkMap,
) -> Option<CommandStatus> {
    use crate::world::tile::TileKind;
    // If the agent had a target_entity (plant) and it's gone, completed.
    if let Some(ent) = ai.target_entity {
        if plant_query.get(ent).is_err() {
            return Some(CommandStatus::Completed);
        }
    }
    // If the tile is no longer a gatherable kind, completed.
    let kind = chunk_map.tile_kind_at(tile.0, tile.1);
    if matches!(kind, Some(TileKind::Wall) | Some(TileKind::Stone)) {
        // Mining-like gather: completed when no longer Wall/Stone.
        return None;
    }
    completion_when_agent_idle(ai, aq)
}

fn chebyshev(a: (i32, i32), b: (i32, i32)) -> i32 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Routing resources bundled together (Bevy 16-param ceiling).
#[derive(SystemParam)]
pub struct CommandRouting<'w, 's> {
    pub chunk_map: Res<'w, ChunkMap>,
    pub chunk_graph: Res<'w, ChunkGraph>,
    pub chunk_router: Res<'w, ChunkRouter>,
    pub chunk_connectivity: Res<'w, ChunkConnectivity>,
    pub bp_map: ResMut<'w, BlueprintMap>,
    pub hotspots: ResMut<'w, HotspotFlowFields>,
    // sleepy-dove Phase 6: re-resolve the construction poster at dispatch
    // time (the pool may have shifted between menu open and command
    // processing). Author the manual blueprint with the resolved
    // poster's `design_techs` so it diffuses adoption like an
    // autonomous build.
    pub poster_pool: Res<'w, crate::simulation::construction::ConstructionPosterPool>,
    pub settlement_map: Res<'w, crate::simulation::settlement::SettlementMap>,
    #[system_param(ignore)]
    pub _marker: std::marker::PhantomData<&'s ()>,
}

/// Dispatch every `Commanded { status: Pending }`. Translates the kind into
/// the corresponding task chain in one match. Sets `status = Active` on
/// success, `status = Failed(...)` on routing failure.
///
/// Runs in `SimulationSet::ParallelB` after the drain system in `Input` so
/// fresh `Pending` markers route the same tick. Single source of truth for
/// player-command translation; UI never mutates AI state directly.
pub fn dispatch_player_command_system(
    mut commands: Commands,
    mut routing: CommandRouting,
    mut actors: Query<(
        Entity,
        &mut Commanded,
        &mut PersonAI,
        &mut ActionQueue,
        &Transform,
        Option<&FactionMember>,
    )>,
    mut target_item_q: Query<&mut TargetItem>,
    mut combat_target_q: Query<&mut CombatTarget>,
    mut player_craft: ResMut<crate::simulation::jobs::PlayerCraftRequest>,
    mut lecture_req: ResMut<crate::simulation::teaching::LectureRequest>,
    registry: Res<crate::simulation::faction::FactionRegistry>,
    mut camp_ops: ResMut<crate::simulation::nomad::PendingCampOps>,
    pending_slots: Res<PendingFormationSlots>,
) {
    for (actor, mut cmd, mut ai, mut aq, transform, member) in actors.iter_mut() {
        if cmd.status != CommandStatus::Pending {
            continue;
        }
        let cur_tx = (transform.translation.x / TILE_SIZE).floor() as i32;
        let cur_ty = (transform.translation.y / TILE_SIZE).floor() as i32;
        let cur_chunk = ChunkCoord(
            cur_tx.div_euclid(CHUNK_SIZE as i32),
            cur_ty.div_euclid(CHUNK_SIZE as i32),
        );
        let faction_id = member.map(|m| m.faction_id).unwrap_or(SOLO);

        // Player commands are external preempts. Drop any prior typed-task
        // chain so the new task promotes immediately. Camp commands are an
        // exception — they leave the actor's existing chain alone (the
        // chief keeps doing whatever they were doing while the apply system
        // mutates the world next Sequential tick).
        if !matches!(
            cmd.command,
            PlayerCommand::PackCamp
                | PlayerCommand::PitchCamp { .. }
                | PlayerCommand::SendScout { .. }
                | PlayerCommand::SetMigrationIntent { .. }
                | PlayerCommand::SetPackedAutonomy { .. }
        ) {
            aq.cancel();
        }
        // A fresh MilitaryMove supersedes any prior formation slot the
        // actor was holding — drop it so the new dispatch starts clean.
        if matches!(cmd.command, PlayerCommand::MilitaryMove { .. }) {
            commands.entity(actor).remove::<MilitaryFormationSlot>();
        }

        let outcome = dispatch_one(
            actor,
            &cmd.command,
            (cur_tx, cur_ty),
            cur_chunk,
            faction_id,
            &mut ai,
            &mut aq,
            &mut routing,
            &mut target_item_q,
            &mut combat_target_q,
            &mut player_craft,
            &mut lecture_req,
            &mut commands,
            &registry,
            &mut camp_ops,
            &pending_slots,
        );

        match outcome {
            DispatchOutcome::Active => {
                cmd.status = CommandStatus::Active;
            }
            DispatchOutcome::Failed(reason) => {
                cmd.status = CommandStatus::Failed(reason);
            }
        }
    }
}

enum DispatchOutcome {
    Active,
    Failed(CommandFailure),
}

#[allow(clippy::too_many_arguments)]
fn dispatch_one(
    actor: Entity,
    command: &PlayerCommand,
    cur_tile: (i32, i32),
    cur_chunk: ChunkCoord,
    faction_id: u32,
    ai: &mut PersonAI,
    aq: &mut ActionQueue,
    routing: &mut CommandRouting,
    target_item_q: &mut Query<&mut TargetItem>,
    combat_target_q: &mut Query<&mut CombatTarget>,
    player_craft: &mut crate::simulation::jobs::PlayerCraftRequest,
    lecture_req: &mut crate::simulation::teaching::LectureRequest,
    commands: &mut Commands,
    registry: &crate::simulation::faction::FactionRegistry,
    camp_ops: &mut crate::simulation::nomad::PendingCampOps,
    pending_slots: &PendingFormationSlots,
) -> DispatchOutcome {
    use PlayerCommand::*;
    match *command {
        Move { tile, z } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Idle,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            ai.target_z = z;
            DispatchOutcome::Active
        }
        Gather { tile, .. } | Mine { tile, .. } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Gather,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::Gather { tile });
            DispatchOutcome::Active
        }
        DigDown { tile, .. } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Dig,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::Dig { tile });
            DispatchOutcome::Active
        }
        Deconstruct { tile, .. } => {
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Deconstruct,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::Deconstruct { tile });
            DispatchOutcome::Active
        }
        PickUpItem { item, tile, .. } => {
            if commands.get_entity(item).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Scavenge,
                Some(item),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            if let Ok(mut ti) = target_item_q.get_mut(actor) {
                ti.0 = Some(item);
            }
            aq.dispatch(Task::Scavenge { target: item });
            DispatchOutcome::Active
        }
        PickUpCorpse { corpse, tile, .. } => {
            if commands.get_entity(corpse).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::PickUpCorpse,
                Some(corpse),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::PickUpCorpse { corpse });
            DispatchOutcome::Active
        }
        AttackEntity { foe, tile, .. } => {
            if commands.get_entity(foe).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::MilitaryAttack,
                Some(foe),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            if let Ok(mut ct) = combat_target_q.get_mut(actor) {
                ct.0 = None;
            }
            DispatchOutcome::Active
        }
        Build { kind, tile, z: _z } => {
            // sleepy-dove Phase 6: re-resolve the poster at execution
            // time. The worker is the executor; authority comes from the
            // faction chief / settlement architect. No poster knows the
            // construction tech → reject (the right-click menu greys
            // these out; this guards the race where the pool shifted
            // between menu open and command processing). The author-less
            // fallback only applies to fixture factions with no chief
            // knowledge at all (empty pool entry).
            let settlement_id = routing.settlement_map.first_for_faction(faction_id);
            let resolved_author = routing
                .poster_pool
                .select_poster_for_kind(faction_id, settlement_id, kind)
                .map(|cap| cap.author());
            let author_missing_but_pool_populated = resolved_author.is_none()
                && (routing
                    .poster_pool
                    .chief_by_faction
                    .contains_key(&faction_id)
                    || settlement_id
                        .map(|sid| {
                            routing
                                .poster_pool
                                .by_settlement
                                .contains_key(&(faction_id, sid))
                        })
                        .unwrap_or(false));
            if author_missing_but_pool_populated {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            // Spawn the personal blueprint if there isn't one at this tile.
            let bp_entity = if let Some(&e) = routing.bp_map.0.get(&tile) {
                Some(e)
            } else {
                let wp = tile_to_world(tile.0, tile.1);
                let bz = routing.chunk_map.surface_z_at(tile.0, tile.1) as i8;
                let mut bp = Blueprint::new(faction_id, Some(actor), kind, tile, bz)
                    .with_author(resolved_author);
                // Water-anchored blueprints (Bridge) need a passable bank
                // tile so workers don't try to path onto the river anchor.
                if bp.kind.is_water_anchored() {
                    bp.work_stand = crate::simulation::construction::work_stand_for_bridge(
                        &routing.chunk_map,
                        tile,
                        &routing.bp_map,
                    );
                    if bp.work_stand.is_none() {
                        return DispatchOutcome::Failed(CommandFailure::Unreachable);
                    }
                }
                let bp_e = commands
                    .spawn((
                        bp,
                        Transform::from_xyz(wp.x, wp.y, 0.3),
                        GlobalTransform::default(),
                        Visibility::Visible,
                        InheritedVisibility::default(),
                    ))
                    .id();
                routing.bp_map.0.insert(tile, bp_e);
                Some(bp_e)
            };
            let task_kind = if matches!(kind, BuildSiteKind::Bed) {
                TaskKind::ConstructBed
            } else {
                TaskKind::Construct
            };
            // For water-anchored blueprints route to the bank, not the
            // impassable anchor tile.
            let routing_tile = if kind.is_water_anchored() {
                crate::simulation::construction::work_stand_for_bridge(
                    &routing.chunk_map,
                    tile,
                    &routing.bp_map,
                )
                .unwrap_or(tile)
            } else {
                tile
            };
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                routing_tile,
                task_kind,
                bp_entity,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            if let Some(bp) = bp_entity {
                if matches!(kind, BuildSiteKind::Bed) {
                    aq.dispatch(Task::ConstructBed { blueprint: bp });
                } else {
                    aq.dispatch(Task::Construct { blueprint: bp });
                }
            }
            DispatchOutcome::Active
        }
        Teach { student, tile, .. } => {
            if commands.get_entity(student).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Teach,
                Some(student),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            // Adjacency markers (`TeachingPair` / `BeingTaught`) are inserted
            // by `apply_teach_order_system` once the teacher arrives — keyed
            // off `aq.current_task_kind() == TaskKind::Teach`, which `assign_task_with_routing`
            // just set. No legacy marker required.
            DispatchOutcome::Active
        }
        ReadItem { tech } => {
            // Pin the agent in place and stamp the Read task. `read_task_system`
            // accumulates study progress against the matching tablet/book in
            // the agent's inventory.
            ai.state = AiState::Working;
            ai.work_progress = 0;
            aq.dispatch(Task::Read { tech });
            commands.entity(actor).insert(Drafted);
            DispatchOutcome::Active
        }
        EncodeTablet { tech } => {
            // Faction-level request: post a craft contract for a tablet
            // encoding this tech. `chief_tablet_posting_system` drains.
            if player_craft.0.is_none() {
                player_craft.0 =
                    Some((crate::simulation::crafting::RECIPE_CLAY_TABLET, Some(tech)));
            }
            DispatchOutcome::Active
        }
        HoldLecture { tech } => {
            // Faction-level request: a `LectureRequest` directs
            // `apply_lecture_request_system` to draft nearby adults and
            // start the lecture session anchored on this actor.
            lecture_req.0 = Some((actor, tech));
            DispatchOutcome::Active
        }
        Muster => {
            // Promote this actor to military mode. UI determined eligibility
            // (player faction + Hunter profession for the HUD button; current
            // selection for the R-key toggle). Sim resets task state and
            // attaches `Drafted`. Carrying / reservations cleared so the
            // unit goes military-clean. `aq.cancel()` drops any in-flight
            // task chain (Forage, Build, etc.) — without it the queue holds
            // a stale task that resumes when `Disband` later removes the
            // `Drafted` marker.
            aq.cancel();
            ai.state = AiState::Idle;
            ai.target_entity = None;
            ai.work_progress = 0;
            commands.entity(actor).remove::<Carrying>().insert(Drafted);
            DispatchOutcome::Active
        }
        Disband => {
            // Inverse of Muster. Removes `Drafted` and idles tasks. Also
            // drops the typed queue so a stale military-side task can't bleed
            // back into autonomous execution.
            aq.cancel();
            ai.state = AiState::Idle;
            ai.target_entity = None;
            commands.entity(actor).remove::<Drafted>();
            if let Ok(mut ct) = combat_target_q.get_mut(actor) {
                ct.0 = None;
            }
            DispatchOutcome::Active
        }
        MilitaryMove { tile, z } => {
            // Per-actor slot for multi-actor dispatches; falls back to the
            // anchor for single-actor moves (absent from the side-table).
            let (slot_tile, slot_meta) = match pending_slots.map.get(&actor) {
                Some(None) => {
                    return DispatchOutcome::Failed(CommandFailure::Unreachable);
                }
                Some(Some(a)) => (a.slot_tile, Some(*a)),
                None => (tile, None),
            };
            // Anchor (not slot) seeds the rally-point flow field so the
            // whole group shares one hotspot.
            routing
                .hotspots
                .register((tile.0, tile.1, z), HotspotKind::RallyPoint);
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                slot_tile,
                TaskKind::MilitaryMove,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            ai.target_z = z;
            aq.dispatch(Task::WalkTo {
                tile: slot_tile,
                z,
                why: WalkReason::MilitaryMove,
            });
            if let Ok(mut ct) = combat_target_q.get_mut(actor) {
                ct.0 = None;
            }
            if let Some(m) = slot_meta {
                commands.entity(actor).insert(MilitaryFormationSlot {
                    anchor: tile,
                    slot_index: m.slot_index,
                    group: m.group,
                });
            }
            DispatchOutcome::Active
        }
        MilitaryAttack { foe, tile, z } => {
            if commands.get_entity(foe).is_none() {
                return DispatchOutcome::Failed(CommandFailure::TargetGone);
            }
            routing
                .hotspots
                .register((tile.0, tile.1, z), HotspotKind::RallyPoint);
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::MilitaryAttack,
                Some(foe),
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            aq.dispatch(Task::MilitaryAttack { foe });
            if let Ok(mut ct) = combat_target_q.get_mut(actor) {
                ct.0 = None;
            }
            DispatchOutcome::Active
        }
        PackCamp => {
            let _ = (target_item_q, lecture_req, player_craft);
            // Resolve actor's faction; nomadic + Pitched required.
            let fid = registry.root_faction(faction_id);
            let Some(faction) = registry.factions.get(&fid) else {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            };
            if !faction.caps.home.is_mobile() {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            if matches!(
                faction.camp_state,
                crate::simulation::faction::CampState::Packed { .. }
            ) {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            // Idempotent enqueue (same faction queued twice in one tick
            // is fine — apply system uses the first entry).
            if !camp_ops.packs.iter().any(|(f, _)| *f == fid) {
                camp_ops.packs.push((fid, faction.home_tile));
            }
            DispatchOutcome::Active
        }
        SendScout { direction, range } => {
            let _ = (target_item_q, lecture_req, player_craft);
            let fid = registry.root_faction(faction_id);
            let Some(faction) = registry.factions.get(&fid) else {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            };
            if !faction.caps.home.is_mobile() {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            if !camp_ops
                .manual_scouts
                .iter()
                .any(|s| s.fid == fid && s.direction == direction)
            {
                camp_ops
                    .manual_scouts
                    .push(crate::simulation::nomad::PendingManualScout {
                        fid,
                        direction,
                        range,
                    });
            }
            DispatchOutcome::Active
        }
        SetMigrationIntent { intent } => {
            let fid = registry.root_faction(faction_id);
            let Some(faction) = registry.factions.get(&fid) else {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            };
            if !faction.caps.home.is_mobile() {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            camp_ops.intent_sets.push((fid, intent));
            DispatchOutcome::Active
        }
        SetPackedAutonomy { mode } => {
            let fid = registry.root_faction(faction_id);
            let Some(faction) = registry.factions.get(&fid) else {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            };
            if !faction.caps.home.is_mobile() {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            camp_ops.autonomy_sets.push((fid, mode));
            DispatchOutcome::Active
        }
        PitchCamp { tile, z } => {
            let fid = registry.root_faction(faction_id);
            let Some(faction) = registry.factions.get(&fid) else {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            };
            if !faction.caps.home.is_mobile() {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            if !matches!(
                faction.camp_state,
                crate::simulation::faction::CampState::Packed { .. }
            ) {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            // Passability check.
            if !routing.chunk_map.is_passable(tile.0, tile.1) {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            // Component-exact reachability from the actor's tile to the
            // exact target tile/z (not just the chunk pair).
            if !routing.chunk_connectivity.tile_reachable(
                &routing.chunk_graph,
                (cur_tile.0, cur_tile.1, ai.current_z),
                (tile.0, tile.1, z),
            ) {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            // Prevent same-spot pitch.
            let cheb = (tile.0 - cur_tile.0).abs().max((tile.1 - cur_tile.1).abs());
            if cheb < crate::simulation::nomad::MIN_PITCH_DISTANCE {
                return DispatchOutcome::Failed(CommandFailure::Ineligible);
            }
            if !camp_ops.pitches.iter().any(|p| p.fid == fid) {
                camp_ops
                    .pitches
                    .push(crate::simulation::nomad::PendingPitch {
                        fid,
                        tile,
                        z,
                        command_actor: actor,
                    });
            }
            DispatchOutcome::Active
        }
        QueueVehicle { .. } | QueueCustomVehicle { .. } | VehicleOrder { .. } => {
            // Faction-level — applied directly in
            // `drain_player_command_events_system` (empty `actors`). If an
            // event ever arrives carrying actors, the dispatch is a no-op.
            DispatchOutcome::Active
        }
        Lookout { tile, z } => {
            // Stand-on routing (target == stand tile). The lookout task
            // is preserved by being under AgentGoal::FollowingPlayerCommand
            // (short-circuit at the top of goal_dispatch_system).
            let routed = assign_task_with_routing(
                ai,
                cur_tile,
                cur_chunk,
                tile,
                TaskKind::Lookout,
                None,
                &routing.chunk_graph,
                &routing.chunk_router,
                &routing.chunk_map,
                &routing.chunk_connectivity,
            );
            if !routed {
                return DispatchOutcome::Failed(CommandFailure::Unreachable);
            }
            ai.target_z = z;
            aq.dispatch(crate::simulation::typed_task::Task::Lookout {
                anchor: tile,
                anchor_z: z,
                expires_tick: None,
            });
            DispatchOutcome::Active
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_status_terminal_classification() {
        assert!(!CommandStatus::Pending.is_terminal());
        assert!(!CommandStatus::Active.is_terminal());
        assert!(CommandStatus::Completed.is_terminal());
        assert!(CommandStatus::Failed(CommandFailure::Unreachable).is_terminal());
        assert!(CommandStatus::Superseded.is_terminal());
    }

    #[test]
    fn id_generator_is_monotonic() {
        let mut gen = PlayerCommandIdGen::default();
        let a = gen.allocate();
        let b = gen.allocate();
        let c = gen.allocate();
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 2);
    }
}
