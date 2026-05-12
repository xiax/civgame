# Autonomous Subsistence + Scoring-Based Goal Selection

## Context

Two interlocking issues showed up while investigating workers stuck Idle under `AgentGoal::GatherFood`:

1. **The dispatcher gate is wrong.** `htn_stockpile_food_dispatch_system` requires a `JobClaim::Stockpile{food}`, but `goal_update_system` legitimately assigns `GatherFood` as the unclaimed-worker default whenever `prioritize_food = faction_food_ratio < 1.0`. Personal/household food stockpiling is *subsistence reflex*, not economic coordination — it doesn't need a posting any more than eating does. The self-posting workaround (`worker_self_post_stockpile_system`) is currency-laundering: wallet → escrow → wallet, models nothing, and falls apart for food because pricing per-calorie-aggregate isn't well-defined.

2. **`goal_update_system` is a procedural if-else cascade that can't grow.** Lines 779–810 of `goals.rs` are ~10 hardcoded branches in fixed priority order. There's no room for: per-agent variation (some workers are more entrepreneurial), exploration vs. exploitation (ε-greedy across reasonable alternatives), or personal-enterprise goals (`EarnIncome`, `SaveForTool`, `LearnTech`). Every new motivation requires editing the ladder and re-ordering branches.

This plan addresses both: ships the immediate bug fix in Phase 1 with the right semantics (subsistence, not market), and refactors goal selection into a scorer registry in Phases 3–5 so personal-enterprise and ε-greedy land cleanly.

## Design principles

- **Subsistence reflexes bypass markets entirely.** Eat, sleep, stockpile-for-self/household, forage-for-self. No posting, no claim, no currency.
- **Coordination uses postings.** Chief allocates communal labor; households post buy-orders for goods they don't produce; individuals post craft contracts. These add *wage* incentive on top of subsistence work; they don't replace it.
- **Goals are scored, not branched.** Each goal-candidate produces a utility; the system argmaxes (with optional ε-greedy exploration). Crisis goals (Survive/Defend/Rescue) preempt above the argmax floor.
- **Per-agent variation lives in `Disposition`.** A small `[u8; N]` component biases certain goal classes (industrious, social, entrepreneurial, risk-tolerant). Stable across an agent's life; inherited at birth with mutation.

## Phased plan

### Phase 1 — Drop the claim gate for autonomous food stockpile

`src/simulation/htn.rs:4981–5004` (`htn_stockpile_food_dispatch_system`): make the `JobClaim` check optional, identical to the haul branch in `htn_acquire_good_dispatch_system`. Comment block rewritten to credit subsistence behavior.

This is the fix from the prior attempt. The test fallout (Phase 2) is paid for separately.

### Phase 2 — Route subsistence deposits to the right storage

The dispatch chain ends in `Task::DepositToFactionStorage`. Today it targets `member.faction_id` (the village). For Market-preset households, this puts food in the village granary that the worker doesn't own — wrong. Resolve the deposit target by walking the membership hierarchy:

```rust
fn subsistence_deposit_faction(
    member: &FactionMember,
    household: Option<&HouseholdMember>,
    registry: &FactionRegistry,
) -> u32 {
    // Prefer household sub-faction if it owns a storage tile; else the village.
    household
        .and_then(|h| registry.factions.get(&h.household_id))
        .filter(|f| f.caps.storage.is_tile_backed())
        .map(|f| h.unwrap().household_id)
        .unwrap_or(member.faction_id)
}
```

Call this in `htn_stockpile_food_dispatch_system` when computing `scavenge_deposit_tile`, `forage_deposit_tile`, etc. (`htn.rs:5042–5056` and sister blocks). Subsistence + Mixed villages keep village-storage routing because households don't have their own tiles there.

### Phase 3 — Test fixture support: `seed_faction_food` + `idle_during_warmup`

Two helpers on `TestSim` close the 17-test regression cleanly:

