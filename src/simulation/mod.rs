use bevy::prelude::*;

pub mod animals;
pub mod carry;
pub mod carve;
pub mod combat;
pub mod construction;
pub mod corpse;
pub mod crafting;
pub mod dig;
pub mod faction;
pub mod gather;
pub mod goals;
pub mod items;
pub mod jobs;
pub mod line_of_sight;
pub mod lod;
pub mod memory;
pub mod military;
pub mod mood;
pub mod movement;
pub mod needs;
pub mod person;
pub mod plan;
pub mod plants;
pub mod production;
pub mod projects;
pub mod raid;
pub mod reproduction;
pub mod schedule;
pub mod settlement;
pub mod skills;
pub mod sound;
pub mod stats;
pub mod tasks;
pub mod technology;
pub mod terraform;
pub mod world_sim;

pub use schedule::SimClock;

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum SimulationSet {
    Input,
    ParallelA,
    ParallelB,
    Sequential,
    Economy,
}

pub struct SimulationPlugin;

impl Plugin for SimulationPlugin {
    fn build(&self, app: &mut App) {
        let mut step_registry = plan::StepRegistry::default();
        let mut plan_registry = plan::PlanRegistry::default();
        plan::register_builtin_steps(&mut step_registry);
        plan::register_builtin_plans(&mut plan_registry);

        app.add_plugins(jobs::JobsPlugin)
            .add_plugins(projects::ProjectsPlugin)
            .add_event::<combat::CombatEvent>()
            .add_event::<combat::DistressCallEvent>()
            .add_event::<combat::HandDropEvent>()
            .add_event::<plan::DropAbandonedFoodEvent>()
            .insert_resource(SimClock::default())
            .insert_resource(faction::FactionRegistry::default())
            .insert_resource(faction::PlayerFaction::default())
            .insert_resource(faction::StorageTileMap::default())
            .insert_resource(faction::StorageReservations::default())
            .insert_resource(production::TileDepletion::default())
            .insert_resource(plants::PlantMap::default())
            .insert_resource(plants::PlantSpriteIndex::default())
            .insert_resource(step_registry)
            .insert_resource(plan_registry)
            .insert_resource(construction::AutonomousBuildingToggle(true))
            .insert_resource(construction::BedMap::default())
            .insert_resource(construction::WallMap::default())
            .insert_resource(construction::CampfireMap::default())
            .insert_resource(construction::DoorMap::default())
            .insert_resource(construction::WorkbenchMap::default())
            .insert_resource(construction::LoomMap::default())
            .insert_resource(construction::TableMap::default())
            .insert_resource(construction::ChairMap::default())
            .insert_resource(construction::GranaryMap::default())
            .insert_resource(construction::ShrineMap::default())
            .insert_resource(construction::MarketMap::default())
            .insert_resource(construction::BarracksMap::default())
            .insert_resource(construction::MonumentMap::default())
            .insert_resource(construction::BlueprintMap::default())
            .insert_resource(crafting::CraftOrderMap::default())
            .insert_resource(construction::RoadCarveQueue::default())
            .insert_resource(construction::RitualState::default())
            .insert_resource(terraform::TerraformMap::default())
            .insert_resource(terraform::PendingFootprints::default())
            .insert_resource(settlement::SettlementPlans::default())
            .insert_resource(settlement::ZoneOverlayToggle::default())
            .insert_resource(plan::RelInfluence::default())
            .insert_resource(military::ActiveRallyPoints::default())
            .insert_resource(corpse::CorpseMap::default())
            .insert_resource(military::MusterHuntersRequest::default())
            .configure_sets(
                FixedUpdate,
                (
                    SimulationSet::ParallelA,
                    SimulationSet::ParallelB.after(SimulationSet::ParallelA),
                    SimulationSet::Sequential.after(SimulationSet::ParallelB),
                    SimulationSet::Economy.after(SimulationSet::Sequential),
                ),
            )
            .add_systems(
                PostStartup,
                (
                    person::spawn_population
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    animals::spawn_animals
                        .after(person::spawn_population)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    faction::center_camera_on_player_faction
                        .after(person::spawn_population)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                ),
            )
            .add_systems(
                FixedUpdate,
                (
                    needs::tick_needs_system,
                    mood::derive_mood_system,
                    items::recompute_inventory_capacity_system,
                    lod::update_lod_levels_system,
                    faction::update_storage_tile_map_system,
                    faction::sync_faction_center_hotspots_system,
                    animals::animal_needs_tick_system,
                )
                    .in_set(SimulationSet::ParallelA),
            )
            .add_systems(
                FixedUpdate,
                (
                    goals::goal_update_system.after(needs::tick_needs_system),
                    animals::animal_sense_system,
                )
                    .in_set(SimulationSet::ParallelA),
            )
            .add_systems(
                FixedUpdate,
                (
                    jobs::job_goal_lock_system.before(tasks::goal_dispatch_system),
                    jobs::job_claim_system.before(jobs::job_goal_lock_system),
                    jobs::job_board_command_system.before(jobs::job_claim_system),
                    tasks::goal_dispatch_system,
                    terraform::terraform_dispatch_system.after(tasks::goal_dispatch_system),
                )
                    .in_set(SimulationSet::ParallelB),
            )
            .add_systems(
                FixedUpdate,
                (
                    carry::enforce_hand_state_system.before(gather::gather_system),
                    gather::gather_system
                        .after(carry::enforce_hand_state_system)
                        .before(production::production_system),
                    dig::dig_system
                        .after(gather::gather_system)
                        .before(plan::plan_execution_system),
                    terraform::terraform_system
                        .after(gather::gather_system)
                        .before(plan::plan_execution_system),
                    terraform::footprint_completion_system
                        .after(terraform::terraform_system)
                        .before(plan::plan_execution_system),
                    construction::construction_system
                        .after(gather::gather_system)
                        .after(terraform::footprint_completion_system)
                        .before(plan::plan_execution_system),
                    construction::deconstruct_system
                        .after(construction::construction_system)
                        .before(plan::plan_execution_system),
                    construction::road_carve_system.after(construction::construction_system),
                    construction::door_proximity_system.after(construction::construction_system),
                    movement::movement_system,
                    movement::dismount_system.after(movement::movement_system),
                    animals::animal_movement_system,
                    movement::update_spatial_index_system
                        .after(movement::movement_system)
                        .after(animals::animal_movement_system),
                    movement::mount_check_system
                        .after(movement::dismount_system)
                        .after(movement::update_spatial_index_system),
                    movement::horse_position_sync_system.after(movement::mount_check_system),
                    memory::vision_system.after(movement::update_spatial_index_system),
                    combat::combat_system.after(movement::update_spatial_index_system),
                    combat::distress_emit_system.after(combat::combat_system),
                    combat::death_system.after(combat::distress_emit_system),
                    sound::respond_to_distress_system.after(combat::distress_emit_system),
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    combat::hand_drop_event_handler.after(combat::combat_system),
                    reproduction::cosleep_observation_system
                        .after(movement::update_spatial_index_system),
                    tasks::play_system
                        .after(movement::update_spatial_index_system)
                        .before(plan::plan_execution_system),
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (military::military_task_system
                    .after(movement::movement_system)
                    .before(combat::combat_system),)
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (corpse::corpse_follow_system.after(movement::movement_system),)
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (plan::explore_satisfaction_system
                    .after(memory::vision_system)
                    .before(plan::plan_execution_system),)
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (movement::recover_stranded_agents_system
                    .after(movement::movement_system)
                    .after(construction::construction_system)
                    .after(dig::dig_system)
                    .before(movement::update_spatial_index_system),)
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    items::item_pickup_system.after(combat::death_system),
                    items::equip_task_system
                        .after(items::item_pickup_system)
                        .before(plan::plan_execution_system),
                    plants::deer_graze_system.after(movement::update_spatial_index_system),
                    production::production_system.after(movement::movement_system),
                    crafting::craft_order_system
                        .after(gather::gather_system)
                        .before(plan::plan_execution_system),
                    production::eat_task_system.after(movement::movement_system),
                    production::withdraw_food_task_system.after(movement::movement_system),
                    production::withdraw_material_task_system.after(movement::movement_system),
                    production::withdraw_good_task_system.after(movement::movement_system),
                    production::tame_task_system
                        .after(movement::movement_system)
                        .before(plan::plan_execution_system),
                    (
                        corpse::pickup_corpse_task_system,
                        corpse::haul_corpse_task_system,
                        corpse::butcher_task_system,
                        corpse::wait_for_party_task_system,
                    )
                        .after(movement::movement_system)
                        .before(plan::plan_execution_system),
                    plan::rel_influence_system.after(movement::update_spatial_index_system),
                    plan::plan_execution_system
                        .after(production::production_system)
                        .after(production::eat_task_system)
                        .after(production::withdraw_food_task_system)
                        .after(production::withdraw_material_task_system)
                        .after(production::withdraw_good_task_system)
                        .after(plan::rel_influence_system),
                    reproduction::wake_up_conception_system.after(plan::plan_execution_system),
                    faction::bonding_system.after(movement::update_spatial_index_system),
                    production::tile_regen_system,
                    schedule::advance_sim_clock,
                    crate::world::seasons::advance_calendar_system,
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    memory::memory_decay_system,
                    faction::social_fill_system,
                    memory::conversation_memory_system.after(faction::social_fill_system),
                    plan::plan_gossip_system.after(memory::conversation_memory_system),
                    plan::plan_decay_system,
                    faction::faction_profession_system,
                    faction::drop_items_at_destination_system,
                    faction::compute_faction_storage_system
                        .after(faction::drop_items_at_destination_system),
                    reproduction::pregnancy_system,
                    animals::animal_reproduction_cooldown_system,
                    animals::animal_reproduction_system
                        .after(animals::animal_reproduction_cooldown_system),
                    raid::faction_decision_system.after(faction::compute_faction_storage_system),
                    raid::raid_detection_system.after(raid::faction_decision_system),
                    raid::raid_execution_system
                        .after(raid::raid_detection_system)
                        .after(faction::compute_faction_storage_system),
                    world_sim::world_sim_system,
                    world_sim::agent_exploration_system,
                    faction::chief_selection_system,
                    construction::chief_directive_system.after(faction::chief_selection_system),
                )
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                (
                    projects::project_lifecycle_system
                        .after(faction::compute_faction_storage_system)
                        .after(construction::chief_directive_system),
                    projects::workforce_budget_system
                        .after(projects::project_lifecycle_system),
                    projects::project_stagnation_system
                        .after(projects::project_lifecycle_system),
                    jobs::chief_job_posting_system
                        .after(faction::compute_faction_storage_system)
                        .after(faction::chief_selection_system)
                        .after(projects::project_lifecycle_system),
                    crafting::faction_craft_order_system
                        .after(jobs::chief_job_posting_system),
                    jobs::job_build_completion_system
                        .after(jobs::chief_job_posting_system),
                    jobs::job_claim_release_system
                        .after(jobs::job_build_completion_system),
                )
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                (
                    settlement::settlement_planner_system
                        .before(construction::chief_directive_system),
                    construction::building_upgrade_system
                        .after(settlement::settlement_planner_system)
                        .before(construction::chief_directive_system),
                    construction::ritual_system.after(construction::chief_directive_system),
                    construction::assign_beds_system,
                    faction::resource_demand_system
                        .after(construction::chief_directive_system)
                        .after(faction::compute_faction_storage_system),
                    faction::update_material_targets_system
                        .after(faction::compute_faction_storage_system),
                    technology::tech_discovery_system
                        .after(faction::compute_faction_storage_system)
                        .after(raid::raid_execution_system),
                    plan::drop_abandoned_food_system
                        .after(faction::drop_items_at_destination_system),
                    military::expire_rally_points_system,
                    military::apply_muster_hunters_system,
                    faction::chief_hunt_order_system
                        .after(faction::compute_faction_storage_system),
                    faction::faction_hunter_assignment_system
                        .after(faction::chief_hunt_order_system),
                    corpse::corpse_decay_system,
                )
                    .in_set(SimulationSet::Economy),
            );
    }
}
