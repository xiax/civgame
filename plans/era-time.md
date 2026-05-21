# Civic Time Use: Farm Rushes & Festivals

## Context

A civic-calendar layer that changes how agents spend ordinary time by season and
institution: seasonal farm rushes that raise pressure on the **existing**
`JobKind::Farm`/`FieldWork` pipeline (no new farm tasks), and a two-phase festival
(muster → assemble → benefit-on-arrival) that replaces the current "benefit
whoever is already nearby" `ritual_system`.

This revises an earlier draft. Three fixes over that draft:

1. **Don't conflate two mechanics in one enum.** `PlantingRush`/`HarvestRush` are
   *persistent seasonal labor modes* (weeks long, modulate workforce pressure);
   a festival is a *discrete short event*. They must not mutually exclude. → Split
   into a persistent `labor_mode` field and a separate `Option<CivicEvent>`.
2. **No `EmergencySubsistence` day kind.** Severe food shortage is already handled
   by `compute_workforce_budget`'s critical-food override (`stockpile_food` →
   `CRITICAL_FOOD_FLOOR=0.45` when food pressure ≥ 80). → Food crisis is a
   *suppressor* that cancels festivals; the existing override redirects labor.
3. **Festival is a goal, not a side-channel.** civgame routes every activity
   through `AgentGoal` → `GoalScorer` → HTN method → ParallelB dispatcher. A real
   goal gets exclusions (starving/injured/commanded/migrating/drafted) for free
   via `GoalClass` tiers + existing query filters — no parallel dispatcher, no
   bespoke "job-claim skip".

Institutional days (Market/Corvee/Lecture/Drill) are deferred to the Phase 2
skeleton below.

## Reuse (do not reimplement)

| Concern | Existing surface |
|---|---|
| Seasonal farm phase | `farm::farm_season_phase → FarmSeasonPhase` (`farm.rs:52`) |
| Farm postings | `chief_job_posting_system` Farm branch (`jobs.rs:2219+`), `JobProgress::FieldWork` |
| Farm claim scaling | `posting_target_workers` (`jobs.rs:3725`), `(target/8).clamp(3,12)` |
| Workforce shares | `compute_workforce_budget`/`workforce_budget_system` (`projects.rs:522,875`) |
| Private-plot farming | `FarmWorkScorer` (`goal_scorers.rs:1052`), `GoalScoringContext` |
| Season-edge ritual benefit | `ritual_system`+`RitualState`/`RitualEvent` (`construction.rs:5240-5350`) — **replaced** |
| Civic anchors | `MonumentMap`/`ShrineMap`/`CampfireMap` (`construction.rs:50,98,110`), `FactionData.home_tile` |
| Goal pipeline | `AgentGoal` (`goals.rs:158`), `GoalScorer`+registry, HTN method + ParallelB dispatcher |
| Slot-reservation precedent | `GatherClaims` per-tile reservation (`gather_claims.rs`) |
| "Pulled from work" precedent | `Drafted` + `Without<Drafted>` query filters |

## Phase 1 — Implementation

### 1. New module `src/simulation/time_use.rs`

```rust
pub enum SeasonalLaborMode { Normal, PlantingRush, HarvestRush }   // recomputed daily

pub struct CivicEvent {
    pub kind: CivicEventKind,          // Phase 1: RitualFestival only
    pub anchor: (i32, i32), pub anchor_z: i8,
    pub muster_start: u32,             // [muster_start, festival_start)
    pub festival_start: u32, pub festival_end: u32,  // [festival_start, festival_end)
    pub seed: u64,                     // splitmix(faction_id, year, season)
    pub invited: u32, pub arrived: u32, pub benefited: u32,   // debug counters
}
pub enum CivicEventKind { RitualFestival }   // Phase 2: +Market/Corvee/Lecture/Drill
```

Civic state lives on `FactionData` (mirrors `raid_phase`/`workforce_budget`):
`civic_labor_mode: SeasonalLaborMode` (default `Normal`) + `civic_event: Option<CivicEvent>`.

