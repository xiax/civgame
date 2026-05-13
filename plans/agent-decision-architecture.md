# Scalable Agent Decision Architecture

## Summary

Refactor agent AI into a layered model that can support Stone Age through modern-age complexity without requiring every agent to reason over the full world each tick.

The target architecture is:

- **Utility scorers choose intent**: cheap, tiered scoring decides what an agent wants next.
- **HTN methods execute intent**: HTN remains responsible for decomposing a chosen goal into concrete typed tasks.
- **Institutions generate opportunities**: households, job boards, markets, schools, clinics, governments, and military systems create structured options for agents to choose from.
- **LOD/cohort simulation preserves scale**: full individual reasoning only runs for important/nearby agents; distant populations run through aggregate models.

This should be implemented incrementally, preserving existing behavior while moving one goal family at a time out of the hardcoded cascade in `goal_update_system`.

## Key Changes

### 1. Make `GoalScorerRegistry` the Primary Goal Selector

Introduce a unified scoring pass that replaces most of the imperative goal cascade in `src/simulation/goals.rs`.

Keep `AgentGoal` as the compatibility/public goal enum for UI, HTN dispatchers, job locks, and existing systems.

Add or expand:

- `GoalScore`
  - `goal: AgentGoal`
  - `class: GoalClass`
  - `score: f32`
  - `reason: &'static str`
  - `commitment: GoalCommitment`
  - `interrupt_policy: InterruptPolicy`

- `GoalCommitment`
  - `None`
  - `UntilTaskComplete`
  - `UntilTick(u64)`
  - `UntilNeedBelow { need, threshold }`

- `InterruptPolicy`
  - `AlwaysInterruptible`
  - `InterruptibleByHigherClass`
  - `UninterruptibleExceptSurvival`

- `AgentDecisionState`
  - last chosen class/score/reason
  - last evaluation tick
  - committed goal expiration
  - last scorer id/name for debug UI

Scoring rules:

- `Survival` and hard safety goals always beat lower classes.
- Within the same `GoalClass`, choose max score.
- Preserve hysteresis: an active goal keeps running unless a challenger exceeds it by a configurable margin or belongs to a higher class.
- Preserve existing forced states: player commands, drafted behavior, migration targets, rescue targets, active job claims, and packed-camp gating still bypass or constrain autonomous choice.

### 2. Convert Existing Goal Logic Into Scorers

Move the current `goal_update_system` branches into scorer structs, one family at a time.

Initial scorer set:

- `StarvationScorer`: `Survive` when hunger is severe or faction food exists.
- `EatHeldFoodScorer`: `Survive` when carrying food and hungry.
- `SleepScorer`: `Sleep` from sleep need, fatigue, time of day.
- `FactionDefenseScorer`: `Defend` during raids.
- `FactionRaidScorer`: `Raid` when faction has target and agent is fit.
- `ChiefLeadScorer`: `Lead` for chiefs outside crisis.
- `ReturnSurplusScorer`: `ReturnCamp` when carrying extra food and storage is reachable.
- `SocialScorer`: `Socialize` from social need and disposition.
- `PlayScorer`: `Play` from willpower and personality/disposition.
- `PersonalBuildScorer`: `Build` for personal blueprints.
- `CraftNeedScorer`: `Craft` when faction craft demand and inputs exist.
- `StockpileNeedScorer`: `GatherFood`, `GatherWood`, `GatherStone`, or `Stockpile`.
- Existing `EarnIncomeScorer`: keep and let it compete in `Enterprise`.

After migration, `goal_update_system` becomes mostly:

1. handle forced/authoritative states;
2. build `GoalScoringContext`;
3. ask registry for best valid score;
4. apply commitment/hysteresis;
5. write `AgentGoal`, `GoalReason`, and cleanup only when the goal actually changes.

### 3. Keep HTN, But Unify Dispatch Shape

Do not replace HTN. Refactor it so HTN is the “how” layer beneath a selected goal.

