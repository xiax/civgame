# Simulation (`src/simulation/`)

Agent AI, factions, knowledge, hunting, typed-task pipeline, pluralist economy. See root `CLAUDE.md` for cross-cutting `SimulationSet` ordering and tile/Z conventions.

## Game speed (`speed.rs`)

`GameSpeed { current, last_unpaused }` is the player-facing state; `SpeedPreset::{Paused, Normal, Fast (2×), VeryFast (5×)}`. `sync_game_speed_to_virtual_time` (PreUpdate, change-detected) mirrors it onto `Time<Virtual>`. Higher presets make FixedUpdate fire more often per real second — every per-tick decay, cooldown, calendar advance, and `tick % CADENCE == 0` Economy system scales naturally. `SimClock.scale_factor()` carries bucket compensation (`population / bucket_size`) only. `handle_speed_keybinds_system` maps `Space` → pause toggle and `1/2/3` → presets, egui-focus-gated. Per-preset CPU budgets via `SpeedPreset::budget_ms_per_tick()`.

## Agent AI (Goals → HTN → Tasks)

End-to-end through HTN.

- **Goals (`goals.rs`):** high-level objectives (`Survive`, `GatherWood`, `GatherStone`, `GatherFood`, `Haul`, `Defend`, `Farm`, `Build`, `Socialize`, `Sleep`, `MigrateToCamp`, …).
- **HTN registry (`htn.rs`):** per-goal decomposition. Methods carry `precondition` / `utility` / `expand(ctx) -> Vec<Task>`; dispatcher argmaxes and routes the head, prefetching the tail on `ActionQueue`.
- **Tasks (`tasks.rs`):** the agent's *current* action (`TaskKind` enum). `Idle` between tasks.
- **`Disposition { entrepreneurial, gregariousness, curiosity, martial }`** (`u8`, scattered at spawn) drives per-agent preferences.
- **`GoalClass`** (`Survival > Subsistence > Safety > Belonging > Esteem > Enterprise > Discretionary`, `Ord`-derived) tiers goals so registry argmax picks `Survival → Sleep` over `Enterprise → Craft` regardless of raw score.
- **`GoalScorer` trait + `GoalScorerRegistry` resource**: pluggable scorers (`SurvivalHunger`, `Sleep`, `ReturnSurplus`, `Social`, `Play`, `TameHorse`, `PersonalBuild`, `CraftDemand`, `Stockpile`, `HealNeed`, `ProvideCare`, `EarnIncome`, …). `register_default_scorers` runs at `SimulationPlugin::build`. Continuous utility curves (`utility_curves.rs`) anchored on legacy `HUNGER_*` / `SLEEP_*` cliffs. Per-scorer disposition lifts live in one tunables block at the top of `goal_scorers.rs`.
- **`goal_update_system` is the sole goal-selection path.** Builds `GoalScoringContext` from precomputed gates + `Calendar::time_phase()` and calls `registry.best_with_incumbent(...)`. Hysteresis: `GOAL_CHALLENGER_MARGIN = 0.10` dampens single-tick flips. `interrupt_policy_allows` gates replacement. Legacy imperative cascade survives as `fallback_pick` for SOLO / fixtures.
- **`ScorerInputs` SystemParam** bundles registry, `JobBoard`, `OpportunityIndex`, `DecisionMetrics`, per-agent queries to stay under Bevy's 16-param ceiling.
- **Opportunity index (`opportunity.rs`)** rebuilt every 20 ticks in Economy after `compute_faction_storage_system`. Producers emit `PaidJob` / `MaterialDeficit` / `FoodSource` / `CareNeed`. `EarnIncomeScorer` and `ProvideCareScorer` read it first, with direct fallback.
- **Opportunistic en-route interrupts (`opportunistic.rs`):** scorers opt in via `GoalScorer::opportunistic()`. `opportunistic_interrupt_system` (ParallelA, 20-tick cadence) flips the goal when an eligible scorer scores above `OPPORTUNISTIC_INTERRUPT_THRESHOLD (0.50)` and the prior goal isn't on `GoalCooldown`. Skips `MF_UNINTERRUPTIBLE` chains and `JobClaim`-locked agents.
- **HTN disposition lift:** `Method::disposition_lift(...)` multiplies utility (cap 1.3 to stay below tier breakpoints). Overridden by Socialize / Hunt / Play methods. `dispatch_for_goal(...)` is the shared argmax helper.
- **Goal-update cadence:** active agents re-evaluate every 200 ticks; idle agents bypass the gate. Target validation is narrow — only despawned `target_entity` invalidates. Stage/kind drift on a live target is caught at arrival.

### Healing pipeline (`medicine.rs`)