- `sim.seed_faction_food(faction_id, calories)` — directly bumps `faction.storage.totals` for the faction's edible category so `prioritize_food` is false during warmup. Already half-built; just needs to be public and documented.
- `PersonBuilder::dormant()` — spawns the agent with `LodLevel::Dormant` so dispatchers skip it during warmup. Test calls `sim.wake(person)` before the act-phase assertions.

Failing tests get a one-line edit each — `b.dormant()` in the spawn closure or `seed_faction_food` before `tick_n`. The mechanical churn is real but bounded and doesn't touch any production code.

### Phase 4 — Scorer-registry refactor of `goal_update_system`

Replace the procedural ladder with a `GoalScorer` trait mirroring the existing `Method` trait:

```rust
pub trait GoalScorer: Send + Sync {
    fn goal(&self) -> AgentGoal;
    fn class(&self) -> GoalClass; // Crisis | Subsistence | Coordination | Enterprise | Discretionary
    fn score(&self, ctx: &GoalScoringCtx) -> Option<f32>; // None = inapplicable
    fn name(&self) -> &'static str;
}

pub struct GoalRegistry(pub Vec<Box<dyn GoalScorer>>);
```

`GoalScoringCtx` bundles what the current ladder reads: `&Needs`, `&EconomicAgent`, `Option<&HouseholdMember>`, `Option<&JobClaim>`, faction food/material ratios, calendar, `Disposition`, and a handful of resource queries. Built once per agent per tick, passed read-only to every scorer.

`goal_update_system` becomes:

```rust
let mut best = (AgentGoal::Idle, f32::MIN, "default");
for scorer in &registry.0 {
    let Some(score) = scorer.score(&ctx) else { continue; };
    // Crisis class hard-preempts: any positive Crisis score wins over any non-Crisis.
    let tiered = score + tier_bias(scorer.class());
    if tiered > best.1 { best = (scorer.goal(), tiered, scorer.name()); }
}
*goal = best.0;
```

Registered scorers replicate the current ladder one-to-one in Phase 4 — same triggers, same precedence via `tier_bias`. No behaviour change. The refactor is the win: every future motivation is one new scorer + one registration.

`GoalClass::Crisis` (Survive/Defend/Rescue/MigrateToCamp) gets `tier_bias = 1000.0`. `Subsistence` gets `100.0`. `Coordination` (claim-driven) gets `50.0` and only scores positive when a `JobClaim` is held. `Discretionary` (Socialize/Play) gets `0.0`. Within a tier, scores decide.

### Phase 5 — `Disposition` + ε-greedy

Add `Disposition([u8; 4])` Component: `industrious`, `social`, `entrepreneurial`, `risk_tolerant`. Default `[128; 4]`. Inherited at birth: `child = lerp(mom, dad) + jitter([-16, 16])`. Surfaced in the inspector.

Each scorer reads `ctx.disposition` and applies a bias. Examples:
- `GatherFoodScorer.score` adds `+0.5 * industrious_norm` (some agents work harder at stockpiling).
- `SocializeScorer.score` adds `+1.0 * social_norm`.
- A future `EarnIncomeScorer.score` adds `+1.0 * entrepreneurial_norm`.

ε-greedy lives in `goal_update_system`'s selection step:

```rust
let epsilon = match best.tier {
    GoalClass::Crisis => 0.0,         // never explore in a crisis
    GoalClass::Subsistence => 0.05,   // small jitter
    GoalClass::Coordination => 0.10,  // try other postings sometimes
    GoalClass::Enterprise => 0.20,    // entrepreneurs try new ventures
    GoalClass::Discretionary => 0.30, // play vs socialize is mostly whim
};
if fastrand::f32() < epsilon {
    // pick weighted-random over candidates within tier whose score
    // is within EPSILON_WINDOW (0.5) of the best
    let alts = candidates.filter(|c| c.tier == best.tier && best.score - c.score < 0.5);
    *goal = weighted_random_choice(alts).0;
}
```