Current issue: many separate `htn_*_dispatch_system`s are manually ordered. Keep them initially, but introduce a shared dispatch helper:

- map `AgentGoal -> Vec<AbstractTask>`
- build `PlannerCtx`
- filter methods by precondition, policy gate, history, LOD, and reachability
- choose method via `score_method_with_history`
- expand into `ActionQueue`

Target behavior:

- `AgentGoal::Survive` can try `Eat`, then `AcquireFood`.
- `AgentGoal::Craft` can try material delivery, then work, then fallback acquisition.
- `AgentGoal::Farm` can try harvest, then plant.
- `AgentGoal::Play` can try partner, toy, stone throwing, recreational planting.
- `AgentGoal::Build` can try clear obstacle, haul material, construct.

Do this gradually. Avoid a giant rewrite of `htn.rs` in one pass.

### 4. Add Opportunity Caches

To scale beyond thousands of agents, scorers must not repeatedly scan raw ECS/world state.

Add faction/settlement/region-level opportunity caches refreshed on appropriate cadences:

- `FoodOpportunityCache`: reachable storage food, known forage, ground food, market food.
- `LaborOpportunityCache`: job postings summarized by kind, reward, distance bands, required skill.
- `SocialOpportunityCache`: nearby social partners/venues.
- `LearningOpportunityCache`: teachers, books/tablets, lectures, schools.
- `CareOpportunityCache`: injured agents, healers, beds, clinics.
- `ThreatOpportunityCache`: raids, predators, hostile factions, unsafe regions.
- `MarketOpportunityCache`: local price gaps, shortages, trade routes.

Scorers read these caches first. Raw ECS scans remain inside cache builders or narrow HTN methods.

Cadence defaults:

- survival and threats: frequent, bucketed
- local opportunities: every few seconds
- jobs/markets/institutions: daily or economy cadence
- technology/career/life decisions: daily to seasonal

### 5. Model Institutions as Producers of Choices

Modern-age complexity should live mostly in systems that create structured opportunities, not inside individual agent brains.

Add institutional layers incrementally:

- **Households**: food security, rent, child care, property, inheritance, household work requests.
- **Markets/firms/guilds**: paid work, production orders, wage signals, capital ownership.
- **Schools/teachers**: learning opportunities, apprenticeships, lectures, literacy paths.
- **Clinics/healers**: care jobs, patient triage, medicine demand.
- **Government/civic systems**: taxes, public works, military drafts, laws, welfare/rations.
- **Transport/logistics**: commute options, caravans, roads, public transit equivalents later.

Agents then score “available opportunities” rather than deriving all social/economic behavior from scratch.

### 6. Add Real Aggregate LOD

Current `LodLevel::{Full, Aggregate, Dormant}` mostly gates system execution. Extend `Aggregate` into actual cohort simulation.

Rules:

- `Full`: individual needs, goals, HTN, movement, combat, visible stories.
- `Aggregate`: no per-agent HTN; simulate grouped production/consumption/social change by faction/settlement/profession/household.
- `Dormant`: very low-frequency demographic/economic updates only.

Add cohort records keyed by:

- faction
- settlement/camp/region
- profession
- age band
- wealth band
- household/lifestyle class when needed

Aggregate systems update:

- food consumption and production
- births/deaths/migration
- skill/tech diffusion
- wages and shortages
- disease/injury recovery later
- public order/safety later

Promotion/demotion:

- Agents near the camera, player-selected, involved in combat, named/story-important, or part of active player commands become `Full`.
- Distant ordinary agents collapse into cohorts.
- When promoting a cohort member back to an entity, sample plausible needs, inventory, wealth, profession, skills, and recent memory from the cohort state.

## Implementation Plan

### Phase 1: Instrument Before Refactoring

Add lightweight debug counters:

- number of goal evaluations per tick
- number of scorer evaluations per tick
- chosen goal distribution
- HTN method attempts/success/failure by method id
- average `ActionQueue` length
- LOD population counts
- time spent in goal selection and HTN dispatch

