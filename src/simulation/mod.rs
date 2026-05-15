use bevy::prelude::*;

pub mod animals;
pub mod apprenticeship;
pub mod archetype;
pub mod building_template;
pub mod camp;
pub mod capital;
pub mod carry;
pub mod carve;
pub mod civic_milestones;
pub mod clear_obstacle;
pub mod cohort;
pub mod combat;
pub mod construction;
pub mod corpse;
pub mod crafting;
pub mod dig;
pub mod doormat;
pub mod drink;
pub mod faction;
pub mod farm;
pub mod gather;
pub mod gather_claims;
pub mod goal_scorers;
pub mod goals;
pub mod htn;
pub mod items;
pub mod jobs;
pub mod knowledge;
pub mod land;
pub mod lifecycle;
pub mod line_of_sight;
pub mod lod;
pub mod medicine;
pub mod memory;
pub mod military;
pub mod mood;
pub mod movement;
pub mod needs;
pub mod nomad;
pub mod nomad_pack_labor;
pub mod nomad_pool;
pub mod obstacle;
pub mod opportunistic;
pub mod opportunity;
pub mod organic_settlement;
pub mod pack_deploy;
pub mod person;
pub mod plants;
pub mod player_command;
pub mod production;
pub mod profession_choice;
pub mod projects;
pub mod raid;
pub mod region;
pub mod reproduction;
pub mod river_context;
pub mod sanitation;
pub mod schedule;
pub mod sedentary_collapse;
pub mod settlement;
pub mod shared_knowledge;
pub mod skills;
pub mod sound;
pub mod speed;
pub mod stats;
pub mod survey_task;
pub mod tasks;
pub mod teaching;
pub mod technology;
pub mod technology_adoption;
pub mod terraform;
#[cfg(test)]
pub mod test_fixture;
pub mod trader;
pub mod typed_task;
pub mod utility_curves;
pub mod wild_herd;
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

/// Flip `SimulationState::Warmup → Active` after the `OnEnter(Playing)`
/// pass has spawned factions, run the initial settlement survey, and
/// stamped seed structures. Future per-tick systems can opt in to the
/// gate via `.run_if(in_state(SimulationState::Active))`; current
/// FixedUpdate sim systems run unconditionally to avoid a one-tick
/// idle gap on game start.
fn mark_warmup_complete_system(mut next: ResMut<NextState<crate::SimulationState>>) {
    next.set(crate::SimulationState::Active);
}

pub struct SimulationPlugin;