`FestivalAssembly` — `Resource` of ring-slot reservations, modelled on
`GatherClaims`: `AHashMap<u32 /*faction*/, FestivalSlots>`; `FestivalSlots` holds
the anchor + a `Vec<(tile, Option<Entity>)>` reservable stand ring; rebuilt when a
faction's `civic_event` starts/ends.

Helpers: `pick_civic_anchor` (Monument → Shrine → Campfire/Hearth → `home_tile`,
~30-cheb scan of the existing maps, same as `construction.rs:5309`);
`festival_ring_slots` (passable ring via `chunk_map.passable_at`, fallback
`tasks::nearest_reachable_tile_near` `tasks.rs:323`; slot count caps attendance);
`derive_labor_mode`.

### 2. `civic_calendar_system` (Economy, daily, `.before(workforce_budget_system)`)

Per non-SOLO faction:
- **Food-crisis suppressor first** — if food pressure ≥ critical (same signal as
  `compute_workforce_budget`'s override), cancel any `civic_event`, set
  `civic_labor_mode = derive_labor_mode(...)`, return.
- **Labor mode** — `PlantingRush` when `SpringPrepPlant` + plots have Prepare/Plant
  work; `HarvestRush` when `AutumnHarvest` + mature plot Grain exists; else `Normal`.
- **Festival scheduling** — on a season transition (`Local<Option<Season>>` edge,
  as `ritual_system` triggers), if a usable anchor exists and not in food crisis,
  set `civic_event = Some(CivicEvent { RitualFestival, .. })` with deterministic
  `MUSTER_TICKS`/`FESTIVAL_TICKS` windows. Clear once `now >= festival_end`.

### 3. Farm-rush integration (no new farm tasks)

- **Workforce pressure** — thread `civic_labor_mode` into `compute_workforce_budget`;
  multiply the **`farm_pressure` input** (`[0..100]`, not the output `.farm` field)
  by `FARM_RUSH_PRESSURE_MULT` (~1.5) during a rush, so the existing proportional
  allocation + EMA blend absorbs it instead of being overwritten.
- **Posting priority** — in `chief_job_posting_system`'s Farm branch raise
  `JobPosting.priority` for `FieldWork` postings matching the rush phase.
- **Private plots** — add `civic_labor_mode` to `GoalScoringContext`; bump
  `FarmWorkScorer`'s `base` (0.90) by a small rush lift.
- **Claim scaling unchanged** — `posting_target_workers` already scales with
  target; the binding constraint during a rush is the workforce-budget cap, which
  the pressure boost raises. Winter → `Normal`, no rush.

### 4. Festival as a first-class goal (replaces `ritual_system` broadcast)

- `AgentGoal::AttendFestival` (`goals.rs:158`), `GoalClass::Belonging` — below
  `Survival`/`Safety` (starving/thirsty/injured preempt automatically), above
  `Esteem`/`Discretionary`.
- **`FestivalScorer`** (registered in `register_default_scorers`) — fires when the
  faction has an active `civic_event` covering `now`, agent is a member, and a free
  ring slot exists; returns `None` when slots full (that *is* the attendance cap).
  Because it runs in normal `goal_update_system` re-evaluation, a `JobClaim`-holder
  stays on the maintenance-only pass and is **not** pulled off critical work — the
  "skip claim-holders" intent, achieved structurally.
- **HTN** — `AbstractTaskKind::AttendFestival`, `AttendFestivalMethod` →
  `[WalkTo { slot }, AttendFestival { event }]`. `htn_attend_festival_dispatch_system`
  (ParallelB, `Without<Drafted>`) claims the nearest free ring slot (reserve on the
  slot, mirroring `GatherClaims`), routes via `assign_task_with_routing`, dispatches.
  Commanded/migrating agents are skipped by the existing goal-forcing / mobile gate.
- **`Task::AttendFestival`** + `TaskKind` discriminant; executor
  `attend_festival_task_system` (Sequential): on arrival, linger until
  `now >= festival_end` then `aq.finish_task`. During `[festival_start, festival_end)`
  apply per-tick relief to `Needs.social` + `Needs.willpower` (`needs.rs:52`),
  magnitude reusing `ritual_system`'s `pulse_f = 15 + ceremonial/255 × 35`. `esteem`
  is inert in R8 — skip.
- **`goal_dispatch_system`** — add preserve-arm `(AttendFestival, AttendFestival)`;
  release the ring slot on cancel/finish.
- **Retire `ritual_system`'s broadcast** — fully convert (no half-measures); benefit
  now requires being on-task at a slot. Repurpose `RitualState`/`RitualEvent` debug
  ring as the festival debug surface, recording `invited`/`arrived`/`benefited`.
  Paleolithic bands get festivals too (campfire/home anchor).
- **Feast (optional)** — only with safe food surplus, a festival may debit a small
  grain amount for a stronger willpower lift.

### 5. Scheduling

```
Economy (daily):  civic_calendar_system .before(workforce_budget_system)
                  workforce_budget_system / chief_job_posting_system  (read civic_labor_mode)
ParallelA:        goal_update_system  (FestivalScorer)
ParallelB:        htn_attend_festival_dispatch_system  (Without<Drafted>)
Sequential:       attend_festival_task_system
```

## Files touched

- **New:** `src/simulation/time_use.rs`.
- `faction.rs` (`civic_labor_mode`/`civic_event`), `goals.rs` (`AttendFestival`),
  `goal_scorers.rs` (`FestivalScorer` + ctx field), `htn.rs` (abstract task +
  method + dispatcher), `typed_task.rs`/`tasks.rs` (`Task::AttendFestival`,
  executor, preserve-arm), `projects.rs` (rush pressure mult), `jobs.rs` (Farm
  posting priority), `construction.rs` (retire ritual broadcast), `mod.rs`
  (registration/ordering), `simulation/CLAUDE.md`.

## Test plan (`cargo test --bin civgame`, `test_fixture.rs` harness)

- **Civic selection** — `derive_labor_mode` deterministic: Spring+plot work →
  `PlantingRush`, Autumn mature grain → `HarvestRush`, Winter → `Normal`; food
  crisis cancels a scheduled festival.
- **Farm rush** — Spring rush raises `farm` workforce share + realized `FieldWork`
  claim count vs. a `Normal` control; no duplicate Farm postings; Autumn prefers
  Harvest.
- **Festival** — no relief until the `AttendFestival` task reaches the linger state
  at a slot; starving/injured/commanded/migrating/drafted agents never adopt the
  goal; slot exhaustion caps attendance; debug ring records non-zero counts.
- **End-to-end (`cargo run`)** — a spring settlement pushes field work; a
  season-edge festival pulls idle members to the monument/shrine/campfire.

## Phase 2 — Institutional days (deferred skeleton)

Each is a new `CivicEventKind` reusing Phase 1 machinery (`CivicEvent` window +
festival muster/slot pattern or a workforce-pressure boost). Write
`plans/civic-institutional-days.md` when starting:

- **MarketDay** — Chalcolithic+ w/ Market/trade. Lift market goal scoring
  (`EarnIncomeScorer`) + trader activity for the window; no muster. Entry:
  `economy/market.rs`, `goal_scorers.rs`, Trader systems.
- **CorveeDay** — Neolithic+ w/ public projects waiting. Temporary `build`
  workforce-pressure boost (same pattern as the farm rush). Entry:
  `compute_workforce_budget`, `Projects`.
- **LectureDay** — tech gaps + teacher/tablet. Auto-emit `LectureRequest`, reuse
  `HoldLecture`/`teaching.rs` draft. Entry: `teaching.rs`,
  `self_actualization_teaching_system`.
- **DrillDay** — Bronze+ w/ Barracks/army tech. Muster militia to a rally point
  (`military/formation.rs::plan_compact_ring`), brief combat XP. Entry:
  `military/`, `player_command.rs` `Muster`.

Open question: whether multiple institutional events may overlap a festival
(current `Option<CivicEvent>` allows one — may need a small fixed array).

## Constraints

No new crates. `ahash::AHashMap`. `fastrand`/splitmix for the festival seed (never
an entropy-keyed hasher — `home_pick_seed` determinism rule). Logic in systems,
data on components/resources. Update `CLAUDE.md` when behavior changes.