Add a sandbox/debug overlay or log summary for these values.

Acceptance: running `cargo run -- --sandbox` shows stable counters and no behavior change.

### Phase 2: Scorer-First Goal Selection

Implement `AgentDecisionState`, `GoalCommitment`, and `InterruptPolicy`.

Move existing branches from `goal_update_system` into scorers while preserving current thresholds:

- hunger thresholds from `goals.rs`
- sleep thresholds
- social/play thresholds
- craft gates
- gather fallback logic
- raid/defense/chief overrides

Keep forced states in `goal_update_system`.

Acceptance: sandbox behavior is visually equivalent, and tests cover that survival, sleep, raid defense, chief leading, job claims, and player commands still win correctly.

### Phase 3: Normalize HTN Dispatch

Add a shared `dispatch_abstract_task_for_goal` helper in `htn.rs`.

Start with low-risk goals:

- `Sleep`
- `Socialize`
- `Play`
- `Defend`
- `Lead`
- `Raid`

Then migrate complex goals:

- `Survive`
- `GatherFood`
- `GatherWood`
- `GatherStone`
- `Craft`
- `Farm`
- `Build`

Acceptance: no behavior regression, fewer manually duplicated argmax blocks, method history still biases repeated failures.

### Phase 4: Opportunity Caches

Introduce cache resources and replace scorer-local scans.

First caches:

- food opportunities
- labor/job opportunities
- material deficits
- social partners/venues

Then add:

- learning opportunities
- care/healing opportunities
- threat opportunities
- trade/market opportunities

Acceptance: scorer code no longer performs broad ECS scans; cache builders are bucketed or cadence-gated.

### Phase 5: Aggregate LOD

Make `Aggregate` agents stop running full goal/HTN dispatch.

Add settlement/faction cohort simulation for:

- consumption
- production
- work allocation
- births/deaths
- basic learning/tech spread
- migration pressure

Keep important entities pinned to `Full`.

Acceptance: large populations can run with most agents aggregate while visible agents still behave individually.

### Phase 6: Modern-Age Expansion

Build new complexity through institutions and scorers, not ad hoc goal branches.

Add new goal families only as scorers plus HTN methods:

- education/study/teach
- healing/seek care/provide care
- household care
- government/public works/taxation
- policing/law/order
- commerce/trade/arbitrage
- prestige/status/politics
- recreation/culture/religion

Acceptance: adding a new life domain requires one scorer, one or more HTN methods, and optional opportunity cache/institution producer, without editing a giant central cascade.

## Test Plan

Add unit tests for scorer priority:

- starving beats paid work
- sleep beats enterprise but loses to emergency defense
- job claim preserves assigned goal
- player command bypasses autonomous scoring
- packed camp rejects settled-life goals
- chief leads only outside crisis
- income scorer only overrides fallback work, not survival/safety

Add HTN tests:

- selected `AgentGoal` maps to expected `AbstractTask` candidates
- method history lowers repeated failed methods
- uninterruptible methods survive lower-class goal flips
- interruptible methods are abandoned and recorded

Add integration/sandbox tests:

- small settled village survives multiple days
- nomadic camp migrates and resumes work
- market faction assigns professioned workers to paid jobs
- food shortage reallocates labor toward subsistence
- aggregate LOD population consumes/produces without per-agent dispatch

Add performance benchmarks:

- 1k full agents
- 10k mixed full/aggregate agents
- 100k mostly aggregate agents
- target: goal selection cost scales with active `Full` agents, not total population

## Assumptions

- `AgentGoal` remains the public compatibility enum for now.
- HTN remains the task decomposition layer.
- No new crates are required.
- The first implementation should preserve current gameplay behavior before adding modern-age domains.
- Hundred-thousand-agent scale requires aggregate simulation; individual HTN for every agent every few seconds is not the target.