- **`Injury { severity, applied_tick, last_damage_tick }`** derived reactively from `Health.current < Health.max` by `injury_tracking_system` (observer on `Changed<Health>`); clears at full restore.
- **Goals:** `AgentGoal::{SeekCare, ProvideCare}`. `HealNeedScorer` is **Survival** (Safety wouldn't preempt routine craft); `ProvideCareScorer` (Subsistence 0.65) fires for Healer/Apprentice when any same-faction injured exists.
- **Provider chain:** `htn_provide_care_dispatch_system` (ParallelB) routes nearest patient within `HEAL_SCAN_RADIUS = 12` → `Task::Heal { patient }`. `heal_task_system` (Sequential, after combat) requires chebyshev ≤ 1, ticks `Injury.severity -= 1`, grants Medicine XP, removes `Injury` at zero.
- **Patient chain:** `htn_seek_care_dispatch_system` routes to nearest faction-owned `Shrine` via `WorkshopOwnership`; short-circuits within `SEEK_CARE_AT_SITE_RADIUS = 6`.
- **Chief auto-promotion:** `chief_healer_assignment_system` (Economy, daily quarter) mirrors `chief_craft_assignment_system`; target is injury-driven (`HEALER_PER_INJURY_DIVISOR=4`, capped by `HEALER_MAX_DIVISOR=3`). Sub-threshold Medicine routes through `ApprenticeProgress { target_profession: Healer }`.

### Thirst pipeline (`drink.rs` + `sanitation.rs` + `medicine.rs`)

`Needs.thirst` decays at ~2× hunger rate, gated by `THIRST_TRIGGER` / `THIRST_SEVERE`. Never damages `Health`.

- **`ThirstScorer`** (Survival) fires `AgentGoal::Drink`; `htn_drink_dispatch_system` tries inventory `clean_water` first, then nearest `WellMap` entry (chebyshev-adjacent stand tile, treated as clean by default), then `nearest_fresh_drinkable_tile` (chebyshev rings, skips salt via `world::biome::water_kind_at`; severe widens scan 2×).
- **`drink_task_system`** consumes from hands or sips an adjacent fresh tile / well; raw drinks bump `Sickness.severity` 60, contaminated tiles 140 (well tiles read `SanitationMap::is_contaminated` exactly like river tiles, so wells without a nearby `Latrine` get tainted by `WastePile`). Multi-sip loop mirroring `eat_task_system`: drinks until `thirst ≤ DRINK_SATIETY_FLOOR (40)`, source exhausted, or `MAX_SIPS_PER_ACTION (4)` reached — one dispatch fully quenches the agent. Sickness severity scales with sips taken. `ThirstScorer` uses `utility_curves::thirst_utility` (two-stage smoothstep, weights 0.60/0.40) so urgency at `THIRST_TRIGGER` ≈ 0.60 — at-or-above hunger's curve.
- **`BuildSiteKind::Well`** (right-click → Build Well, 4 stone + 2 wood, `WELL_DIGGING` Neolithic). Wells live in `WellMap`; finalize is bare-public (no `OwnedBy` / `WorkshopKind`) and never mutates the tile. `DrinkSource::Well { tile }` is the new typed variant; `organic_settlement::SettlementPressureKind::WaterAccess` queues wells when local fresh water is far or population > 40 / 90.
- **Boiling**: `2 raw_water + 1 wood → 1 clean_water` (CraftRecipe 13, `FIRE_MAKING`, Workbench).
- **`SanitationMap`**: sparse `(tile → f32)` populated daily from `WastePile` via `1/(d²+1)` falloff in `CONTAMINATION_RADIUS = 6`; decays `2^(-1/4)`/day; `is_water_contaminated` above `0.5`. `WastePile` emerges via `agent_defecation_system` (one per `DEFECATION_INTERVAL_TICKS = 2 days`, staggered by entity index). `Latrine` within `LATRINE_ROUTING_RADIUS = 8` marks the pile `LatrineContained`. `BuildSiteKind::Latrine` (right-click → Build Latrine, 2 wood + 1 stone).
- **`Sickness`** (orthogonal to `Injury`): daily decay `-16`; `sickness_work_factor(severity) → [0.5, 1.0]` multiplies movement-driven work progress.
- **Animals:** `AnimalNeeds.thirst`, `AnimalState::Drinking`, `animal_water_seek_system` + `animal_drink_system`.

### Needs

7 needs (hunger, thirst, sleep, shelter, safety, social, reproduction), `[0,255]`, decay over time. Maslow tiers (`Physiological → Safety → Belonging → Esteem → SelfActualization`) layer on top via `MaslowTier::next_unmet(needs)`. Decay rates anchored on the 180-sec game-day: `HUNGER_RATE = 2.0/s`, `SLEEP_RATE = 1.2/s` paired with `SLEEP_RECOVER_RATE = 6.0/s` (12.0 on a bed). `WILLPOWER_WORK_DRAIN = 1.8/s`, `WILLPOWER_IDLE_DRAIN = 0.15/s`. `con_scale` (Constitution) attenuates hunger+sleep decay (floor 0.25×).

## Method-design rules

- **Scoring = viability + distance, with time-of-day/fatigue scaling for AcquireFood / AcquireGood{wood,stone}.** Base tiers: `UTIL_BASELINE=1.0` / `UTIL_VISIBLE_GROUND=1.5` / `UTIL_CLAIMED_HAUL=2.0` / `UTIL_EXPLORE_FALLBACK=0.3`. Chebyshev distance penalty capped at `MAX_DIST_PENALTY=0.30`. Recent failures bias via `MethodHistory` + `score_method_with_history` (`METHOD_FAILURE_PENALTY=0.4`, ring len 6, TTL 600 ticks). Two stacked failures push a baseline method below the Explore fallback. **Context-aware penalty:** `ScoringScope::ContextAware { time_phase, dusk_remaining, fatigue }` multiplies the geometric value by time-of-day (Day 1.0 / Dawn 1.10 / Dusk ramp / Night 4.0) and fatigue (`1.0 + fatigue`). Cap rises to `MAX_DIST_PENALTY_NIGHT=1.50` only at Night.
- **In-hand fast-path for haul dispatchers.** `htn_acquire_good_dispatch_system::Haul` and `htn_build_claimed_blueprint_dispatch_system` Path B check `Carrier + EconomicAgent` inventory before `WithdrawMaterial`; if already carrying, dispatch `HaulToBlueprint` directly. **Never count agent inventories in posting creation or chief blueprint-candidate scoring** — those layers read deposited storage only.
- **Farming is `Farm`-goal only.** Both halves HTN-driven (`PlantFromStorage`, `HarvestPlant`). Seed↔plant mapping centralised in `PlantKind::seed_resource()` + `PlantKind::ALL`.
- **`MF_UNINTERRUPTIBLE`** marks methods whose multi-leg chain must survive across goal-dispatch ticks.

## Player command authority (`player_command.rs`)

UI emits `PlayerCommandEvent` (event bus). Sim drains in `SimulationSet::Input` (exclusive), attaches `Commanded { command, status, issued_tick, command_id }`, then `dispatch_player_command_system` (ParallelB) routes the command into the typed-task pipeline. **All routing is sim-side** — UI never mutates `PersonAI` / `ActionQueue` / authority markers directly.

- **Variants:** `Move`, `Gather`, `Mine`, `Build`, `Deconstruct`, `DigDown`, `PickUpItem`, `PickUpCorpse`, `AttackEntity`, `Teach`, `HoldLecture`, `ReadItem`, `EncodeTablet`, `Muster`, `Disband`, `MilitaryMove`, `MilitaryAttack`, `PackCamp`, `PitchCamp { tile, z }`. Pack/Pitch are faction-scoped (chief actor); dispatcher writes into `nomad::PendingCampOps`.
- **Goal forcing replaces filter scatter.** `goal_update_system` reads `Commanded` via `GoalValidationQueries.commanded_q`; non-terminal forces `AgentGoal::FollowingPlayerCommand`. HTN dispatchers don't recognize the goal so they naturally skip.
- **Lifecycle (`player_command_lifecycle_system`):** Move/MilitaryMove → chebyshev arrival; Gather → target despawned OR agent idle; Mine → tile no longer Wall/Stone; Build → blueprint despawned; PickUp* → target gone; Attack/MilitaryAttack → foe dead. `reap_terminal_commands_system` strips `Commanded` once status is terminal; UI reads `RemovedComponents<Commanded>` for feedback. **MilitaryMove arrival reads `PersonAI.dest_tile`** (per-actor slot tile), not the click anchor.
- **Multi-unit `MilitaryMove` formations (`military/formation.rs`).** Events with `actors.len() > 1` expand into a compact ring of per-actor slot tiles. `expand_military_move_system` runs `plan_compact_ring(anchor, n, is_passable)` + `greedy_assign`; output in `PendingFormationSlots`. `dispatch_player_command_system` routes each actor to its slot + inserts `MilitaryFormationSlot { anchor, slot_index, group }`. Anchor seeds `HotspotKind::RallyPoint`. `MilitaryFormationGroupGen` allocates one `u32` group id per dispatch. `MilitaryAttack` is deliberately untouched.
- **Faction-level commands** (e.g. `EncodeTablet`) carry empty `actors` and apply directly in `drain_player_command_events_system`.
- **Adding a new order type:** new `PlayerCommand` variant + `dispatch_one` arm + `player_command_lifecycle_system` arm + UI button emits the event.

## ActionQueue and typed Task variants (`typed_task.rs`)

`aq.current: Task` is canonical "task running now"; `Task::Idle` default. Behind it sits a fixed `queued: [Task; 4]` prefetch ring (private; access via `enqueue` / `pop_next` / `peek_next` / `queued_len` / `clear_queued`).

- **Producers** (HTN dispatchers, `ui/orders.rs`, `teaching.rs::ReadItem`, legacy-only producers `building_upgrade_system` / `terraform_dispatch_system` / `military_task_system` reroute) route through `aq.dispatch(task)` — enqueues then promotes head into `current` if `current == Idle`. Returns `DispatchOutcome::{Promoted, Queued, Rejected}`. `Rejected` (queue full) fires `debug_assert!` inside `dispatch`; silent burial is no longer possible. Most callers ignore the outcome since `Queued` is legitimate for chain prefetch.
- **Consumers** (executor exit paths in `gather.rs`, `dig.rs`, `corpse.rs`, `construction.rs`, `items.rs`, `production.rs`, `teaching.rs`, plus `MilitaryMove` arrival) call `aq.advance()` instead of writing `current = Idle`. Canonical exit helpers: `aq.finish_task(&mut ai)` (success — `state = Idle` + `work_progress = 0` + advance) and `aq.cancel_chain(&mut ai)` (chain abort — same fields + cancel).
- **External preempts** (`dispatch_player_command_system`, hunter demote, stale reset, `movement::release_to_idle` on path failure / stranded-Z recovery, `combat::combat_retaliation_cleanup_system` draining `CombatRetaliationStartedEvent`) call `aq.cancel()` / `aq.cancel_chain(&mut ai)`, dropping both `current` and queue. The retaliation cleanup runs in `Sequential` right after `combat_system` because `combat_system`'s `attacker_query` holds `Option<&mut ActionQueue>` mutably across iteration, so the victim's `ActionQueue` is only reachable post-iteration via the event-deferred system. `goal_update_system` overrides (Lead/Defend/Raid/Rescue/Scout/Migrate chief / commanded forced-flip / earnincome override) must also call `aq.cancel()` — otherwise the typed channel carries a stale task that the next dispatcher's `aq.dispatch` would silently park behind. **Debug-time enforcement:** `aq.dispatch` fires a `debug_assert!` when the incoming task shares a `TaskKind` with `current` *and* the queue is empty — the canonical stale-typed-channel pattern. Legit chain prefetch interleaves variants and always has `queued_len > 0` at that point, so the assert doesn't fire on it.
- Per-tick "pin" sites (lecture/teach pin writes) stay as direct `aq.current = X` writes — idempotent re-assertions.

**Variants:** `Idle`, `WalkTo { tile, z, why }`, `WithdrawGood { filter }`, `WithdrawMaterial { resource_id, qty }`, `WithdrawFood { tile }`, `Equip { slot, resource_id }`, `Construct { blueprint }`, `ConstructBed { blueprint }`, `Deconstruct { tile }`, `Terraform { tile }`, `MilitaryAttack { foe }`, `Gather`, `Dig`, `Scavenge`, `Read/Teach/HoldLecture/AttendLecture`, `PickUpCorpse`, `HuntPartyMuster`, `Hunt`, `HaulCorpse`, `Butcher`, `TameAnimal`, `Sleep`, `Eat`, `HaulToBlueprint`, `HaulToCraftOrder`, `WorkOnCraftOrder`, `DepositToFactionStorage`, `WalkAndTakeFromMember`, `PlayThrow`, `PlayPlant`, `Explore`, `Migrate`, `UnpitchStructure`, `UnloadCampCargo`, `PitchStructureAt`, plus `Lead`/`Defend`/`Raid`/`RescueAlly`/`Socialize`/`Play`.

**Rules:**

- Systems mutating typed task **must** include `&mut ActionQueue` alongside `&mut PersonAI`.
- Every executor exit must `aq.advance()` (success) or `aq.cancel()` (chain-drop). Prefer the bundled helpers `aq.finish_task(&mut ai)` / `aq.cancel_chain(&mut ai)`.
- `military_task_system`, `withdraw_good_task_system`, `withdraw_material_task_system`, `drink_task_system` fall back to `Idle` if `aq.current_task_kind() == X` but `aq.current` is the wrong variant — defence in depth.
- **Reading the task discriminant.** All consumers read `aq.current_task_kind()` (derives `u16` from `aq.current` via `task_kind_for`). The legacy `PersonAI.task_id` mirror is gone; `UNEMPLOYED_TASK_KIND` (`u16::MAX`) is the "no task" sentinel returned for `Task::Idle`.

## HTN domain (`htn.rs`)

- **`AbstractTask` / `AbstractTaskKind`** — high-level goal a method decomposes (`Sleep`, `Eat`, `AcquireFood`, `AcquireGood { resource_id }`, `StockpileFood`, `Scout`, `EquipHuntingSpear`, `ReturnSurplus`, `TameWildHorse`, `PlantFromStorage`, `HarvestPlant`, `ConstructBlueprint`, `Socialize`, `DeliverMaterialToCraftOrder`, `WorkOnCraftOrder`, `HarvestGrainForCraftOrder`, `Play`, `JoinHuntParty`, `EngagePrey`, `DeliverHuntKill`, plus single-step combat/faction goals).
- **`Method` trait** — `precondition` / `utility` / `expand(ctx) -> Vec<Task>` / `flags` / `tech_gate` / `profession_gate` / `policy_gate` / `name`. `policy_gate` returns `&'static [(ResourceId, RequiredFlag)]` for resource-policy gating.
- **`MethodRegistry`** built once in `SimulationPlugin::build` via `register_builtin_methods`. Read-only after init.
- **`MethodFlags`** = `u8` bitmask. **`MethodId`** = `u16` newtype with one `pub const` per registered method (stable identity for `MethodHistory` keying).
- **`MethodHistory`** — per-agent ring (len 6, TTL 600). Spawned at every Person spawn site.
- **Failure-biased scoring:** multi-method dispatchers score via `score_method_with_history(method, abstract_task, ctx, history, now) = utility - failures * 0.4`. Routing/missing-target failures push `FailedRouting` / `FailedTarget`.
- **Cancel paths record failure.** Executor `aq.cancel()` paths that drop a chain push the agent's `active_method` onto `MethodHistory` via `record_target_failure(...)` / `record_routing_failure(...)`. `MethodId::UNKNOWN` is the sentinel for cancels with cleared `active_method`. Sites: `gather::finish_gather`, `items::item_pickup_system`, `items::finish_scavenge`, `production::finish_withdraw_material`.
- **Goal-flip Abandoned (`record_abandoned_method_system`)** ParallelA, after `goal_update_system`, filtered on `Changed<AgentGoal>`. Pushes `Abandoned` on the agent's `active_method` and clears it — unless `MF_UNINTERRUPTIBLE`, in which case `goal_dispatch_system`'s preserve-arms keep the chain alive.
- **Terminal Explore fallback** (`htn_acquire_food_dispatch_system` + `htn_stockpile_food_dispatch_system`): when every precondition fails, fall through to synthetic `Task::Explore { kind: AnyEdible }` rather than returning silently.
- **Reachability-aware deposit pick** (`StorageTileMap::nearest_for_faction_reachable`) and **vision pickers** (`CurrentVision::nearest_gather_target` / `nearest_scavenge_target`) both take an `is_reachable` closure with two-pass fallback (filter, then connectivity-blind). The `source_tile` for deposits is the agent's post-pickup tile.
- **Chronic-failure release** (`goals::chronic_failure_release_system`, Economy daily quarter): agents with ≥3 non-success entries within TTL get the goal stamped on `GoalCooldown` (ring of 4 with `(goal_disc, expires_tick)`). Then bifurcates: claim-holders release the claim; autonomous agents land in `ForceGoalReevaluate` (drained by `goal_update_system` next tick to bypass the 200-tick cadence). `Survive` / `Sleep` are exempt — terminal Explore is the relief valve there.
- **Success recording:** dispatchers stamp `ai.active_method = Some(method.id())` on success; `htn_method_completion_system` observes `current == Idle && queued_is_empty && active_method.is_some()` and records `Success`.

### Methods (highlights)

| Abstract task | Method(s) | Utility | Expansion |
|---|---|---|---|
| `Sleep` | `SleepMethod` | n/a | `[Sleep { bed }]` (own → home → in-place) |
| `Eat` | `EatFromInventoryMethod` | 1.0 | gates on `edible_count > 0 && hunger ≥ EAT_TRIGGER_HUNGER (180)` |
| `AcquireFood` | Withdraw / Scavenge / Forage / Explore | 1.0 / 1.5 / 1.0 / 0.3 | chains end in `Eat` |
| `AcquireGood` | Withdraw / Haul / Gather / Scavenge / Explore | 1.0 / 2.0 / 1.0 / 1.5 / 0.3 | chains end in `DepositToFactionStorage` (or `HaulToBlueprint`) |
| `StockpileFood` | Scavenge / Forage / Explore | 1.5 / 1.0 / 0.3 | end in `DepositToFactionStorage` |
| `Scout` | `ScoutForPreyMethod` | 1.0 | `[Explore { kind: Prey }]` |
| `EquipHuntingSpear` | `WithdrawAndEquipHuntingSpearMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[WithdrawMaterial, Equip]` |
| `PlantFromStorage` | `WithdrawAndPlantSeedMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[WithdrawMaterial, Planter]` |
| `ConstructBlueprint` | Build / Withdraw+Haul / Gather+Haul | 1.0 / 2.0 / 1.0 (all `MF_UNINTERRUPTIBLE`) | claimed-build + personal-bp paths |
| `JoinHuntParty` | Muster / Travel | 1.0 (`MF_UNINTERRUPTIBLE`) | `HuntPartyMuster` or `Explore { Prey }` |
| `EngagePrey` | Hunt / PickUpFreshCorpse | 1.0 / 1.5 (`MF_UNINTERRUPTIBLE`) | `Hunt` or `PickUpCorpse` |
| `DeliverHuntKill` | `DeliverHuntKillMethod` | 1.0 (`MF_UNINTERRUPTIBLE`) | `[HaulCorpse, Butcher]` |
| `Play` | 6 methods (partner/solo/throw/toy/plant grain/plant berry) | 1.0–1.5 | varies; storage-fed `MF_UNINTERRUPTIBLE` |

### Dispatch + chain handoffs

All dispatchers run `ParallelB` after `goal_dispatch_system`. Each gates on goal + Idle + non-Dormant + `aq.is_idle()`, builds `PlannerCtx`, runs argmax, routes the head via `assign_task_with_routing` + `aq.dispatch(...)`, prefetches the tail. SOLO/unsettled skip storage-dependent methods. Spear-arming runs *before* food dispatchers so unarmed hunters fetch a spear first.

Trailing legs of multi-task chains live in exit helpers on the head executor:

- **`production::finish_withdraw_food`** primes `Eat`.
- **`production::finish_withdraw_material`** releases reservation; routes `HaulToBlueprint` / `HaulToCraftOrder`; primes `Equip` / `PlayThrow` / `Play`.
- **`gather::finish_gather`** (5 exit sites) routes `DepositToFactionStorage` / `HaulToBlueprint` / `HaulToCraftOrder`; primes `Eat`. Takes `FinishGatherOutcome::{Completed, TargetInvalid}` — `TargetInvalid` cancels the chain so empty-handed walks don't happen.
- **`items::finish_scavenge`** is the mirror; primes `Eat`. Same `FinishGatherOutcome` parameter.
- **`drop_items_at_destination_system`** executes `TaskKind::DepositResource`, credits `JobClaim::Stockpile`, `aq.advance()`.

### Stale-reset (`goal_dispatch_system`)

Runs before HTN dispatchers. Stale `aq.current_task_kind() != UNEMPLOYED_TASK_KIND` clears with `aq.cancel()` unless a preserve-arm for the (goal, task-kind) pair applies. Catch-all resets `Explore` arrivals.

### Adding a method

New `AbstractTask` variant (if needed) + `Method` impl + register call in `register_builtin_methods`. Method tests in `htn::tests` exercise the trait directly without an `App`. New abstract-task *kinds* also need a per-goal dispatch system.

## Memory & gathering (`shared_knowledge.rs`)

3-tier `SharedKnowledge` map is the system of record for static-resource queries.

- **`SharedKnowledge`** keyed by `KnowledgeTier::{Household(u32), Settlement(SettlementId), Faction(u32)}`. Resources stored as `ResourceCluster` influence nodes (center + radius + LRU `representative_tiles[4]` + `estimated_count`). `report_sighting` merges within `CLUSTER_MERGE_RADIUS=8`; `report_depleted` decrements + despawns at zero. `nearest_in_tier_set(...)` walks finest-tier-first. `cluster_decay_system` drops clusters older than `CLUSTER_DECAY_TTL_TICKS=25200` daily.
- **Cluster-saturation skip:** dispatchers inject `gather_claims.cluster_is_saturated(...)` to skip clusters at `MAX_PARALLEL_GATHERERS_PER_CLUSTER = 3`. **Pressure-aware rep selection** picks the least-pressured rep slot.
- **`ResourceOwner`** — `Public | Person | Household | Settlement | Faction`. `is_accessible_to(viewer, ...)` gates harvest.
- **`LandClaim` on Plants** stamped at planting: `HouseholdMember` → Household; chief Farm posting → Faction; else → self. Wild reseed → no claim.
- **`GatherClaims` (`gather_claims.rs`)** — per-`(tile, kind)` reservation. `pressure(tile, now, viewer)` feeds `claim_penalty` ×4 weight. Staked at dispatch; released at every `gather::finish_gather` exit and stale-cancel. `CurrentVision::nearest_gather_target` consumes the same weight.
- **Vision write-through (`memory.rs::vision_system`)** — active agents report plant/item/prey/stone sightings to the finest tier (Household if member, else Faction). Settlement/Faction tiers materialise via gossip. Vision is additive only — `report_depleted` fires from gather-arrival, never vision.
- **`CurrentVision` vision-first dispatch** — `vision_system` populates `CurrentVision { entries: Vec<VisionEntry> }` (cleared per bucket-active pass). Food ground items dual-write under `AnyEdible` + `Resource(rid)`. Resource-target dispatchers consult `nearest_gather_target` / `nearest_scavenge_target` before `gk.nearest_target_tile`.
- **Stale-target neighbor-scan retarget (`gather.rs`):** when `gather_system` arrives to find the plant gone, `retarget_neighbor` scans chebyshev `P6B_RETARGET_RADIUS = 2` for a same-kind mature plant; one swap per `P6B_RETARGET_COOLDOWN = 40` ticks.
- **HTN under-foot fast path** (`plants.rs::nearest_mature_plant_under_agent`): probes `PlantMap` directly within radius 2, gated on `pressure == 0`. Wired into `htn_acquire_food_dispatch_system` and `htn_stockpile_food_dispatch_system`.
- **LOS (`line_of_sight.rs`)** — 3D Bresenham at eye height `current_z + 1`. Shared by fog + vision.
- **Tier gossip promotion (`knowledge.rs`)**: `HouseholdMember` socialising within 3 tiles of a same-root-faction `Bureaucrat`/`Chief` promotes household clusters to Settlement tier; two officials → Settlement bubbles to Faction.
- **Cluster-density-gated chief postings** (`chief_job_posting_system::faction_knows_cluster`) walks faction-tier + owned settlement-tier knowledge. No cluster → skip food/Wood/Stone posting. CraftOrder-driven Stockpile and Build are ungated.

`AgentMemory` is ~30 lines: `visited_settlements: [Option<(SettlementId, u8)>; 8]`. `relationship_decay_system` walks `RelationshipMemory` only — `cluster_decay_system` owns cluster freshness.

## Behavioural test fixture (`test_fixture.rs`)

Headless `App` harness for AI assertions without rendering/UI/globe gen. `TestSim::new(seed)`, `flat_world(radius, z, kind)`, `spawn_person(faction, tile, |b| b.hunger(...).add_inventory(...))`, then `tick()` / `tick_n(n)`.

- Time deterministic via `TimeUpdateStrategy::ManualDuration` — one fixed-tick per `app.update()`.
- State stays in `SpawnSelect`; FixedUpdate sim runs regardless.
- Camera at origin so LOD doesn't drop test agents to Dormant.
- Currency-invariant helpers: `set_currency`, `assert_total_currency_invariant(app, baseline, eps)`. Snapshot sums `EconomicAgent.currency + FactionData.treasury + Settlement.treasury + JobEscrow.amount`.
- `inject_faction_sighting` pre-populates a faction-tier cluster. Inject **outside** `VIEW_RADIUS=15` of any test agent or vision will deplete it.

## Faction systems

- **Faction archetypes (`archetype.rs`):** `FactionCapabilities { home, storage, shelter, settlement, posting, land, economic_policy, income, inheritance }` on `FactionData.caps`. `derive_from_legacy(lifestyle, preset, catalog)` mirrors the legacy cross product. Every previously-branchy site consults caps (settlement spawning, chief postings, seeding, migration, storage rollup, inheritance).
- **`FactionArchetypeRegistry`** loaded at `WorldPlugin::build` from `assets/data/factions/archetypes/*.ron`. `core.ron` ships `settled_subsistence` / `settled_mixed` / `settled_market` / `nomadic_subsistence`. `derive_from_archetype_key` is used by `spawn_population`, `bonding_system`, `process_settlement_lifecycle_system`.
- **Lifecycle events (`lifecycle.rs`):** `SettlementLifecycleEvent::{Establish, Abandon, Migrate, SwitchArchetype}` typed queue drained by `process_settlement_lifecycle_system` (exclusive, Economy). `SwitchArchetype` swaps caps + land_policy + economic_policy, cleans up old camp structures within `OLD_CAMP_RADIUS`, bumps `culture_hash`, spawns new storage tile or re-seeds nomadic camp.
- **Storage backend (`economy/storage_backend.rs`):** typed `WithdrawSource` / `DepositTarget` (`GroundTile` / `MemberHands` / `PackBundle`) + `nearest_withdraw` / `nearest_deposit` matching on `caps.storage`. `compute_faction_storage_system` is backend-symmetric (tile pass: FactionTile/Hybrid; member pass: MemberPool/Hybrid).
- **Camp entity (`camp.rs`):** lightweight nomadic counterpart to `Settlement` with `SettlementMarket` + `treasury`, no plots/zones/spine/peak_pop. `auto_found_default_camps_system` (Economy). `MarketNodeRef` + `faction_market_node(...)` resolve a faction's economic node uniformly. `economy::market::camp_price_update_system` mirrors the settlement variant.
- **`FactionTechs`** is a derived projection of the chief's `PersonKnowledge.aware`, rebuilt every Economy tick by `sync_faction_techs_from_chief_system`.
- **Organic settlement AI (`organic_settlement.rs`):** persistent `SettlementBrain` per full settlement tracks phase, anchors, traffic heat, soft districts, planned `road_segments` / `road_tiles`, organic parcels, frontier, seed, layout hash. Economy systems survey terrain/structures, sketch road skeleton, derive `ConstructionIntent`s, select one affordable/allowed intent per faction. `chief_directive_system` consumes `SelectedSettlementIntents` first; legacy `generate_candidates` is fallback.
- **Survey pacing**: `survey_task::survey_cursor_system` (FixedUpdate Economy) replaces the legacy burst. Walks settled non-SOLO settlements one-per-cursor-fire at `interval = (120 / N).max(1)`. Snapshot types (`ChunkSnapshot` / `MapsSnapshot` / `FactionSnapshot` / `MemberOffsets`) are scaffolding for future async pipeline.
- **Unified survey body (`organic_settlement::survey_one_settlement`)** — shared by `settlement_survey_system` (FixedUpdate) and `kickoff_initial_survey_system` (OnEnter, before seeding). Ensures `SettlementBrain` exists before seed picks anchors.
- **Brain-aware seed anchors (`construction::pick_seed_house_anchor`):** with `Option<&SettlementBrain>` parameter, walks Residential parcels, biases `−100` for frontage-edge parcels, falls back to legacy plan-zone scan, then radial spiral. Layout-decision foundation: `walled_house_tile_plan(...)` enumerates wall+door+bed cells consumed by both `plan_building` and `seed_walled_house_at`.
- **`SimulationState::{Warmup, Active}` SubState** flips to `Active` at the tail of OnEnter chain. No system currently gates on `Active` (avoids tick-0 stall); scaffolding for future opt-in.

### Construction (`construction.rs`)

`BlueprintMap`, `WallMap`, `BedMap`. `faction_blueprint_system` decides; `construction_system` consumes resources and finalizes. Every finished structure carries `StructureLabel(&'static str)` from `.label()`; `StructureIndex` (tile→entity) maintained by `on_structure_label_add` / `on_structure_label_remove` hooks.

- **Wall ladder** (`best_wall_material`): Palisade < WattleDaub (`PERM_SETTLEMENT`) < Mudbrick (`FIRED_POTTERY`) < Stone (`COPPER_WORKING`). Walls survive chunk streaming; natural-bedrock fallback walls get `WallMaterial::Stone` + `StructureLabel("Stone Wall")`.
- **Door direction + doormat reservations (`doormat.rs`):** `Door { dir: TileEdge, doormat_tile }`. `Blueprint.door_dir` set by `plan_building` / `plan_composite_building` from `frontage_edge` (or `TileEdge::toward(centre, home)` fallback). `entrance_cell_for_edge` returns the centre cell, never a corner. Finalization writes doormat to `Road` via `write_road_tile` and queues a Bresenham (`doormat → home`) on `RoadCarveQueue` when no Road sits within chebyshev 4. **`DoormatReservations`** consulted by every placement helper (`is_clear_footprint`, `plot_rect_vacant`, `find_palisade_site`, `find_clear_tile_in_zone`, `find_unfilled_civic_zone_tile`, `find_bed_tile_around_hearth`, `seed_farmstead_yard`, `seed_walled_house_at`). `Door`'s `on_remove` hook frees the reservation.
- **Door direction is verified, not assumed.** `pick_clear_door_cardinal` tries the preferred cardinal then falls back ranked by doormat→home chebyshev; returns `None` if every cardinal is blocked (planners abort, seed loop stamps anchor as used). **`doormat_reaches_home`** runs a bounded BFS (1500-node cap) — courtyard-pocketed cardinals fail this gate.
- **Roads are protected from new construction.** All footprint/shape/anchor/perimeter helpers reject `TileKind::Road`. Palisade carvers leave 3-tile gateways for spine flow capacity.
- **Composite footprints (`BuildIntent::CompositeHouse`)**: `plan_composite_building` walks `building_template::shape_tiles(shape, anchor, rotation)`, classifies each cell as Wall / Door / Bed. LShape/UShape support exists, but automatic LShape shelter emission is disabled until the small farmstead mask grows true interior bed cells; growth currently stays on Huts/Longhouses.
- **Civic milestones (`civic_milestones.rs`):** `(Era, peak_pop) → bool` table gates Granary / Shrine / Market / Barracks / Monument / Bridge growth. Seeded structures bypass the gate.
- **Settlement.peak_population** — monotonic max of owner-faction `member_count`, maintained by `settlement_peak_population_system`.
- **Organic layout randomness** (`settlement.rs`): `culture_hash` seeds `fastrand::Rng` for ±1 tile zone jitter. `generate_candidates` rolls Hut/Longhouse (Longhouse forced when `bed_deficit ≥ 4`).
- **`SettlementPlan` compatibility projection:** `settlement_planner_system` prefers `organic_settlement::compat_plan_from_brain` when a brain exists. `SettlementParcelIndex` is authoritative; `SettlementPlan` is transitional/debug.

### Game-start seeding (`seed_starting_buildings_system`)

`OnEnter(Playing)` chain: `spawn_population → auto_found_default_settlements_system → {sync_faction_techs_from_chief_system → derive_tech_adoption_system → seed_prime_tech_adoption_system, settlement_peak_population_system} → kickoff_initial_survey_system → seed_starting_buildings_system → clear_obstacles_under_seeded_structures → mark_warmup_complete_system`.

Era-aware priming runs in two passes:

1. **Reused runtime systems** re-fire once at OnEnter: `sync_faction_techs_from_chief_system` projects chief Aware onto `FactionData.techs`; `derive_tech_adoption_system` fills `tech_adoption` from member knowledges + workshops + recent-use; `settlement_peak_population_system` ratchets `Settlement.peak_population` from spawned `member_count`. Idempotent at tick 0 (cadence gate passes).

2. **Founder-band override (`seed_prime_tech_adoption_system`)** force-stamps every era-prior chief-Aware tech to `AdoptionStage::Adopted` for each faction. Necessary because the runtime `derive_stage` gates fail at tick 0 for default 20-person bands: Specialist scale needs `members ≤ 8` or every adult Learned (only chief + ~1/8 Specialists are Learned), Institutional needs a civic building or every adult Learned. Without this override, `community_adoption_bitset` returns 0 for `PERM_SETTLEMENT` at seed time and `generate_candidates` falls into the Paleo radial-`Single(BuildSiteKind::Bed)` branch regardless of `GameStartOptions.era`. Runtime decay/growth resumes at the first Economy cadence; the seed pass only needs correct adoption at tick 0. Skipped when `seed_buildings == false` (sandbox).

OnEnter coverage lives in `test_fixture::onenter_era_seeding`: drives the real state transition + `app.update()` and asserts `PERM_SETTLEMENT` reaches `Adopted` for Neo / Chalco / Bronze starts, walled houses (Wall + Door) stamp for Neolithic+, Bronze starts stamp Market/Barracks/Monument plus Bronze-grade tiers, and Paleolithic stays in the band-camp branch (regression guard against accidental upgrades from priming).

- Paleo/Meso: deterministic band-camp seeder (`paleolithic_hearth_positions_river_aware` + `seed_paleo_beds_around_hearth`).
- Neolithic+: unified intent loop — drives runtime `generate_candidates` with `seed_techs = techs_through_era(GameStartOptions.era)` and applies via `seed_apply_intent` (direct stamp, no Blueprint+worker round-trip). Same intent stream as `chief_directive_system`; seed mode bypasses civic milestone gates, tries the full ranked candidate list each pass, and relocates blocked single-tile or residential structures nearby so blocked high-score anchors don't starve lower-scored craft/civic intents or starting bed capacity. Residential fallback uses the actual stamped anchor for the farmstead yard.
- Seeded and runtime-finalized Bed/Campfire/Door/Workbench entities use `best_*_for(community_adoption_bitset)` (or seed era techs) instead of component defaults.
- Era-additive: Paleo (1 Campfire + 4 Beds); Meso (+2 beds); Neo (Hearth→Ringed, +Workbench, +Granary, +Loom, beds→8); Chalco (Hearth→Lined, +Shrine, Palisade r=5 with east Door); Bronze (+Market, +Barracks, +Monument, walls→Mudbrick).
- Nomadic factions branch to `seed_nomadic_camp` first.

### Bridges

`BuildSiteKind::Bridge` adds a tile-replacing finalize path (alongside `Wall`): writes `TileKind::Bridge` over the prior `River` cell and spawns `Bridge { restore_tile, faction_id, tile }`. Deconstruct refunds `2 wood + 1 stone` on the nearest passable bank.

- **Recipe**: "Timber Bridge" — 4 wood + 2 stone, 120 work_ticks, `tech_gate = Some(BRIDGE_BUILDING)`.
- **Tech**: `BRIDGE_BUILDING` (Chalcolithic, `AdoptionScale::Institutional`, prereqs `PERM_SETTLEMENT + DUGOUT_CANOE + COPPER_TOOLS`). **Civic gate**: `CivicKind::Bridge` at `(Chalcolithic, 20)` peak pop.
- **Worker adjacency**: `Blueprint.work_stand: Option<(i32, i32)>` caches a chebyshev-1 bank tile. `BuildSiteKind::is_water_anchored()` returns true only for Bridge. `work_stand_for_bridge(...)` populates it at spawn (prefers cardinals, rejects other-Bridge neighbours).
- **AI generation**: `organic_settlement::bridge_intent_emitter_system` scans each faction's `SettlementBrain.road_segments` for river runs of `1..=MAX_BRIDGE_SPAN = 4` bounded by passable banks; one bridge per faction per cadence.
- **River-aware seeding**: `paleolithic_hearth_positions_river_aware` projects offsets through `river_context::project_to_safe_bank` (bounded BFS); rejects `river_distance_at <= 1` flood band. Wired into Paleo/Meso seed + `seed_nomadic_camp`.
- **River-aware organic planner** (`build_road_network`): pre-`BRIDGE_BUILDING`, drops river-crossing segments via `trace_crosses_river` and filters anchors through `same_bank_bfs`. Primary spoke order is `RiverAxis`-aware. Post-tech, filters lift and `bridge_intent_emitter_system` back-fills crossings.

### Obstacle clearing (`obstacle.rs`, `clear_obstacle.rs`)

`ConstructionObstacle { resolution }` blocks construction. Resolutions: `WorkerClear { work_ticks, skill, skill_xp }` (worker `Task::ClearObstacle` adjacent; entity despawns, yields drop) and `Relocate` (`relocate_entity_aside`, spiral chebyshev radius 6). Plants attach `WorkerClear`; loose rocks + `spawn_ground_drop` items attach `Relocate`.

- `populate_pending_clear_system` (Sequential, before construction) observes `Added<Blueprint>`, scans footprint, pushes `WorkerClear` to `bp.pending_clear`, synchronously relocates the rest.
- `construction_system` gates on `bp.obstacles_cleared()`.
- `htn_clear_obstacle_dispatch_system` + `clear_obstacle_task_system` consume work, drop yields, despawn, award XP, `aq.advance()`.
- `terraform::footprint_completion_system` calls `obstacle::resolve_footprint_sync` for interior tiles. Seeded structures bypass blueprints via `clear_obstacles_under_seeded_structures`. `react_obstacle_under_structure_system` handles late-streamed loose rocks.

### Chief directives + jobs

- **`chief_directive_system` concurrency cap:** up to `max(2, members/6)` blueprints per faction (ceiling `MAX_BLUEPRINTS_SAFETY_CAP - 1 = 19`), one new bp per 60-tick window. Pending terraform footprints count. Organic intents tried first; fallback rejects footprints intersecting `SettlementBrain.road_tiles`.
- **Haul posting cleanup**: `construction_system` eager cleanup at deposit moment + `chief_job_posting_system` periodic catch-up (60 ticks) via `Blueprint::slot_satisfied(...)`. Both ignore the "skip postings with claimants" short-circuit for Haul only.
- **CraftOrders + jobs:** `faction_craft_order_system` despawns orders > `CRAFT_ORDER_TIMEOUT_TICKS=600`. `chief_job_posting_system` emits one `JobKind::Craft` per faction, picking the recipe with largest `demand-supply` (tech + station gated). Ingredient gate uses `faction.storage.stock_of()` (deposited stock only, not `resource_supply` which double-counts inventories). Missing inputs pull-post `Stockpile { resource_id: missing_input }` for each non-Wood/Stone shortfall.
- **Multi-worker postings (`jobs.rs::posting_target_workers`):** Build=3, Stockpile material=`(target/4).clamp(2,6)`, Stockpile food=`(target/80).clamp(2,8)`, Haul=2, Farm=3, Craft=1.
- **Posting target curves:** `GATHER_TARGET_CAP=1500`, `MATERIAL_GATHER_CAP=96`. Food multiplied by `food_seasonal_multiplier` (Spring/Summer 1.0, Autumn 1.5, Winter 1.3).
- **WithdrawMaterial intent:** dispatcher picks the resource. Each dispatcher reads `FactionStorage.totals` + `StorageReservations` to find the nearest tile with effective stock.
- **`StorageReservations`** (`(tile, ResourceId) → reserved_qty`, mutex-wrapped). Successful `WithdrawMaterial` increments + stashes `(reserved_tile, reserved_resource, reserved_qty)` on `PersonAI`; `release_reservation` decrements on every teardown.

## Knowledge & technology

- **`PersonKnowledge`** carries `aware` and `learned` (`u64` bitsets, learned ⊆ aware) + `learned_at: [u32; 64]`. `complexity(tech)` is era-based (Paleo 1 → Bronze 5; Cuneiform/Lunar/City-State 6). `learning_slowdown(stats, k) = 1 + complexity_used / (intelligence × 2)` is applied to `add_study_progress` and passive teaching's chance. Newborns inherit `Aware ∪ Learned` from both parents. `study_progress: AHashMap<TechId, u32>`; at `study_threshold(tech) = complexity * STUDY_TICKS_PER_COMPLEXITY (3600)` → `try_learn`.
- **Discovery (per-action):** `discovery_system` consumes `DiscoveryActionEvent`s. Eligible = prereqs Learned + not already Aware. Roll `base × (1 + INT_mod × 0.1) × (1 + skill_xp / 1000)`, cap 0.5; success grants Aware and jump-starts `study_progress` by `complexity × INSIGHT_PROGRESS_PER_COMPLEXITY (1200)` capped below threshold.
- **Awareness gossip:** `awareness_gossip_system` (Economy) ORs adjacent socialising agents' `aware` bitsets and `AgentMemory.visited_settlements`.
- **Passive teaching:** `tech_teaching_system` scans pairs within 3 tiles, rolls `0.004 × INT_scale / learning_slowdown(student)` per shared Aware-but-not-Learned tech. Highest-complexity teachable lands directly in Learned.
- **Directed teaching (`teaching.rs`):** **Read** `+1`/tick; **Lecture** drafts ≤8 same-faction adults within 6 tiles, `+2`/tick for 600 ticks; **1-on-1 Teach** `+3`/adjacent-tick. All carry `Drafted`.
- **Community adoption (`technology_adoption.rs`):** `FactionData.tech_adoption: [AdoptionStage; TECH_COUNT]` derived by `derive_tech_adoption_system` (Economy, every `TICKS_PER_DAY/4`). Six stages: `Unknown / Rumored / Demonstrated / Practiced / Adopted / Institutionalized`. Per-tech `AdoptionScale` (Personal / Household / Subsistence / Specialist / MilitaryTransport / Institutional) drives thresholds. `RecentTechUse` per-(faction, tech) ring (cap 8, TTL 60 days). Gating surface: `can_direct_tech(faction, tech)` = chief-Aware (planning); `community_has_adopted(faction, tech)` = civic/tier/material gates; `worker_can_perform(person, tech)` = Learned. Per-person execution uses `has_learned`.
- **Decay:** `derive_tech_adoption_system` is symmetric — conditions falling away walk the stage down. Downgrades throttled to one stage per game-day; upgrades immediate.
- **Founder seeding (`PersonKnowledge::seeded_realistic_through_era`):** chief learns Personal+Household+Subsistence+Specialist+Institutional through era ≤ E; ~1/8 members Specialist (+MilitaryTransport); rest Personal+Household+Subsistence Learned. Everyone Aware of the full era.
- **Tablets and books:** `Good::ClayTablet` and `Good::Book` carry `Item.tech_payload: Option<TechId>`. Recipes gated on `CUNEIFORM_WRITING`, Workbench. Reading doesn't consume.
- **Tablet posting (`chief_tablet_posting_system`):** every 3600 ticks. Picks highest-complexity chief-Learned bit that <50% of adults are Aware of, posts `JobKind::Craft` with `tech_payload`. Player override via `PlayerCraftRequest` (`JobSource::Player`, priority 180).
- **Tech tree (`technology.rs`):** 44 techs, 5 eras, prereq DAG, per-tech `triggers: &[TechTrigger]`. `TechBonus` adds yield/storage/combat bonuses; bonuses aggregate via `faction.techs` (chief-aware).

## Hunting pipeline (`corpse.rs` + HTN)

- Wolf/Deer no longer drop Meat/Skin on death. `combat.rs::death_system` strips AI/needs/species and inserts `Corpse { species, fresh_until_tick }`. `corpse_decay_system` despawns at `CORPSE_FRESHNESS_TICKS=600`.
- **Chief hunt orders (`HuntOrder` on `FactionData`):** `chief_hunt_order_system` posts daily (staggered by `fid`). Scans `SpatialIndex` within `HUNT_SCAN_RADIUS=40`; posts `Hunt { species, area_tile, target_party_size = 4(Wolf)/2(Deer), … }` or `Scout`.
- **HuntFood pipeline:** three HTN abstract tasks (hunter-only, `HUNTING_SPEAR`-gated, `MF_UNINTERRUPTIBLE`): `JoinHuntParty` → `EngagePrey` → `DeliverHuntKill`. Butcher drops `species_yield()` Meat+Skin.
- **Weapon precondition:** all hunt methods gate on `PlannerCtx.agent_has_weapon` (`Equipment[MainHand]`, `Carrier` hands, or `EconomicAgent.inventory`). `PickUpFreshCorpseMethod` skips the gate (no combat).
- **`EquipHuntingSpear`** is **goal-agnostic** — runs for any unarmed Hunter so a hunter mid-`Lead`/`Defend`/`Socialize` fetches their spear when `HuntOrder::Hunt` is live. Chain preservation keys on `ai.reserved_resource == Some(weapon())` for `WithdrawMaterial` and `Task::Equip { resource_id == weapon() }` for `Equip`.
- **Live chase (`combat::hunt_chase_system`):** Sequential, after `sync_indexed_after_move_system`, before combat. Reads `aq.current = Task::Hunt { prey }`; on despawn → `record_target_failure + aq.cancel()`; on prey-moved within `HUNT_LEASH_RADIUS (30)` → update `dest_tile`/`target_tile` and replan; beyond leash → cancel with `FailedTarget`.
- **`Carrying(Entity)`** marker for "carrying corpse E"; inserted at pickup, removed at butcher/decay/rescue/muster/hunter-demote. `respond_to_distress_system` recruits any same-faction Hunter within `HUNTER_RESPOND_RANGE=50` regardless of LOS.

## Plant lifecycle (`plants.rs`)

Calendar-driven, season-edge-triggered. `Plant.growth: u16` is a stage-time accumulator; on each season change `plant_lifecycle_system` adds `season_growth(kind, prev_season)` and runs at most one stage transition.

- **Per-season growth (`season_growth`):** Winter dormant (lethal for Grain). Grain Spring/Summer/Autumn = 4/5/2; BerryBush 5/4/2; Tree 4/5/3.
- **Stage thresholds:** Grain Seed/Seedling/Mature = 3/5/2 (annual). BerryBush 5/30/4 with Harvested mirroring Seedling (≈3 calendar years to first fruit). Tree 12/48/18 (≈4 years to maturity).
- **Sprout chance:** Seed → Seedling rolls `f32() < 0.20`; failure despawns.
- **Mature → fruiting:** single dice roll at transition (Grain 20% / BerryBush 10% / Tree 5%) for one Seed in chebyshev radius (Grain/Berry r=2, Tree r=3) onto `Grass | Farmland`. Post-roll Grain enters Overripe, BerryBush reverts to Harvested, Tree reverts to Mature.
- **Winter mortality:** every Grain plant despawns at Winter onset (mature ones scatter dice first).
- `plant_lifecycle_system` runs in `FixedUpdate` `SimulationSet::Sequential` after `advance_calendar_system`. Edge-triggered via `Local<Option<Season>>`.

## Animal spawn distribution (`animals.rs::spawn_animals`)

Initial-condition only. Per-species `SocialPattern`: `HERD` (Deer/Horse/Cow, 8–15, r=5), `PACK` (Wolf/Pig/Rabbit/Fox, 3–6, r=3), `SOLITARY` (Cat). `cluster_spawn_tiles` pops shuffled centers from species biome pool and lays members within `cluster_radius`. Runtime cohesion not modeled — `animal_movement_system` wanders independently.

## Nomadic mode

`Lifestyle::{Settled (default), Nomadic}` on `FactionData`. Nomads skip Settlement spawning, run a camp pipeline, migrate seasonally. AI commits atomically (Surveying → PendingCommit → PackingCamp → Traveling → PitchingCamp); player nomads transition via `PlayerCommand::PackCamp` / `PitchCamp`.

- **`Deployable { packed_form, refund_pct, refund_resource, refund_qty }`** (`pack_deploy.rs`) on every nomadic structure. Bedroll (1 skin + 2 wood) and Yurt (8 wood + 6 skin, gated on `PORTABLE_DWELLINGS`) are `fully_packable`; Tent (6 wood + 3 skin) is `refund_only(0.5, wood, 6)`.
- **Build sites:** `BuildSiteKind::{Bedroll, Tent, Yurt}`. Bedroll finalizes as `(Bed { tier: Crude }, Deployable, StructureLabel("Bedroll"))`. Yurt packs into `packed_yurt` good.
- **Camp seeding (`seed_nomadic_camp`):** hearth → bedrolls (radial 2..=5, one per founder), tents (outer 5..=7, ~1 per 4), yurts (Neolithic+, inner 3..=5, cap 2).

### Lifecycle

`FactionData.camp_state: CampState::{Pitched, Packed { since_tick }}` + `migration_phase: MigrationPhase::{Idle, Surveying, PendingCommit, PackingCamp, Traveling, PitchingCamp}` + `nomad_autopilot: bool`. AI runs the FSM; player nomads (`autopilot == false`) skip it for free-form Pack → Move orders → Pitch Camp Here.

**AI migration pipeline (`nomad.rs`):**

- `nomad_migration_system` (Economy, daily, exclusive): for `Idle` factions past cooldown, scores local food < `members × 3`, picks 2–3 lowest-need members, stamps `AgentGoal::Scout` + `ScoutAssignment { quadrant, target_tile, … }`, transitions to `Surveying`. Quadrant targets at `SURVEY_SCOUT_RADIUS = 100` chebyshev seed faction-tier `SharedKnowledge`.
- `nomad_survey_dispatch_system` (ParallelB) routes Scouts via `Task::Explore { kind: AnyEdible }`.
- `nomad_survey_completion_system` (Economy daily, exclusive): at `SURVEY_WINDOW_TICKS = TICKS_PER_DAY*4`, runs `pick_migration_target`, writes `pending_migration`, transitions to `PendingCommit`.
- `pick_migration_target` score = food + herd + water + biome-season + danger + recency − `d * DIST_WEIGHT (0.4)`. `NOMAD_MIN_TARGET_DIST = 8`, max `200`.
- `nomad_migration_commit_system` (Sequential, exclusive) is a lifecycle driver: `PendingCommit` validates target → `PackingCamp` flips `camp_state = Packed`, dispatches `UnpitchStructure` labor for Deployables + no-cargo Campfires → `Traveling` stamps every member with `MigrationTarget`, inserts `FollowingBand` on owned `Tamed`, keeps `home_tile` at old camp → `PitchingCamp` begins on arrival; only then `home_tile` moves, recent_camps updates, `CampMoved` log fires.
- `MigrationTarget { tile, route_tile, started_tick, last_dispatched_tick, bounce_count }` + `WalkReason::Migration` + `TaskKind::Migrate`. `bounce_count` cap 2 reroutes via reachable peer/centroid.
- `nomad_migration_dispatch_system` (ParallelB, `ParamSet`): no `Without<Drafted>` filter — drafted hunters migrate with the band.
- `nomad_migration_arrival_system` (Sequential, after movement): strips marker on `MIGRATE_ARRIVAL_RADIUS = 4`, `MIGRATE_TIMEOUT_TICKS = TICKS_PER_DAY*3`, or `MIGRATE_STALL_TICKS = TICKS_PER_DAY/2`.
- `goal_dispatch_system` preserve-arms: `(MigrateToCamp, Migrate)`, `(Scout, Explore)`.

**Player Pack / Pitch:** chief-actor faction-scoped commands write to `PendingCampOps`. `apply_pack_camp_command_system` runs observable HTN labor — stamps non-drafted members with `PackingDuty` (preempts task chains, releases reservations, drops `JobClaim`), enumerates `Deployable`s within `seed_nomadic_camp_extent` of `home_tile`, dispatches `Task::UnpitchStructure` to nearest worker. `unpitch_structure_task_system` (`nomad_pack_labor.rs`) accumulates to `UNPITCH_WORK_TICKS = 40`, despawns, drops packed_form / packed_bundles / refund as `GroundItem`s. `camp_state = Packed` flips immediately. `apply_pitch_camp_command_system` keeps the synchronous `seed_nomadic_camp` call.

**Manual scout dispatch (`PlayerCommand::SendScout { direction, range }`):** chief-actor command queues `PendingManualScout`; stamps `ScoutAssignment { kind: ScoutKind::PlayerManual { faction_id }, … }`. `manual_scout_completion_system` runs `pick_migration_candidates(intent, k=4)` on arrival; folds into `FactionData.candidate_sites` (ring cap 16). `PlayerCommand::SetMigrationIntent` writes `faction.migration_intent`; AI consumes `intent.weights()`.

**Intent-weighted scoring** — six intents: `FreeRoute` (uniform), `FollowWater` (water×2), `FollowHerds` (herd×2), `SeekWinterShelter` (biome×2.5, dist×0.8), `SeekSummerPasture` (food×1.5, biome×2), `AvoidDanger` (danger×3). Candidates carry per-tile `reasons: Vec<CandidateReason>`.

**Cargo manifest / pitch repair:** `FactionData.cargo_manifest: CampCargoManifest { required, loaded, abandoned, deployed, pitching_started_tick, repair_unlocked }`. AI final-destination pitch unloads carried `bedroll`/`packed_yurt`/`wood`/`skin` near the target, consumes via `PitchStructureAt`. Once a minimal camp exists, migration completes after 80% arrival or `TICKS_PER_DAY/2` since pitch start; `camp_state = Pitched`; `nomad_chief_directive_system` repairs missing shelter. Temporary route waypoints never unlock repair.

**Mobile-state goal gate (`mobile_state_goal_gate_system`, ParallelA after `goal_update_system`):** `CampState::Packed` demotes settled-life goals + strips `JobClaim`. Player `Forage` uses `allowed_while_packed` (Survive/Sleep/GatherFood/Socialize/Defend/Rescue/Play/FollowingPlayerCommand/MigrateToCamp/Scout/care/Drink). AI caravan phases use stricter `allowed_while_ai_caravan` (Survive/Defend/Rescue/SeekCare/ProvideCare/Drink/FollowingPlayerCommand/MigrateToCamp).

**Player-locked packed migration:** `PackedMigrationAutonomy::{Hold, Forage}`. Player-driven nomads default to `Hold`, reset on every `PackCamp`. `Hold` strict-demotes everything except `FollowingPlayerCommand`, `PlayerManual` Scout, sim-owned `MigrateToCamp`. `Forage` falls through to legacy `allowed_while_packed`.

**Pitch conserves shelter count:** `apply_pitch_camp_command_system` sweeps Deployables within `seed_nomadic_camp_extent` of old home (Tents drop materials, fully-packable silently discard), then `seed_nomadic_camp` re-seeds and debits one `bedroll`/`packed_yurt` per spawned shelter from member then pack-animal inventories. First pitch is forgiven; subsequent cycles conserve exactly.

**Slim chief directives (`nomad_chief_directive_system`):** gates on `caps.home.is_mobile() && pending_migration.is_none()`. Targets via `nomad_shelter_targets(members)`. Spawns one Single-tile blueprint per kind per tick, cap `NOMAD_DIRECTIVE_BP_PER_TICK = 2`. No job postings.

**Band redistribution (`nomad_pool.rs`):** `nomad_band_pool_balance_system` shrinks max-min spread for `bedroll`/`packed_yurt`/`preserved_meat` to ≤1 unit across the band within `POOL_BAND_RADIUS = 12` chebyshev.

**Pack-animal logistics:** `PackAnimalInventory { items: [(ResourceId, u32); 6], capacity_g }` on `Tamed`. Capacities: Horse 60kg / Cow 80kg / Pig 30kg / Dog 15kg. `attach_pack_inventory_system` auto-inserts on `Added<Tamed>`. `compute_faction_storage_system` folds pack inventories into nomad faction `storage.totals`. `combat::death_system` drops contents as `GroundItem`s.

**Preserved meat ration:** `2 meat + 1 wood → 3 preserved_meat` (CraftRecipe 12, gated on `FOOD_SMOKING`). `eat_task_system` two-pass: fresh first, preserved only if nothing else on hand.

**Wild herds (`wild_herd.rs`):** `WildHerdRegistry` per-herd `(id, species, aggregate_count, leader_tile, range_center, bloomed, members, flee_until_tick, last_birthed_tick)`. `seed_wild_herds_system` (Startup) places `WILD_HERD_COUNT = 3` herds at grasslands, `WILD_HERD_AGGREGATE = 120` each. `wild_herd_migration_system` (daily): predator flee × 3 / water seek / camp avoidance × 4 / non-Winter birth +12 capped at 200. Bloom/collapse at camera distance 32/48 spawns up to 60 individuals; predation shrinks the herd across cycles.

**Sedentarization (`nomad_sedentarize_system`):** emits `SwitchArchetype` when `member_count ≥ 12`, no `pending_migration`, stable for ≥ `TICKS_PER_SEASON * 4`. **Reverse collapse (`sedentary_collapse.rs`):** settled faction sampling `<6` members + food deficit + shelter loss bumps `collapse_streak`; at `COLLAPSE_TRIGGER_TICKS = TICKS_PER_SEASON` emits `SwitchArchetype` to nomadic variant.

## Pluralist Economy

Per-resource policy flags + per-settlement markets + sub-faction households + Bureaucrat/Trader/Healer professions + P2P currency + escrow + U_bid scoring + tribute + craft contracts. Currency invariant: `EconomicAgent.currency + FactionData.treasury + Settlement.treasury + JobEscrow.amount` is conserved across every operation.

### Settlements (`settlement.rs`)

The **economic** unit (market + treasury + market_tile), distinct from `SettlementPlan` (layout). One faction can own many settlements.

- `Settlement` (Component): `id`, `owner_faction`, `market_tile`, `founding_tick`, `name`, `treasury: f32`, `market: SettlementMarket`, `peak_population`.
- `SettlementId(u32)`; `SettlementMap` (Resource) with `by_id` / `by_megachunk` / `by_faction` indices.
- `auto_found_default_settlements_system` (FixedUpdate Economy + OnEnter, idempotent) spawns one Settlement per non-SOLO faction at its `home_tile`.

### Currency + escrow

- **`pay(world, from, to, amount) -> bool`** (`transactions.rs`) — atomic agent-to-agent transfer. Only sanctioned way to move currency.
- **`FactionData.treasury: f32`** — faction-level wealth pool.
- **`JobEscrow { amount, beneficiary }`** sidecar per funded posting. Producer debits wallet + spawns sidecar. `job_payout_system` (Economy, exclusive, after `job_claim_release_system`) splits `escrow.amount` across claimants on completion (direct credit; funds already held). On failure the `on_remove` hook refunds `beneficiary`. Workers get `Earnings(VecDeque<EarningEntry>)` ring (cap 16) + `ActivityEntryKind::WagePaid`.
- **Chief-funded postings:** `chief_post_funding_system` (Economy, exclusive, after `chief_job_posting_system`) computes `chief_wage_for(progress)` (Stockpile/Haul/Craft → `trade_base_value × qty × CHIEF_MARGIN (0.5)`, Haul ×0.5; Calories/Build/Planting → flat per-day, cap `CHIEF_BUILD_WAGE_CAP=30`), debits `faction.treasury`, spawns `JobEscrow`. Subsistence factions skip funding.
- **Wage signal:** `FactionData.wage_signal: AHashMap<(JobKind, Option<ResourceId>), WageEMA>`. `faction_wage_signal_system` (Economy, daily, after `job_payout_system`) folds last-day Earnings into EMA at `α ≈ 0.129` (5-day half-life). Cross-faction perception via `PerceivedFactionWages` (cap 32). `wage_gossip_system` piggybacks `Socialize` within `WAGE_GOSSIP_RADIUS = 3` with `exp(-age / TICKS_PER_DAY)` staleness.
- **Hook registration:** `JobsPlugin::build` registers `on_remove(on_job_escrow_remove)`.

### Per-resource policy (`economy/policy.rs`)

`ResourceControlPolicy` flags: `chief_allocates_labor`, `private_actors_allowed`, `state_sells_at_market`, `prices_fixed_by_state`, `fixed_price`. `Default` = all-communist; `capitalist()` preset. `FactionData.economic_policy` is empty by default. `Method::policy_gate` returns required flags; `method_passes_policy_gate(...)` is the check. SOLO agents reject any non-empty gate.

### Sub-factions / households

A household is a `FactionData` with `parent_faction = Some(village_id)`. Reuses the entire faction primitive.

- `FactionData.{parent_faction, household_head, children_factions}`.
- `FactionRegistry::spawn_household(parent, home_tile, head, &catalog)` — wires links, sets `household_head`. Non-empty parent `economic_policy` → household stamped `capitalist()`; empty → empty.
- `FactionRegistry::root_faction(id)` walks `parent_faction` to the village.
- **Formation:** `CoSleepTracker.bond_strength` accumulates ticks of cosleep with the same partner; at `HOUSEHOLD_BOND_THRESHOLD` (one game-week) `household_formation_system` spawns the household.
- **Market preset spawn seeding:** `spawn_population` calls `seed_market_households` after the per-faction loop. Every adult founds a one-person household with its own plot tile, dedicated `FactionStorageTile`, and `HOUSEHOLD_SEED_TREASURY (15.0)`.
- **Inheritance:** `pregnancy_system` threads the mother's `Option<&HouseholdMember>` to the newborn.
- **`HouseholdMember` is a marker only** — parents keep `FactionMember.faction_id` pointing at the village.

### Wage-aware labor market

End-to-end: chief-funded postings → escrow → worker payout → faction wage signal → EV-driven profession choice.

- **Capital recognition (`capital.rs`):** `capital_factor(...)` averages three axes in `[1.0, 2.0]`: `tool_capital_factor` (+0.5 when a held item maps via `tool_profession`: weapon→Hunter, tools→Crafter); `workshop_capital_factor` (+1.0 household-owned affine workshop within `WORKSHOP_AFFINITY_RADIUS = 12`, else +0.5 village-owned); `land_capital_factor` (Farmer: +0.5 for household-held Agricultural plot under non-StateOwned tenure). `OwnedBy { faction_id, kind, tile }` stamped at workshop finalize; add/remove hooks maintain `WorkshopOwnership`. `WorkshopKind::affine_to`: Market→Bureaucrat, Workbench/Loom→Crafter, Shrine→Healer.
- **Skill decay (`skills.rs`):** `Skills` clamped at `SKILL_MAX=255`; `SkillPeaks` + `SkillUseTicks` ratchet via `skill_peaks_tracker_system` (observer on `Changed<Skills>`). `skill_decay_system` (daily, half-life 90 days): mastered skills (peak ≥ `SKILL_MASTERY_LINE=80`) decay toward `SKILL_MASTERED_FLOOR=30`; below mastery toward `max(SKILL_FLOOR_BASE=5, peak × 0.30)`.
- **Profession choice (`profession_choice.rs`):** `expected_wage = aggregate_wage_per_day × skill_competence(primary_skill) × capital_factor`; `skill_competence(s) = 0.2 + (s/SKILL_MAX) × 0.8`. Target-driven assignment systems sort candidates by `(expected_wage, primary_skill)`. Shared `demote_profession_state` releases reservations, cancels tasks, strips `Carrying`.
- **Survival override:** below `FARMER_SURVIVAL_FLOOR = 16.0` per-head food, every non-Farmer assignment system zeros target, demoting Hunters/Bureaucrats/Crafters/Healers. Bands: <16 stand-down; 16–32 Farmer ramp; >32 × `FARMER_DEMOTE_RATIO (1.6)` shed excess. Apprentices stay bound (override zeros promotion target, not active links).
- **Asymmetric demotion hysteresis:** `HUNTER_DEMOTE_BUFFER = 1`, `BUREAUCRAT_DEMOTE_BUFFER = 1` add one-slot tolerance. Promotion eager; demotion only on `current > target + buffer`. `want == 0` (survival/treasury) bypasses the buffer. Crafter uses EMA-band hysteresis (`crafter_target_with_hysteresis`).
- **Cross-profession switcher (`cross_profession_switch_system`):** daily Economy pass; switches Hunter↔Bureaucrat↔Crafter when `EV(target) > EV(current) × EV_SWITCH_HYSTERESIS (1.20)`. `switching_cost_skill_regret = peak[primary]/SKILL_MAX × 0.20 × aggregate_wage(current)`. Faction caps via `faction_cap_for`. Sub-`APPRENTICE_THRESHOLD` Crafter targets route through `Apprentice`.
- **Apprenticeship (`apprenticeship.rs`):** novice-Crafter onboarding. `None → Crafter` with `Skills[Crafting] < APPRENTICE_THRESHOLD (30)` scans mentor pool (Crafter + `Skills[Crafting] >= MASTER_THRESHOLD (100)` + no `MentorOf`). Pairs `ApprenticeOf { mentor }` / `MentorOf { apprentice }` + `ApprenticeProgress { ticks, target_ticks: TICKS_PER_DAY × APPRENTICESHIP_DURATION_DAYS (30), target_profession }`. No mentor → fall back to direct Crafter. `apprentice_progress_system` (Economy, daily) ticks + graduates (lifts `Skills[Crafting]` to threshold, strips links, sets `*prof = target`). Stale mentor demotes orphan to `None`. Payouts split: apprentice `share × WAGE_FRACTION_APPRENTICE (0.4)`; mentor `share × WAGE_FRACTION_MENTOR_FEE (0.1)`; residual `0.5·share` refunds. Deliberate-practice 2× XP via `apprenticeship::xp_with_apprentice_bonus`.

### Bureaucrat profession

- `Profession::Bureaucrat` + `FactionData.state_funds_public_works` flag.
- `chief_bureaucrat_appointment_system` (Economy, every `BUREAUCRAT_ASSIGNMENT_CADENCE = TICKS_PER_DAY/4`): target = `max(1, members × BUREAUCRAT_MIN_RATIO)`. At `bureaucrat_treasury_empty_streak >= BUREAUCRAT_QUIT_DAYS * TICKS_PER_DAY`, target=0.
- `bureaucrat_salary_tick_system` (Economy, hourly): debits first settlement treasury, credits each bureaucrat by `BUREAUCRAT_DAILY_WAGE/24`. Bottoms at 0.
- `bureaucrat_admin_dispatch_system` (ParallelB after combat dispatcher) dispatches `Task::Lead { dest = settlement.market_tile }` for Idle Bureaucrats. Direct dispatch (no HTN method).

### Chief postings gated on policy

`JobPosting` carries `poster_class: PosterClass { Chief / Bureaucrat / HouseholdHead / Individual }`, `reward: f32`, `settlement_id: Option<SettlementId>`.

| Branch | Gate |
|---|---|
| Stockpile Calories (food) | `policy_for(Fruit).chief_allocates_labor` |
| Stockpile Wood/Stone | `policy_for(target_rid).chief_allocates_labor` |
| Stockpile (CraftOrder demand) | per `target_rid` |
| Haul (per-blueprint deposit) | `policy_for(slot.resource_id).chief_allocates_labor` |
| Build | `!faction.state_funds_public_works` |
| Craft | `policy_for(recipe.output_resource).chief_allocates_labor` |
| Farm | `policy_for(Grain).chief_allocates_labor` |

Default factions have empty policy → all chief postings fire. Capitalist factions opt out selectively.

- **Workforce-budget policy gate** (`compute_workforce_budget`): policy-disabled slots collapse to 0 share; proportional cut of `usable = 1 − FREE_FLOOR` rerouted to `free`.
- **Household income skim** (`split_market_earnings_with_household`) redirects `caps.income.household_skim_pct` (0% Subsistence, 10% Mixed/Market) of trade earnings to household treasury.
- **Household-poster path:** `household_contract_posting_system` (Economy, every `HOUSEHOLD_POSTING_CADENCE = TICKS_PER_DAY`) walks households with `treasury >= 10.0`, posts one paid craft contract via `post_craft_contract_from_treasury`. Recipe picked by `pick_household_recipe` (Belonging-tier head + LOOM_WEAVING-aware → Cloth; else Tools). `poster_class=HouseholdHead`, `reward = 5.0`, lands on the **village's** board.
- **Self-posted Stockpile contracts:** `worker_self_post_stockpile_system` (Economy, exclusive, every `WORKER_SELF_POST_CADENCE = TICKS_PER_DAY`) — for non-household non-nomadic factions whose staple policy disables chief allocation, wealthiest claim-free non-Drafted member self-posts `JobKind::Stockpile`. Min currency `20.0`; target qty 10. Subsistence bypassed; nomadic / `posting.is_disabled()` short-circuits.

### Per-settlement markets

Production trade routes through agent's faction's first settlement market when one exists; SOLO/unsettled falls back to global `Market`.

- `SettlementMarket` helpers: `calculate_price`, `sell_item`, `try_buy_item`, `clear_flow`, `set_stock` (trader fast path).
- `market_sell_system` / `market_buy_system` use `Res<SettlementMap>` + `Query<&mut Settlement>`.
- `settlement_price_update_system` (Economy, alongside `price_update_system`, every `PRICE_UPDATE_INTERVAL=5`) ticks prices + clears bid counters. Bid-driven discovery (no synthetic demand) — see `economy/CLAUDE.md`.

### Maslow needs + esteem/self-actualization

Two additive needs on `Needs`: `esteem` (Tier 4) and `self_actualization` (Tier 5), both inverted polarity. Strictly additive — does not replace goal selection.

- `esteem_driven_posting_system` (Economy, daily): agents with `next_unmet == Esteem` AND `currency >= 50.0` post a Torch contract (`reward = 8.0`), bump `esteem += 30.0`.
- `self_actualization_teaching_system` (`teaching.rs`, daily): agents with `next_unmet == SelfActualization` + ≥1 Learned tech write `LectureRequest` on highest-complexity Learned. Bump `self_actualization += 30.0`.

### U_bid scoring at job-claim layer

`job_claim_system` branches on `posting.reward`:

- **Paid (`reward > 0.0`):** `U_bid = E(R) + priority_bonus + affinity_bonus − C_action − C_opportunity` where `E(R) = posting.reward × wealth_modifier(currency) × disposition.earn_income_multiplier()`; `disposition multiplier = 1.0 + entrepreneurial/255` (fallback 1.5); `wealth_modifier(c) = 1.0 + 0.5 / (c + 50)`; `priority_bonus = posting.priority * 0.01` (chief 200, household 180, individual 100); `C_action = euclidean × BID_DIST_DISCOUNT`; `C_opportunity = 0.0` (stub).
- **Unpaid (`reward == 0.0`, chief / legacy):** legacy `priority + skill + bias − distance`.
- **Affinity:** `CRAFTER_AFFINITY_BONUS = 3.0` lifts Crafter paid-Craft; unpaid `profession_bias` arms `(Crafter, Craft) = 0.5`, `(Crafter, Stockpile) = 0.1`.

### Trader profession

- `Profession::Trader`. `trader_buy_at_settlement` / `trader_sell_at_settlement` are atomic primitives. `set_stock(id, qty)` is the fast-path setter so helpers don't double-mutate currency.
- `TraderPlan { phase: TravelingToBuy | TravelingToSell, buy_settlement, sell_settlement, resource_id, qty }`.
- `trader_market_step_system` (Economy, exclusive, every 20 ticks): on arrival at phase's market tile, calls buy/sell helpers and advances. Seeds new plans by scanning `AgentMemory.visited_settlements` pairs for the best Cloth gap exceeding `TRADER_MIN_GAP=0.25`. Currency floor `30.0`.
- `trader_route_dispatch_system` (ParallelB) dispatches `Task::Lead { dest }` for off-target traders.
- V1 scope: Cloth-only arbitrage + `TRADER_TRADE_QTY=5`.

### Tribute

- `FactionData.dominance_over: Vec<u32>` + `subordinate_to: Option<u32>`.
- `FactionRegistry::set_dominance(dominant, subordinate)` — idempotent.
- `tribute_payment_system` (Economy, daily): transfers `min(TRIBUTE_PER_DAY, subordinate.treasury)` to overlord. Destitute pays 0 — no debt.

### P2P craft contracts

- `post_craft_contract(world, poster, faction_id, recipe, qty, reward, deadline)` (`jobs.rs`) — atomic; debits poster, pushes `JobPosting` with `poster_class=Individual`, spawns `JobEscrow`. Refuses on insufficient funds / invalid recipe / qty=0 / non-positive reward.
- Lifecycle: completion → zero `escrow.amount` then despawn; cancellation → despawn with amount, `on_remove` refunds.
- **U_bid integration:** `reward > 0` fires the U_bid branch — wealthy poster's contract outscores equidistant chief Craft.

## Land ownership (`land.rs`)

Plot-based ownership layer over the compatibility `SettlementPlan` projection generated from `SettlementBrain` parcels. Plots start `Tenure::StateOwned`; in Mixed/Market presets the listing system publishes them, households acquire what they can afford, rent collects monthly with eviction after two misses, sharecrop plots split harvest yield at gather time.

- **`Plot` (Component):** `id, settlement_id, faction_id, rect, z, zone_kind, tenure, holder, base_value, last_valued_tick, missed_payments, frontage_edge, access_tile, parent_plot`. `Tenure ∈ {StateOwned, Leased, Sharecropping, Freehold}`. `TenureHolder ∈ {State, Household}`.
- **`PlotIndex`:** `by_id`, `by_settlement`, `by_tile` (surface-only), `by_faction_hash`, `next_id`. `plot_at(x, y)` is the hot lookup.
- **`carve_plots_system`** (FixedUpdate Economy, after `settlement_planner_system`, before `chief_directive_system`): re-carves on `culture_hash` mismatch, values via `compute_plot_value`. Plot sizes: Residential 6×6, Crafting/Storage 4×4, Agricultural **16×16** (matches organic parcel size 1:1 — see Farming below); Civic/Sacred/Market/Defense kept whole.
- **`compute_plot_value`** (`PLOT_BASE_VALUE = 50.0`): `BASE × zone_mul × (centre_factor + home_factor) × terrain_factor`. Distance anchors on `market_tile` + `home_tile`; terrain samples fertility at centre + 4 corners.
- **`LandPolicy`** (`economy/policy.rs`): set by `land_policy_for(EconomyPreset)`. Subsistence all-false; Mixed = rent + sharecrop; Market = adds sale + freehold. `default_lease_period_days = 30`, `rent_yield_pct = 0.04`, `default_share_to_landlord = 0.30`.
- **`tile_buildable_by`**: rejects placements on plots held by another faction/household. Wired into `chief_directive_system` after candidate selection.
- **`land_listing_system`** (Economy, every `TICKS_PER_DAY/4`): publishes Sale + Lease listings for `StateOwned` plots when `LandPolicy` permits. Civic-class zones skipped. Per-(faction, zone-kind) caps (`RESIDENTIAL_LISTINGS_CAP=6`, `AGRICULTURAL_LISTINGS_CAP=6`, `CRAFTING_LISTINGS_CAP=4`); a plot triple-listing as Sale+Lease+Sharecrop counts as one slot. Sale asking = `base_value`; lease asking = `base_value × rent_yield_pct` per month, floored at `MIN_MONTHLY_RENT`.
- **`household_land_acquisition_system`** (Economy, daily): per-zone-kind acquisition track. Households browse parent listings for Residential and Agricultural separately; can hold one of each. Affordability: lease ≤ 40%, buy ≤ 70% of treasury. Preference: Sale → Lease → Sharecrop. Agricultural acquisitions prefer `Profession::Farmer`-headed households when any exist. Acquiring an Agricultural plot also spawns a household-private `FactionStorageTile` for private farm seed/harvest routing. Also claims nearest unowned same-village Agricultural plot within 12 chebyshev as child (mirrored tenure).
- **`rent_collection_system`** (Economy, every 30 game-days): Leased plots with expired `paid_through_tick` attempt household→landlord transfer. Failure bumps `missed_payments`; at `EVICTION_MISS_THRESHOLD = 2` → tenure `StateOwned`, emits `PlotEvictedEvent`.
- **`evicted_plot_cleanup_system`** drains queue per `caps.land.eviction_policy`. `Demolish` walks `StructureIndex` over `plot_rect`, refunds via `Deployable::compute_refund_drop`, `despawn_recursive`s.
- **Sharecropping:** `state_sharecrops` flag adds `ListingKind::Sharecrop` for Agricultural plots. Zero upfront; tenure `Sharecropping { share_to_landlord, paid_through_tick }`. `gather.rs` harvest hook routes landlord share via `split_sharecrop_yield(qty, share_to_landlord)` as a `GroundItem` at landlord's nearest `FactionStorageTile`.

## Farming (`farm.rs`, plot-scoped)

- **Plot size:** Agricultural plots are 16×16 tiles (~576 m² at 1.5 m/tile) carved 1:1 from organic Agricultural parcels — one plot per parcel, no subdivision mismatch. Yields are tuned so a 4-person household subsists on one plot.
- **`FarmPlotAssignments`** (Resource): one-to-one match `farmer Entity ↔ PlotId`. Maintained daily by `chief_farm_plot_assignment_system` (Economy) for villages whose grain policy still has `chief_allocates_labor=true`. Greedy match by chebyshev distance; releases stale entries when farmers despawn / demote, plots leave state ownership, or plots vanish.
- **Plot-scoped chief Farm postings:** `chief_job_posting_system` posts one `JobKind::Farm` per assignment with `JobProgress::Planting { plot_id, assigned_farmer, area: plot.rect, … }`. Bootstrap fallback (`plot_id = None`, `home_tile ±5`) only fires when a faction has farmers but no carved Agricultural plots yet.
- **Claim restriction:** `job_claim_system` rejects a posting whose `assigned_farmer` doesn't match the candidate worker — only the assigned farmer can claim their plot job.
- **Plant ownership:** `production.rs` cascade is `JobKind::Farm + FactionMember → Faction-owned`, then `HouseholdMember → Household`, else `Person`. Fixes the bug where household members on chief-Farm postings stamped Household ownership.
- **`FarmScope` resolver** (`htn.rs::resolve_farm_scope`, shared by both farm dispatchers): three branches mirror the `production.rs` plant-ownership cascade — **Communal** (`JobClaim::Farm` posting with `plot_id` + matching `assigned_farmer` → village storage, plot rect from `PlotIndex`), **Private** (`Profession::Farmer` + `HouseholdMember` whose household holds a `ZoneKind::Agricultural` plot → household storage as `source_faction_id`, deposit override `Some(household_id)`), **Bootstrap** (everything else → village storage, no plot rect).
- **Plot-restricted planting search:** `htn_plant_from_storage_dispatch_system` keys seed-stock lookup and `storage_tile_map.by_faction[..]` on `FarmScope::source_faction_id` (village for Communal/Bootstrap, household for Private), and uses `scope.plot_rect()` to drive `find_nearest_unplanted_in_rect(..)`. Bootstrap falls back to `find_nearest_unplanted_farmland(.., radius=15)`.
- **Plot-restricted harvest search:** `htn_harvest_plant_dispatch_system` reuses the same resolver. Communal/Private walk the plot's rect via `PlantMap`, picking the nearest live `GrowthStage::Mature` plant (Chebyshev) — no more wandering off to forage random berries. Private threads `deposit_target_faction_override = Some(household_id)` so `HarvestMaturePlantForStorageMethod::expand` emits `Task::DepositToFactionStorage { target_faction_id: Some(household_id) }`; `gather_deposit_tile` routes through household storage. Bootstrap keeps the legacy `GatherKnowledge::nearest_target_tile(MemoryKind::AnyEdible)` search.
- **`FarmWorkScorer`** (Subsistence-tier, score 0.90): self-directed planting goal for `Profession::Farmer` workers in factions where grain policy has `private_actors_allowed=true` (Mixed / Market). Registered before `StockpileScorer` so private farmers default to working their own plot rather than wandering off to chief postings; survival/safety scorers still preempt via class precedence.
- **`Task::DepositToFactionStorage` carries `target_faction_id: Option<u32>`:** `Some(household_id)` overrides actor-faction routing so private harvest deposits to household storage. Routing honored in `gather.rs::finish_gather` and `items.rs::finish_scavenge`. Default `None` preserves village-storage behaviour.
- **Mixed preset enables capitalist grain:** `apply_preset` no longer skips grain/wood/stone/edibles for Mixed; every catalog resource gets `mixed()` (chief still allocates AND private actors allowed). Chief Farm jobs and `FarmWorkScorer` co-exist in Mixed villages.
- **Game-start seeding (`seed_starting_farms_system`):** OnEnter(Playing), after `seed_starting_buildings_system`. For every settled non-SOLO village with no Agricultural plots, spawns a 16×16 plot at the best nearby Loam/Silt/Grass tile, leaves it `StateOwned`, and pre-seeds `STARTING_GRAIN_SEEDS=32` grain seeds into faction storage so Farm work begins immediately. Sandbox + nomadic factions are skipped.

## Game lifecycle and regions

- **`GameState`:** `SpawnSelect` (default) | `Playing`. Spawn UI in `SpawnSelect`; spawn systems `OnEnter(Playing)`. Update-stage systems gated `in_state(Playing)`; FixedUpdate sim not gated.
- **Sandbox bypass:** `SandboxPlugin` flips state to `Playing` at Startup.
- **`SettledRegions` (`region.rs`):** `AHashMap<RegionId, SettledRegion>` + `by_megachunk`. `SettledRegion { megachunk, founding_tick, name, camera_bookmark, player_owned }`. `settle()` idempotent.
- **`MegaChunkCoord`:** `MEGACHUNK_SIZE_CHUNKS=16`. Independent of climate cells. Helpers: `from_tile`/`center_tile`/`chunk_range`/`tile_bounds`/`contains_tile`.
- **Player home is mega-chunk-constrained.** `spawn_population` (`person.rs`) special-cases `group_idx == 0`: it calls `region::pick_player_home_in_megachunk(chunk_map, mx, my, world_seed)` so the home tile is *always* inside the cell the player picked on the spawn-select map. The picker is deterministic (`fastrand::Rng` seeded from `(world_seed, mx, my)`) with a centre-out spiral fallback and a forced-centre last resort. AI factions (`group_idx > 0`) keep the legacy 32×32-chunk best-of-200 search so neighbours fan out around the player.
- **Multi-focus chunk streaming (`SimulationFocus`):** `Vec<FocusPoint>` rebuilt each tick from camera + every settled region centre. Chunk DATA loads for any focus disc (camera `LOAD_RADIUS=12`, region `REGION_LOAD_RADIUS=6`); SPRITES + plants + loose rocks only inside camera focus.
- **Focus-aware LOD:** camera distance produces base LOD; entities within 8 chunks of any non-camera focus promote from Dormant to Aggregate. `cohort.rs` (`CohortRegistry`) summarises Aggregate agents by `{faction, settlement_or_camp, profession, age_band, wealth_band, lifestyle}`; `cohort_pin_full_sim_system` (ParallelA after camera LOD) promotes commanded/chief/drafted/combat agents back to `Full` via `PinnedFullSim`.
- **Edge-walk expansion (`region::detect_edge_crossing_system`):** each tick, unsettled mega-chunks the player faction enters get `settle()`, naming "Outpost N", bookmarking, emitting `ActivityLogEvent::RegionSettled`.