impl Plugin for SimulationPlugin {
    fn build(&self, app: &mut App) {
        let mut method_registry = htn::MethodRegistry::default();
        htn::register_builtin_methods(&mut method_registry);

        // Mirror the `Indexed` / `JobEscrow` hook pattern so structure
        // spawn/despawn paths stay untouched and `StructureIndex` is always
        // in sync with live `StructureLabel` entities.
        app.world_mut()
            .register_component_hooks::<construction::StructureLabel>()
            .on_add(construction::on_structure_label_add)
            .on_remove(construction::on_structure_label_remove);

        // Door doormat-release hook: dropping a Door frees its reserved
        // outside tile so future construction can land there. Matches the
        // `JobEscrow` refund-on-despawn pattern.
        app.world_mut()
            .register_component_hooks::<construction::Door>()
            .on_remove(doormat::release_doormat_on_door_remove);

        // Phase 2 (wage-aware-labor-market-v2): workshop ownership index
        // tracks profession-affine capital per faction. Stamped at
        // workshop finalize in `construction.rs`; maintained here so
        // despawn paths (lifecycle abandon, etc.) stay untouched.
        app.world_mut()
            .register_component_hooks::<capital::OwnedBy>()
            .on_add(capital::on_owned_by_add)
            .on_remove(capital::on_owned_by_remove);

        app.add_plugins(jobs::JobsPlugin)
            .add_plugins(projects::ProjectsPlugin)
            .add_event::<combat::CombatEvent>()
            .add_event::<combat::CombatRetaliationStartedEvent>()
            .add_event::<combat::DistressCallEvent>()
            .add_event::<combat::HandDropEvent>()
            .add_event::<knowledge::DiscoveryActionEvent>()
            .add_event::<land::PlotEvictedEvent>()
            .add_event::<player_command::PlayerCommandEvent>()
            .insert_resource(player_command::PlayerCommandIdGen::default())
            .insert_resource(nomad::PendingCampOps::default())
            .insert_resource(SimClock::default())
            .insert_resource(survey_task::SurveyCursor::default())
            .insert_resource(survey_task::InFlightSurveys::default())
            .insert_resource(speed::GameSpeed::default())
            .insert_resource(speed::SimTimingDiagnostics::default())
            .insert_resource(speed::TickTimer::default())
            .insert_resource(goals::ForceGoalReevaluate::default())
            .insert_resource({
                // Phase 6: scorer registry, pre-populated with the
                // default set (`EarnIncomeScorer` today; future
                // scorers — Socialize, Esteem, HealSeeker — pushed
                // alongside in `register_default_scorers`).
                let mut r = goal_scorers::GoalScorerRegistry::default();
                goal_scorers::register_default_scorers(&mut r);
                r
            })
            .insert_resource(faction::FactionRegistry::default())
            .insert_resource(faction::PlayerFaction::default())
            .insert_resource(faction::StorageTileMap::default())
            .insert_resource(faction::StorageReservations::default())
            .insert_resource(production::TileDepletion::default())
            .insert_resource(plants::PlantMap::default())
            .insert_resource(plants::PlantSpriteIndex::default())
            .insert_resource(method_registry)
            .insert_resource(construction::AutonomousBuildingToggle(true))
            .insert_resource(wild_herd::WildHerdRegistry::default())
            .insert_resource(construction::BedMap::default())
            .insert_resource(construction::WallMap::default())
            .insert_resource(construction::CampfireMap::default())
            .insert_resource(construction::DoorMap::default())
            .insert_resource(capital::WorkshopOwnership::default())
            .insert_resource(sanitation::SanitationMap::default())
            .insert_resource(opportunistic::OpportunisticInterruptStats::default())
            .insert_resource(goal_scorers::DecisionMetrics::default())
            .insert_resource(opportunity::OpportunityIndex::default())
            .insert_resource(cohort::CohortRegistry::default())
            .insert_resource(construction::WorkbenchMap::default())
            .insert_resource(construction::LoomMap::default())
            .insert_resource(construction::TableMap::default())
            .insert_resource(construction::ChairMap::default())
            .insert_resource(construction::GranaryMap::default())
            .insert_resource(construction::ShrineMap::default())
            .insert_resource(construction::MarketMap::default())
            .insert_resource(construction::BarracksMap::default())
            .insert_resource(construction::MonumentMap::default())
            .insert_resource(construction::BridgeMap::default())
            .insert_resource(construction::WellMap::default())
            .insert_resource(construction::StructureIndex::default())
            .insert_resource(construction::BlueprintMap::default())
            .insert_resource(crafting::CraftOrderMap::default())
            .insert_resource(construction::RoadCarveQueue::default())
            .insert_resource(doormat::DoormatReservations::default())
            .insert_resource(construction::RitualState::default())
            .insert_resource(terraform::TerraformMap::default())
            .insert_resource(terraform::PendingFootprints::default())
            .insert_resource(settlement::SettlementPlans::default())
            .insert_resource(settlement::SettlementMap::default())
            .insert_resource(organic_settlement::SettlementBrains::default())
            .insert_resource(organic_settlement::SettlementParcelIndex::default())
            .insert_resource(organic_settlement::SettlementPressureMap::default())
            .insert_resource(organic_settlement::SettlementIntentMap::default())
            .insert_resource(organic_settlement::SelectedSettlementIntents::default())
            .insert_resource(organic_settlement::load_building_archetype_catalog())
            .insert_resource(camp::CampMap::default())
            .insert_resource(lifecycle::LifecycleEventQueue::default())
            .insert_resource(settlement::ZoneOverlayToggle::default())
            .insert_resource(land::PlotIndex::default())
            .insert_resource(land::LandListings::default())
            .insert_resource(farm::FarmPlotAssignments::default())
            .insert_resource(military::ActiveRallyPoints::default())
            .insert_resource(military::MilitaryFormationGroupGen::default())
            .insert_resource(military::PendingFormationSlots::default())
            .insert_resource(corpse::CorpseMap::default())
            .insert_resource(teaching::LectureRequest::default())
            .insert_resource(jobs::PlayerCraftRequest::default())
            .insert_resource(shared_knowledge::SharedKnowledge::default())
            .insert_resource(gather_claims::GatherClaims::default())
            .configure_sets(
                FixedUpdate,
                (
                    SimulationSet::Input,
                    SimulationSet::ParallelA.after(SimulationSet::Input),
                    SimulationSet::ParallelB.after(SimulationSet::ParallelA),
                    SimulationSet::Sequential.after(SimulationSet::ParallelB),
                    SimulationSet::Economy.after(SimulationSet::Sequential),
                ),
            )
            // Speed/pause: mirror `GameSpeed` onto `Time<Virtual>` every
            // PreUpdate. Per-frame tick counter lives on Update so it ticks
            // even when sim is paused. The keyboard handler lives in
            // `UiPlugin` (it depends on egui + `ButtonInput<KeyCode>`,
            // neither of which the headless test fixture loads).
            .add_systems(PreUpdate, speed::sync_game_speed_to_virtual_time)
            .add_systems(Update, speed::frame_tick_count_system)
            // Sim-tick CPU timing: stamp the start of each FixedUpdate
            // before any sim work and read it at the end to fold into the
            // EMA + worst-tick window. Both systems live outside
            // `SimulationSet` so they bracket every other tick body.
            .add_systems(
                FixedUpdate,
                speed::fixed_tick_timing_start_system.before(SimulationSet::Input),
            )
            .add_systems(
                FixedUpdate,
                speed::fixed_tick_timing_end_system.after(SimulationSet::Economy),
            )
            .add_systems(
                OnEnter(crate::GameState::Playing),
                (
                    person::spawn_population
                        .after(crate::world::terrain::spawn_world_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    animals::spawn_animals
                        .after(person::spawn_population)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    wild_herd::seed_wild_herds_system
                        .after(animals::spawn_animals)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    faction::center_camera_on_player_faction
                        .after(person::spawn_population)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Promote Settlement spawning to OnEnter so the kickoff
                    // survey + seed pass downstream see live `Settlement`
                    // entities. `auto_found_default_settlements_system` is
                    // idempotent (gates on `map.by_faction.contains_key`);
                    // the FixedUpdate registration in the Economy schedule
                    // remains so post-startup faction creation still flows
                    // through it.
                    settlement::auto_found_default_settlements_system
                        .after(person::spawn_population)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Era-aware seeding priming: project the chief's
                    // awareness onto `FactionData.techs`, derive the
                    // community `tech_adoption` table, and ratchet
                    // `Settlement.peak_population` from spawned
                    // `member_count` — all *before* the survey + seed
                    // pass reads them. Reuses the same Economy-schedule
                    // systems (idempotent at tick 0) so OnEnter priming
                    // and per-tick maintenance share one source of
                    // truth.
                    faction::sync_faction_techs_from_chief_system
                        .after(person::spawn_population)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    technology_adoption::derive_tech_adoption_system
                        .after(faction::sync_faction_techs_from_chief_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Founder-band override: at tick 0 the runtime
                    // adoption gates (Specialist needs ≤8 members or
                    // every adult Learned; Institutional needs a civic
                    // building) reject Neolithic+ starts of typical
                    // population sizes, leaving `PERM_SETTLEMENT` short
                    // of `Adopted`. Force-stamp every era-prior
                    // chief-Aware tech to `Adopted` so the seed pass
                    // sees a competent civilization.
                    technology_adoption::seed_prime_tech_adoption_system
                        .after(technology_adoption::derive_tech_adoption_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    settlement::settlement_peak_population_system
                        .after(settlement::auto_found_default_settlements_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Run a one-shot survey so `SettlementBrain` (parcels,
                    // road tiles, frontage_edge) exists before the seed
                    // pass picks house anchors. Unified-build-pipeline
                    // step: same `survey_one_settlement` body the
                    // runtime survey loop uses.
                    organic_settlement::kickoff_initial_survey_system
                        .after(settlement::auto_found_default_settlements_system)
                        .after(technology_adoption::derive_tech_adoption_system)
                        .after(technology_adoption::seed_prime_tech_adoption_system)
                        .after(settlement::settlement_peak_population_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    construction::seed_starting_buildings_system
                        .after(person::spawn_population)
                        .after(organic_settlement::kickoff_initial_survey_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Seeded structures (walls, beds, hearths, nomadic shelters)
                    // bypass the blueprint pipeline, so the per-blueprint
                    // obstacle scan never runs on them. This one-shot pass
                    // walks `StructureIndex` and clears obstacles on every
                    // seeded tile — plants despawn (yields drop on ground),
                    // loose rocks relocate aside.
                    clear_obstacle::clear_obstacles_under_seeded_structures
                        .after(construction::seed_starting_buildings_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Farm-planner §15: ensure every starting village has at
                    // least one Agricultural plot + seed grain in storage so
                    // farming can start on tick 1 instead of waiting for the
                    // organic carve pipeline.
                    farm::seed_starting_farms_system
                        .after(construction::seed_starting_buildings_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                    // Flip the warmup gate now that the initial survey +
                    // seed have applied. Future systems can opt in via
                    // `.run_if(in_state(SimulationState::Active))`.
                    mark_warmup_complete_system
                        .after(clear_obstacle::clear_obstacles_under_seeded_structures)
                        .after(farm::seed_starting_farms_system)
                        .run_if(not(resource_exists::<crate::sandbox::SandboxMode>)),
                ),
            )
            .add_systems(
                FixedUpdate,
                (
                    player_command::drain_player_command_events_system,
                    // Pre-dispatch: expand multi-actor `MilitaryMove`
                    // events into per-actor slot tiles around the anchor.
                    // Reads `PlayerCommandEvent` directly (independent
                    // `EventReader` from drain) so ordering doesn't
                    // matter inside `Input`.
                    military::expand_military_move_system,
                )
                    .in_set(SimulationSet::Input),
            )
            .add_systems(
                FixedUpdate,
                (player_command::dispatch_player_command_system,).in_set(SimulationSet::ParallelB),
            )
            .add_systems(
                FixedUpdate,
                (
                    player_command::player_command_lifecycle_system,
                    player_command::reap_terminal_commands_system
                        .after(player_command::player_command_lifecycle_system),
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    needs::tick_needs_system,
                    mood::derive_mood_system,
                    items::recompute_inventory_capacity_system,
                    lod::update_lod_levels_system,
                    cohort::cohort_pin_full_sim_system
                        .after(lod::update_lod_levels_system)
                        .before(goals::goal_update_system)
                        .before(goal_scorers::sample_decision_metrics_system)
                        .before(cohort::rebuild_cohort_registry_system),
                    faction::update_storage_tile_map_system,
                    faction::sync_faction_center_hotspots_system,
                    animals::animal_needs_tick_system,
                    goal_scorers::sample_decision_metrics_system,
                    cohort::rebuild_cohort_registry_system,
                )
                    .in_set(SimulationSet::ParallelA),
            )
            .add_systems(
                FixedUpdate,
                (
                    goals::goal_update_system.after(needs::tick_needs_system),
                    // Phase 5: must observe `Changed<AgentGoal>` after
                    // `goal_update_system` flips it but before the typed-task
                    // tick so the abandoned method's bias is in `MethodHistory`
                    // by the time the next dispatcher reads it. `Changed` only
                    // fires on real flips because both `goal_update_system`
                    // and `job_goal_lock_system` guard their `*goal = X` writes
                    // on `*goal != new_goal`.
                    goals::record_abandoned_method_system.after(goals::goal_update_system),
                    // Phase C (mobile gating): demote settled-life
                    // goals on members of CampState::Packed factions
                    // to GatherFood + drop any held JobClaim. Runs
                    // after goal_update so the per-tick selection is
                    // honoured first; before the dispatchers in
                    // ParallelB so blocked goals never run.
                    goals::mobile_state_goal_gate_system.after(goals::goal_update_system),
                    // Phase D (behavioural richness): opportunistic
                    // mid-walk interrupts. Runs after the cascade has
                    // set the agent's authoritative goal but before
                    // record_abandoned_method_system so any opportunistic
                    // flip's goal change feeds Abandoned outcomes into
                    // MethodHistory in the same tick.
                    opportunistic::opportunistic_interrupt_system
                        .after(goals::goal_update_system)
                        .before(goals::record_abandoned_method_system),
                    animals::animal_sense_system,
                    // Bug-fix #2: re-snap tamed animals' target_tile
                    // toward their faction's `home_tile` every
                    // quarter-day, surviving Dormant LOD cycles.
                    animals::following_band_animal_redirect_system,
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
                    htn::htn_dispatch_system.after(tasks::goal_dispatch_system),
                    // Equip-hunting-spear runs ahead of the food dispatchers
                    // so an unarmed hunter prefers fetching their spear over
                    // eating (mirrors legacy plan bias 5.0 + PF_UNINTERRUPTIBLE).
                    htn::htn_equip_hunting_spear_dispatch_system.after(htn::htn_dispatch_system),
                    htn::htn_eat_dispatch_system
                        .after(htn::htn_equip_hunting_spear_dispatch_system),
                    htn::htn_acquire_food_dispatch_system.after(htn::htn_eat_dispatch_system),
                    htn::htn_acquire_good_dispatch_system
                        .after(htn::htn_acquire_food_dispatch_system),
                    htn::htn_scout_dispatch_system.after(htn::htn_acquire_good_dispatch_system),
                    htn::htn_return_surplus_dispatch_system.after(htn::htn_scout_dispatch_system),
                    htn::htn_tame_horse_dispatch_system
                        .after(htn::htn_return_surplus_dispatch_system),
                    htn::htn_plant_from_storage_dispatch_system
                        .after(htn::htn_tame_horse_dispatch_system),
                    htn::htn_build_claimed_blueprint_dispatch_system
                        .after(htn::htn_plant_from_storage_dispatch_system),
                    htn::htn_deliver_hunt_kill_dispatch_system
                        .after(htn::htn_build_claimed_blueprint_dispatch_system),
                    htn::htn_engage_prey_dispatch_system
                        .after(htn::htn_deliver_hunt_kill_dispatch_system),
                    htn::htn_join_hunt_party_dispatch_system
                        .after(htn::htn_engage_prey_dispatch_system),
                    // Subsistence stockpile is the lowest-priority
                    // autonomous work — runs after every specialised
                    // dispatcher so corpse carriers / hunters /
                    // builders / migrators / etc. claim the agent
                    // first.
                    htn::htn_stockpile_food_dispatch_system
                        .after(htn::htn_join_hunt_party_dispatch_system),
                    htn::htn_socialize_dispatch_system
                        .after(htn::htn_stockpile_food_dispatch_system),
                    htn::htn_combat_faction_dispatch_system
                        .after(htn::htn_socialize_dispatch_system),
                )
                    .in_set(SimulationSet::ParallelB),
            )
            .add_systems(
                FixedUpdate,
                (terraform::terraform_dispatch_system
                    .after(tasks::goal_dispatch_system)
                    .after(htn::htn_dispatch_system)
                    .after(htn::htn_equip_hunting_spear_dispatch_system)
                    .after(htn::htn_eat_dispatch_system)
                    .after(htn::htn_acquire_food_dispatch_system)
                    .after(htn::htn_acquire_good_dispatch_system)
                    .after(htn::htn_stockpile_food_dispatch_system)
                    .after(htn::htn_scout_dispatch_system)
                    .after(htn::htn_return_surplus_dispatch_system)
                    .after(htn::htn_tame_horse_dispatch_system)
                    .after(htn::htn_plant_from_storage_dispatch_system)
                    .after(htn::htn_build_claimed_blueprint_dispatch_system)
                    .after(htn::htn_deliver_hunt_kill_dispatch_system)
                    .after(htn::htn_engage_prey_dispatch_system)
                    .after(htn::htn_join_hunt_party_dispatch_system)
                    .after(htn::htn_socialize_dispatch_system)
                    .after(htn::htn_combat_faction_dispatch_system),)
                    .in_set(SimulationSet::ParallelB),
            )
            // Pluralist Economy R5 follow-on: bureaucrat admin
            // dispatcher in its own add_systems call (the main
            // ParallelB tuple already brushes the 20-element
            // IntoSystemConfigs ceiling). Ordered after the combat
            // dispatcher so chief / lead / defend / raid take
            // priority over bureaucrat town-hall stationing.
            .add_systems(
                FixedUpdate,
                (
                    htn::bureaucrat_admin_dispatch_system
                        .after(htn::htn_combat_faction_dispatch_system),
                    // Heal-3 (heal-pipeline): Healer-side dispatcher.
                    // Ordered after combat so combat-driven Heal events
                    // settle first; idle + UNEMPLOYED gate ensures only
                    // newly-tasked Healers are reached.
                    medicine::htn_provide_care_dispatch_system
                        .after(htn::htn_combat_faction_dispatch_system),
                    // Heal-3b: SeekCare patient routes to the nearest
                    // faction Shrine (or home_tile) so Healers can
                    // converge. Ordered after ProvideCare so a Healer
                    // who is also a patient (rare) prefers serving
                    // others first.
                    medicine::htn_seek_care_dispatch_system
                        .after(medicine::htn_provide_care_dispatch_system),
                    // Pluralist Economy R10 follow-on: trader route
                    // dispatcher. Same ParallelB shape as bureaucrat
                    // admin (idle + UNEMPLOYED gate + Lead routing);
                    // gated additionally on `TraderPlan` so only
                    // mid-cycle traders route via this path. Ordered
                    // after bureaucrat dispatch for deterministic
                    // ordering — neither system writes to the same
                    // agent (Bureaucrats and Traders are disjoint
                    // professions).
                    trader::trader_route_dispatch_system
                        .after(htn::bureaucrat_admin_dispatch_system),
                    // P1: nomad-band migration dispatcher. Walks members
                    // with a `MigrationTarget` toward the new camp tile.
                    // Ordered after bureaucrat (idle agents only) and
                    // gated on goal == MigrateToCamp inside the system.
                    nomad::nomad_migration_dispatch_system
                        .after(trader::trader_route_dispatch_system),
                    // Phase D: route Scout-goal members toward their
                    // ScoutAssignment tile to seed faction-tier
                    // SharedKnowledge before commit.
                    nomad::nomad_survey_dispatch_system
                        .after(nomad::nomad_migration_dispatch_system),
                )
                    .in_set(SimulationSet::ParallelB),
            )
            // P8: attach `PackAnimalInventory` to freshly tamed pack
            // animals (Horse / Cow / Pig). Own add_systems call to dodge
            // the 20-element ceiling on the Sequential tuple.
            .add_systems(
                FixedUpdate,
                animals::attach_pack_inventory_system.in_set(SimulationSet::Sequential),
            )
            // Calendar-driven plant lifecycle (replaces the old per-frame
            // tick growth + scatter pair). Edge-triggers on season change;
            // ordered after `advance_calendar_system` so the season the
            // calendar just landed on is the one we react to.
            .add_systems(
                FixedUpdate,
                plants::plant_lifecycle_system
                    .after(crate::world::seasons::advance_calendar_system)
                    .in_set(SimulationSet::Sequential),
            )
            // ClearObstacle executor — own add_systems call (Sequential
            // tuples are at the 20-element ceiling). Ordered after
            // movement (work_progress is incremented there) and before
            // construction (build gate reads `pending_clear`).
            .add_systems(
                FixedUpdate,
                clear_obstacle::clear_obstacle_task_system
                    .after(movement::movement_system)
                    .before(construction::construction_system)
                    .in_set(SimulationSet::Sequential),
            )
            // Observable nomad pack/pitch labor — workers dismantle old
            // camp structures, unload caravan cargo, then pitch minimal
            // final-camp structures before the chief repairs the rest.
            .add_systems(
                FixedUpdate,
                (
                    nomad_pack_labor::unpitch_structure_task_system,
                    nomad_pack_labor::unload_camp_cargo_task_system,
                    nomad_pack_labor::pitch_structure_at_task_system,
                    nomad_pack_labor::continue_pack_labor_system
                        .after(nomad_pack_labor::unpitch_structure_task_system)
                        .after(nomad_pack_labor::unload_camp_cargo_task_system)
                        .after(nomad_pack_labor::pitch_structure_at_task_system),
                )
                    .after(movement::movement_system)
                    .in_set(SimulationSet::Sequential),
            )
            // Reactive: every newly-spawned `Blueprint` has its footprint
            // scanned for `ConstructionObstacle` entities. WorkerClear
            // hits land on `pending_clear`; Relocate hits move aside
            // synchronously. Sequential, before construction so the
            // gate sees populated pending_clear on the same tick.
            .add_systems(
                FixedUpdate,
                obstacle::populate_pending_clear_system
                    .before(construction::construction_system)
                    .in_set(SimulationSet::Sequential),
            )
            // Reactive cleanup for obstacles that spawn under existing
            // structures / blueprints — most importantly, loose rocks
            // streamed in by `chunk_streaming_system` (FixedUpdate) onto
            // tiles that the OnEnter(Playing) seeded-structure pass
            // already left behind. Sequential after movement so the
            // `relocate_entity_aside` Transform mutation rides the next
            // `sync_indexed_after_move_system` pass.
            .add_systems(
                FixedUpdate,
                clear_obstacle::react_obstacle_under_structure_system
                    .after(movement::sync_indexed_after_move_system)
                    .in_set(SimulationSet::Sequential),
            )
            // ClearObstacle dispatcher — own add_systems call (ParallelB
            // tuples are at the 20-element ceiling). Routes idle Build-
            // goal agents to the first pending_clear obstacle on a same-
            // faction (or personal) blueprint. Ordered after the build
            // dispatcher; both systems gate on (Idle + UNEMPLOYED), so
            // one wins the agent each tick.
            .add_systems(
                FixedUpdate,
                htn::htn_clear_obstacle_dispatch_system
                    .after(htn::htn_build_claimed_blueprint_dispatch_system)
                    .in_set(SimulationSet::ParallelB),
            )
            // Thirst pipeline (Phase 2): dispatcher routes `AgentGoal::Drink`
            // agents to drinking sources. Drink-task executor runs Sequential
            // after movement so adjacency arrival is settled.
            .add_systems(
                FixedUpdate,
                drink::htn_drink_dispatch_system
                    .after(htn::htn_eat_dispatch_system)
                    .in_set(SimulationSet::ParallelB),
            )
            .add_systems(
                FixedUpdate,
                drink::drink_task_system
                    .after(movement::movement_system)
                    .in_set(SimulationSet::Sequential),
            )
            // Phase 3 (thirst): animal water seek + drink executors.
            // Seek runs in ParallelA after `animal_needs_tick_system`
            // (which has updated thirst this tick). Drink runs Sequential
            // after `animal_movement_system` has settled the new position.
            .add_systems(
                FixedUpdate,
                animals::animal_water_seek_system
                    .after(animals::animal_needs_tick_system)
                    .in_set(SimulationSet::ParallelA),
            )
            .add_systems(
                FixedUpdate,
                animals::animal_drink_system
                    .after(animals::animal_movement_system)
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                // Phase 5e-xi-a/b: split-off because the main ParallelB tuple
                // is already at Bevy's 20-element IntoSystemConfigs ceiling.
                // Same SimulationSet, ordered after the combat dispatcher to
                // keep the HTN chain semantically contiguous.
                FixedUpdate,
                (
                    htn::htn_deliver_material_to_craft_order_dispatch_system
                        .after(htn::htn_combat_faction_dispatch_system),
                    htn::htn_work_on_craft_order_dispatch_system
                        .after(htn::htn_deliver_material_to_craft_order_dispatch_system),
                    htn::htn_harvest_grain_for_craft_order_dispatch_system
                        .after(htn::htn_work_on_craft_order_dispatch_system),
                    htn::htn_harvest_plant_dispatch_system
                        .after(htn::htn_harvest_grain_for_craft_order_dispatch_system),
                    htn::htn_play_dispatch_system.after(htn::htn_harvest_plant_dispatch_system),
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
                    dig::dig_system.after(gather::gather_system),
                    terraform::terraform_system.after(gather::gather_system),
                    terraform::footprint_completion_system.after(terraform::terraform_system),
                    construction::construction_system
                        .after(gather::gather_system)
                        .after(terraform::footprint_completion_system),
                    construction::deconstruct_system.after(construction::construction_system),
                    construction::road_carve_system.after(construction::construction_system),
                    construction::door_proximity_system.after(construction::construction_system),
                    movement::movement_system,
                    movement::dismount_system.after(movement::movement_system),
                    animals::animal_movement_system,
                    movement::sync_indexed_after_move_system
                        .after(movement::movement_system)
                        .after(animals::animal_movement_system),
                    movement::mount_check_system
                        .after(movement::dismount_system)
                        .after(movement::sync_indexed_after_move_system),
                    movement::horse_position_sync_system.after(movement::mount_check_system),
                    memory::vision_system.after(movement::sync_indexed_after_move_system),
                    combat::combat_system.after(movement::sync_indexed_after_move_system),
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
                    combat::combat_retaliation_cleanup_system.after(combat::combat_system),
                    reproduction::cosleep_observation_system
                        .after(movement::sync_indexed_after_move_system),
                    tasks::play_system.after(movement::sync_indexed_after_move_system),
                    medicine::injury_tracking_system.after(combat::combat_system),
                    // Heal-3 executor: Healer adjacent to patient
                    // decrements `Injury.severity` per tick. Runs
                    // after combat so freshly-applied damage is
                    // already reflected on the patient.
                    medicine::heal_task_system.after(combat::combat_system),
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
                (combat::hunt_chase_system
                    .after(movement::sync_indexed_after_move_system)
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
                (
                    teaching::apply_teach_order_system.after(movement::movement_system),
                    teaching::read_task_system.after(movement::movement_system),
                    teaching::teach_task_system.after(teaching::apply_teach_order_system),
                    teaching::lecture_tick_system.after(movement::movement_system),
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (movement::recover_stranded_agents_system
                    .after(movement::movement_system)
                    .after(construction::construction_system)
                    .after(dig::dig_system)
                    .before(movement::sync_indexed_after_move_system),)
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    items::item_pickup_system.after(combat::death_system),
                    items::equip_task_system.after(items::item_pickup_system),
                    plants::deer_graze_system.after(movement::sync_indexed_after_move_system),
                    production::production_system.after(movement::movement_system),
                    crafting::craft_order_system.after(gather::gather_system),
                    production::eat_task_system.after(movement::movement_system),
                    production::withdraw_food_task_system.after(movement::movement_system),
                    production::withdraw_material_task_system.after(movement::movement_system),
                    production::take_from_member_task_system.after(movement::movement_system),
                    production::withdraw_good_task_system.after(movement::movement_system),
                    production::tame_task_system.after(movement::movement_system),
                    (
                        corpse::pickup_corpse_task_system,
                        corpse::haul_corpse_task_system,
                        corpse::butcher_task_system,
                        corpse::wait_for_party_task_system,
                    )
                        .after(movement::movement_system),
                    reproduction::wake_up_conception_system
                        .after(production::production_system)
                        .after(production::eat_task_system),
                    faction::bonding_system.after(movement::sync_indexed_after_move_system),
                    production::tile_regen_system,
                    // Phase 8 follow-on: drain `pending_migration` orders
                    // queued by `nomad_migration_system` (Economy, daily).
                    // Sits in Sequential so the despawn happens cleanly
                    // before the next tick's pathing / sleep dispatch.
                    nomad::nomad_migration_commit_system,
                    // P1: per-tick arrival check for in-flight migrations.
                    // Sequential after movement so position reads are
                    // already up-to-date this tick.
                    nomad::nomad_migration_arrival_system.after(movement::movement_system),
                    // Phase 10: per-tick bloom/collapse based on camera
                    // proximity. Runs in Sequential so the entity spawns/
                    // despawns are visible to next tick's render + AI
                    // systems without an extra frame of lag.
                    wild_herd::wild_herd_bloom_system,
                    schedule::advance_sim_clock,
                    crate::world::seasons::advance_calendar_system,
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    // Player Pack/Pitch Camp commands. Drain
                    // `PendingCampOps` written by the player-command
                    // dispatcher in ParallelB. Pack runs before Pitch
                    // so a same-tick player Pack→Pitch is well-ordered.
                    // After AI commit so they share shelter cleanup
                    // ordering rather than fighting it.
                    nomad::apply_pack_camp_command_system
                        .after(nomad::nomad_migration_commit_system),
                    nomad::apply_pitch_camp_command_system
                        .after(nomad::apply_pack_camp_command_system),
                    // Phase 2/3: player-driven nomad commands.
                    nomad::apply_manual_scout_command_system
                        .after(nomad::apply_pitch_camp_command_system),
                    nomad::manual_scout_completion_system
                        .after(nomad::apply_manual_scout_command_system),
                    nomad::apply_migration_intent_system
                        .after(nomad::manual_scout_completion_system),
                    nomad::apply_packed_autonomy_system.after(nomad::apply_migration_intent_system),
                )
                    .in_set(SimulationSet::Sequential),
            )
            .add_systems(
                FixedUpdate,
                (
                    memory::relationship_decay_system,
                    faction::social_fill_system,
                    memory::conversation_memory_system.after(faction::social_fill_system),
                    knowledge::awareness_gossip_system.after(memory::conversation_memory_system),
                    faction::faction_profession_system,
                    faction::drop_items_at_destination_system,
                    htn::htn_method_completion_system
                        .after(faction::drop_items_at_destination_system),
                    faction::compute_faction_storage_system
                        .after(faction::drop_items_at_destination_system),
                    reproduction::pregnancy_system,
                    reproduction::household_formation_system,
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
                    region::detect_edge_crossing_system,
                )
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                (
                    projects::project_lifecycle_system
                        .after(faction::compute_faction_storage_system)
                        .after(construction::chief_directive_system),
                    projects::workforce_budget_system.after(projects::project_lifecycle_system),
                    projects::project_stagnation_system.after(projects::project_lifecycle_system),
                    jobs::chief_job_posting_system
                        .after(faction::compute_faction_storage_system)
                        .after(faction::chief_selection_system)
                        .after(projects::project_lifecycle_system),
                    jobs::worker_self_post_stockpile_system.after(jobs::chief_job_posting_system),
                    crafting::faction_craft_order_system.after(jobs::chief_job_posting_system),
                    jobs::chief_tablet_posting_system.after(jobs::chief_job_posting_system),
                    jobs::job_build_completion_system.after(jobs::chief_job_posting_system),
                    jobs::job_claim_release_system.after(jobs::job_build_completion_system),
                    jobs::chief_post_funding_system.after(jobs::chief_job_posting_system),
                    jobs::job_payout_system.after(jobs::job_claim_release_system),
                    jobs::faction_wage_signal_system.after(jobs::job_payout_system),
                    skills::skill_peaks_tracker_system.after(jobs::faction_wage_signal_system),
                    skills::skill_decay_system.after(skills::skill_peaks_tracker_system),
                    goals::chronic_failure_release_system.after(jobs::job_claim_release_system),
                )
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                (jobs::wage_gossip_system.after(knowledge::awareness_gossip_system),)
                    .in_set(SimulationSet::Economy),
            )
            // Phase 4 (sanitation): emit pass writes `WastePile` intensity
            // into `SanitationMap`; decay pass exponentially shrinks every
            // cell back toward zero. Both gated to daily inside the systems
            // via the SimClock.
            .add_systems(
                FixedUpdate,
                (
                    // Per-agent defecation runs every tick but the
                    // system self-gates per-agent via
                    // `DefecationCadence.last_emit_tick`.
                    sanitation::agent_defecation_system,
                    sanitation::sanitation_emit_system
                        .after(sanitation::agent_defecation_system)
                        .run_if(|clock: Res<schedule::SimClock>| {
                            clock.tick % crate::world::seasons::TICKS_PER_DAY as u64 == 0
                        }),
                    sanitation::sanitation_decay_system
                        .after(sanitation::sanitation_emit_system)
                        .run_if(|clock: Res<schedule::SimClock>| {
                            clock.tick % crate::world::seasons::TICKS_PER_DAY as u64 == 0
                        }),
                    // Phase 5: nonlethal sickness decay (daily). Self-gates
                    // inside the system via `clock.tick %
                    // TICKS_PER_DAY`, so no `run_if` needed here.
                    medicine::sickness_decay_system,
                )
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                opportunity::rebuild_opportunity_index_system
                    .after(faction::compute_faction_storage_system)
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                (
                    survey_task::survey_cursor_system
                        .after(settlement::auto_found_default_settlements_system)
                        .before(settlement::settlement_planner_system),
                    organic_settlement::settlement_pressure_system
                        .after(survey_task::survey_cursor_system)
                        .before(organic_settlement::settlement_morphology_system),
                    organic_settlement::settlement_morphology_system
                        .after(organic_settlement::settlement_pressure_system)
                        .before(organic_settlement::settlement_project_selection_system),
                    organic_settlement::settlement_project_selection_system
                        .after(organic_settlement::settlement_morphology_system)
                        .before(construction::chief_directive_system),
                    organic_settlement::bridge_intent_emitter_system
                        .after(survey_task::survey_cursor_system)
                        .before(construction::chief_directive_system),
                )
                    .in_set(SimulationSet::Economy),
            )
            .add_systems(
                FixedUpdate,
                (
                    settlement::settlement_planner_system
                        .after(survey_task::survey_cursor_system)
                        .before(construction::chief_directive_system),
                    settlement::auto_found_default_settlements_system
                        .before(survey_task::survey_cursor_system)
                        .before(settlement::settlement_planner_system),
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
                    knowledge::discovery_system.after(faction::compute_faction_storage_system),
                    knowledge::tech_teaching_system.after(knowledge::awareness_gossip_system),
                    faction::sync_faction_techs_from_chief_system
                        .after(faction::chief_selection_system)
                        .after(knowledge::discovery_system)
                        .after(knowledge::tech_teaching_system),
                    military::expire_rally_points_system,
                    teaching::apply_lecture_request_system,
                    faction::chief_hunt_order_system.after(faction::compute_faction_storage_system),
                    // Let higher-specificity profession passes claim scarce
                    // None candidates before the general hunter floor fills
                    // leftover labour slots.
                    faction::faction_hunter_assignment_system
                        .after(faction::chief_hunt_order_system)
                        .after(medicine::chief_healer_assignment_system),
                    faction::chief_bureaucrat_appointment_system
                        .after(faction::compute_faction_storage_system),
                    faction::chief_craft_assignment_system
                        .after(faction::chief_bureaucrat_appointment_system),
                    faction::bureaucrat_salary_tick_system
                        .after(faction::chief_craft_assignment_system),
                    faction::tribute_payment_system.after(faction::bureaucrat_salary_tick_system),
                    faction::household_contract_posting_system
                        .after(faction::tribute_payment_system),
                    corpse::corpse_decay_system,
                )
                    .in_set(SimulationSet::Economy),
            )
            // Phase 5b (wage-aware-labor-market-v2): daily apprentice
            // progress + graduation. Ordered after the Crafter
            // assignment system so a freshly-bound apprenticeship lands
            // before the progress sweep reads it. Separate add_systems
            // call — the Economy tuple above is at Bevy's 20-element
            // ceiling.
            .add_systems(
                FixedUpdate,
                (
                    // Heal-5: chief Healer assignment. Same Economy
                    // cadence as the other specialized-labour systems.
                    // Promotes 1 Healer per `HEALER_PER_INJURY_DIVISOR`
                    // injured members, capped at `member_count /
                    // HEALER_MAX_DIVISOR`. Honors the survival override.
                    // Lives in this second Economy block because the
                    // primary tuple sits at Bevy's 20-element ceiling.
                    medicine::chief_healer_assignment_system
                        .after(faction::chief_craft_assignment_system),
                    apprenticeship::apprentice_progress_system
                        .after(faction::chief_craft_assignment_system)
                        .after(medicine::chief_healer_assignment_system),
                    // Phase 4b unified cross-profession switcher: agents
                    // can move directly between Hunter / Bureaucrat /
                    // Crafter when their EV in another role exceeds
                    // their current by 20%. Runs after the four
                    // per-profession assignment systems so the legacy
                    // None ↔ X transitions settle first, then the
                    // switcher picks up X → Y reassignments.
                    profession_choice::cross_profession_switch_system
                        .after(faction::faction_profession_system)
                        .after(faction::faction_hunter_assignment_system)
                        .after(faction::chief_bureaucrat_appointment_system)
                        .after(faction::chief_craft_assignment_system)
                        .after(apprenticeship::apprentice_progress_system),
                    // Phase 6 (inspector surfacing): emit ProfessionChanged
                    // activity-log entries on real transitions. Ordered
                    // after every profession-mutation system on the
                    // Economy schedule so a promote-then-graduate within
                    // the same tick collapses to one log entry.
                    profession_choice::profession_change_log_system
                        .after(profession_choice::cross_profession_switch_system),
                )
                    .in_set(SimulationSet::Economy),
            )
            // Land ownership Phases 1 & 4: plot carving runs after the
            // settlement planner so any new/replanned `SettlementPlan`
            // is visible to the carve pass on the same tick. Listing +
            // acquisition follow on, ordered listing-then-acquire so a
            // freshly published listing can be claimed in the same
            // tick. Split into its own `add_systems` call — the Economy
            // tuple above sits at Bevy's 20-element ceiling.
            .add_systems(
                FixedUpdate,
                (
                    settlement::settlement_peak_population_system
                        .after(settlement::auto_found_default_settlements_system),
                    // P1b: spawn one Camp per Camp-mode (nomadic) faction.
                    // Sibling to `auto_found_default_settlements_system`;
                    // ordered before the planner so any subsequent camp-aware
                    // logic on the same tick sees the live entity.
                    camp::auto_found_default_camps_system
                        .before(settlement::settlement_planner_system),
                    land::carve_plots_system
                        .after(settlement::settlement_planner_system)
                        .before(construction::chief_directive_system),
                    land::land_listing_system.after(land::carve_plots_system),
                    land::household_land_acquisition_system.after(land::land_listing_system),
                    land::rent_collection_system.after(land::household_land_acquisition_system),
                    land::evicted_plot_cleanup_system.after(land::rent_collection_system),
                    // Farm plot ↔ farmer matching (chief mode). Runs after
                    // plot carving so newly-carved Agricultural plots are
                    // visible, and before chief_job_posting_system so that
                    // system can read the assignments to post plot-scoped
                    // Farm jobs.
                    farm::chief_farm_plot_assignment_system
                        .after(land::carve_plots_system)
                        .before(jobs::chief_job_posting_system),
                    // Nomadic mode (Phase 8). Runs after storage rollup so
                    // the migration trigger reads fresh `faction.storage`
                    // numbers, and after household systems so a
                    // sedentarized-then-relocated band's children-factions
                    // see the new home_tile next tick.
                    nomad::nomad_migration_system.after(faction::compute_faction_storage_system),
                    // Phase D: roll Surveying → PendingCommit once the
                    // scout window has elapsed. After the trigger so a
                    // freshly-entered Surveying state doesn't race-promote.
                    nomad::nomad_survey_completion_system.after(nomad::nomad_migration_system),
                    // P5: band-level inventory equalization. Runs every
                    // quarter-day so a daily migration trigger has at
                    // least one prior balance pass to draw on. Gated by
                    // `caps.storage` ∈ {MemberPool, Hybrid} inside the
                    // system itself; settled factions are no-ops.
                    nomad_pool::nomad_band_pool_balance_system
                        .after(faction::compute_faction_storage_system),
                    // Phase 11. Sedentarization candidate check — runs
                    // after migration trigger so a band that JUST set
                    // `pending_migration` this tick is ineligible (won't
                    // flip lifestyle and lose its move).
                    nomad::nomad_sedentarize_system.after(nomad::nomad_migration_system),
                    // P2: slim nomad chief — queues replacement Bedroll/
                    // Tent/Yurt blueprints when shelter falls below
                    // per-member targets. Daily, after migration trigger
                    // so a band about to move skips the work.
                    nomad::nomad_chief_directive_system.after(nomad::nomad_migration_system),
                    // Phase 10: wild herd seasonal drift. Daily, after the
                    // calendar tick — sits next to the nomad migration so
                    // nomads' food-cluster knowledge can pick up the herd's
                    // new leader_tile via vision_system on subsequent ticks.
                    wild_herd::wild_herd_migration_system,
                    // Community-level tech adoption derivation. Runs every
                    // `ADOPTION_DERIVE_CADENCE = 900` ticks (4× per game-day)
                    // after the chief-aware sync so this tick's chief flips
                    // propagate into adoption gates on the same pass.
                    technology_adoption::derive_tech_adoption_system
                        .after(faction::sync_faction_techs_from_chief_system),
                )
                    .in_set(SimulationSet::Economy),
            )
            // Pluralist Economy R8 follow-on: Esteem-driven posting
            // lives in its own `add_systems` call because the main
            // Economy tuple hit Bevy's 20-element `IntoSystem`
            // ceiling. Still ordered after household contracts so
            // a wealthy household-head's individual prestige
            // posting comes after their household's commission.
            .add_systems(
                FixedUpdate,
                (
                    jobs::esteem_driven_posting_system
                        .after(faction::household_contract_posting_system),
                    teaching::self_actualization_teaching_system
                        .after(jobs::esteem_driven_posting_system)
                        .before(teaching::apply_lecture_request_system),
                    // Pluralist Economy R10 follow-on: trader trade
                    // execution + plan creation. Exclusive system —
                    // calls `trader_buy_at_settlement` /
                    // `trader_sell_at_settlement` which require
                    // `&mut World`. Ordered after household contracts
                    // so trade traffic settles into market state
                    // before the next tick's price update.
                    //
                    // Rate-limited to 1 Hz (every 20 ticks at 20 Hz
                    // fixed update). Arbitrage opportunities don't
                    // shift within a single tick, and the O(N²) gap
                    // scan + exclusive-world borrow makes this a
                    // measurable per-frame cost when run every tick.
                    trader::trader_market_step_system
                        .after(teaching::self_actualization_teaching_system)
                        .run_if(|clock: Res<schedule::SimClock>| clock.tick % 20 == 0),
                    gather_claims::gather_claim_expiry_system,
                    shared_knowledge::cluster_decay_system,
                    knowledge::cluster_tier_promotion_system
                        .after(knowledge::awareness_gossip_system),
                    // P4: settled→nomadic collapse. Daily, before the
                    // lifecycle drain so a same-tick failing sample can
                    // queue + execute its SwitchArchetype event in one
                    // pass.
                    sedentary_collapse::sedentary_collapse_system
                        .after(nomad::nomad_sedentarize_system),
                    // P3: drain SettlementLifecycleEvent queue (today
                    // only `nomad_sedentarize_system` emits — must run
                    // after it). Exclusive World; consumes the queue.
                    lifecycle::process_settlement_lifecycle_system
                        .after(sedentary_collapse::sedentary_collapse_system),
                )
                    .in_set(SimulationSet::Economy),
            );
    }
}