Per-agent stochastic streams seed from `Entity::index()` + `clock.tick / 200` so the choice is stable for the 200-tick `goal_update_system` cycle but varies across cycles.

### Phase 6 (deferred — future plans) — Personal-enterprise goals

The scorer registry enables a clean follow-up:

- **`AgentGoal::EarnIncome`** — agent has high `entrepreneurial` *and* unmet currency-gated need (Esteem-tier contracts, planned tool purchase). Scorer looks at posting board for paid postings with high `U_bid`; high entrepreneurial bias makes the agent prefer wage labor over subsistence stockpiling. Wires to existing paid-posting U_bid scoring (`jobs.rs:2121–2140`).
- **`AgentGoal::SaveForPurchase { target_resource, target_qty }`** — agent commits to accumulating currency for a specific buy; biases all economic scores upward until met.
- **`AgentGoal::Trader`** — entrepreneurial-disposition + Trader profession; layers on top of existing `TraderPlan` system.
- **`AgentGoal::PersonalBuild`** — agent posts their own house blueprint and gathers/builds it themselves.

Each of these is one new scorer file + the goal-enum variant + an HTN dispatch system (or reuse of an existing one). The scorer registry is the contract; no edits to `goal_update_system` per addition.

## Why this design

- **Honest semantics.** Phase 1+2 acknowledges subsistence stockpiling for what it is — autonomous personal reflex, no market involved. The "worker pays themselves" hack disappears.
- **Test fallout addressed at source.** Phase 3 helpers fix the broken test pattern (warmup with low food) rather than constraining production behavior to satisfy fixtures.
- **Refactor pays for itself immediately.** The scorer registry replaces a procedural ladder that everyone editing `goals.rs` has feared. The diff in Phase 4 is mostly mechanical translation; behavior is preserved.
- **Extensibility built-in, not retrofit.** ε-greedy and `Disposition` slot into the registry without further architecture. Personal-enterprise goals (Phase 6) become content additions.
- **All three economy modes work coherently throughout.** Subsistence: chief postings dominate, autonomous fallback rarely fires. Mixed: chief + household postings; autonomous fills gaps. Market: no chief postings, no household coordination, agents autonomously stockpile for their own household *and* claim paid postings from household buy-orders (M4/M5 future) for currency. The same dispatcher serves all three.

## Critical files

- `src/simulation/htn.rs:4981–5004` — drop claim gate (Phase 1).
- `src/simulation/htn.rs:5042–5056` + sisters — household-aware deposit target (Phase 2).
- `src/simulation/test_fixture.rs` — `seed_faction_food`, `PersonBuilder::dormant`, `sim.wake` (Phase 3).
- `src/simulation/goals.rs:585–825` — scorer registry refactor (Phase 4).
- `src/simulation/goals.rs` (new file `goal_scorers.rs`) — individual scorer impls.
- `src/simulation/person.rs::PersonBundle` — `Disposition` component, inheritance hook in `pregnancy_system` (Phase 5).
- `src/simulation/CLAUDE.md` — replace the goal-cadence paragraph with a "Goals → Scorer Registry → HTN" description.

## Verification

- `cargo check && cargo test --bin civgame` — Phase 1+2 expect 17 failures; Phase 3 fixes them. End state of Phases 1–3: ≥497 tests passing (current 497 + the three regression-guard tests).
- Phase 4 ships with a behaviour-equivalence test: spawn an agent in each canonical state (hungry, tired, low-food-faction, hauling, idle-rich, etc.) and assert the new system picks the same goal as the old ladder. Lock this in before deleting the old code.
- Phase 5 ships with a determinism test: same seed, same Disposition, same world → same goal selection (modulo the ε-greedy stochastic stream, which is itself deterministic per `(entity, tick/200)`).
- `cargo run` post-Phase-5: inspector shows Disposition values and selected goal's tier. Spawn 6 founders, observe behavioural variety (some idle workers self-select into Socialize where the old system would have all-picked GatherFood).
