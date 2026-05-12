# Wage-Aware Labor Market: Skill, Capital, Apprenticeship

## Context

Today the goal/HTN system is invariant across economy modes. A market-mode agent picks Farm because the chief posted a Farm job, not because farming pays better than hauling. `faction_profession_system` promotes Farmers on a food-per-head threshold (`src/simulation/faction.rs:972`), `faction_hunter_assignment_system` allocates Hunters on a fixed ~20% headcount (line 172). Neither sees wages, skills, owned capital, or the cost of switching professions.

Real labor economics has sunk costs (accumulated skill, owned tools, household workshops, tenured plots), switching costs (lost competence, apprenticeship time), and information frictions (you only know what people you talk to are earning). A coefficient tweak on profession assignment doesn't model any of this.

This plan extends `plans/autonomous-food-labor-market-completion.md` Phase 6 (currently deferred — EarnIncome, SaveForPurchase, Trader, PersonalBuild) into a coherent labor-economics system. It composes with the scorer-registry refactor (that plan's Phase 4) and Disposition/ε-greedy layer (Phase 5).

**Critical precondition bug:** `JobCompletedEvent` is fired (`src/simulation/jobs.rs:2759`) but no system consumes it to call `pay()`. Workers currently receive zero currency for completed jobs; `JobEscrow.on_remove` refunds the poster instead. Until this is fixed, every wage signal is zero and the rest of the plan is dead weight.

User-decided design choices baked in:
- **Skill decay:** slow drift when unused, with a peak-derived floor.
- **Workshops:** unified through existing sub-faction model — households are factions, so household-owned workshops just live in `WorkbenchMap.by_faction[household_id]`.
- **Apprenticeship:** threshold-gated, crafts only. Farming/hunting/labor learn by doing.

## Design

### Phase 0 — Wage payout (precondition, blocks everything)

`src/simulation/jobs.rs` — new `job_payout_system` (Sequential, after completion):

```rust
fn job_payout_system(
    mut completed: EventReader<JobCompletedEvent>,
    mut commands: Commands,
    mut agents: Query<&mut EconomicAgent>,
    postings: Query<(&JobPosting, &JobEscrow, &JobClaim)>,
) {
    for evt in completed.read() {
        let (post, escrow, claim) = postings.get(evt.job_id)?;
        if let Some(worker) = claim.worker {
            pay_from_escrow(&mut commands, escrow, worker, post.reward);
        }
        commands.entity(evt.job_id).remove::<JobEscrow>(); // suppress refund hook
    }
}
```

Uses existing `pay()` (`src/economy/transactions.rs:14`). Records the payout in the worker's new `Earnings` ring (Phase 3) so wage signal builds immediately.

### Phase 1 — Skill model upgrade

`src/simulation/skills.rs` already has `Skills([u32; 8])` with `gain_xp()`, unbounded, no decay (verified). Add:

- `SkillPeaks([u32; 8])` component — historical max per skill, updated whenever `Skills` rises.
- Explicit ceiling: clamp Skills at `SKILL_MAX = 255` (matches existing `skill_norm` normalization at `jobs.rs:2409`).
- `skill_decay_system` (Economy set, daily cadence `TICKS_PER_DAY`):
  - For each agent, for each skill: if `last_used_tick > 1 game-day ago`, drift `skill -= 1`.
  - **Floor** = `max(SKILL_FLOOR_BASE, peak * 0.3)` where `SKILL_FLOOR_BASE = 5` (default), bumped to `30` once peak ever crossed `MASTERY_LINE = 80` ("you don't forget how to ride a bike").
  - `last_used_tick: [u32; 8]` written by every gather/dig/craft/build XP grant.
- Skills preserved across profession switch (today's behavior); decay applies regardless of profession — practicing keeps it sharp.

### Phase 2 — Capital recognition

**Tools (personal capital, already half-modeled):**

`src/simulation/items.rs` — extend `Item` enum's metadata with a `tool_profession: Option<Profession>` mapping (Hoe→Farmer, Bow→Hunter, Hammer→Builder, Awl/Loom-shuttle→Crafter, Spear→Hunter/Combat). Existing `has_tool()` check (`gather.rs:572`) stays; new `tool_capital_factor(agent, profession)` returns 1.0 base, +0.5 if agent carries a profession-affine tool.

No durability in this plan — out of scope unless explicitly added later.

**Workshops (sub-faction capital):**

No new ownership type needed. The existing `WorkbenchMap.by_faction` already indexes per-faction; household sub-factions are factions; this just works once households are allowed to post workshop blueprints onto their own plots.

Changes:
- `BuildIntent::Workbench` / `Forge` / `Loom` posted by a household poster lands `faction_id = household_id` on the resulting Workbench tile.
- `workshop_capital_factor(agent, profession)`: 1.0 base, +0.5 if agent's village faction has a profession-affine workshop, +1.0 if agent's *household* (sub-faction) has one. Stacks the village floor with the private upgrade.
- Cross-household access uses existing wage-paying contract path (a household with no forge claims a paid posting from another household to use theirs) — deferred to a future plan but mechanically already supported.

**Plot–profession affinity:**

A household with `Tenure::Sharecropping` or `Freehold` on Farmland already has its yield routed to itself (`autonomous-food-labor-market-completion.md` Phase 2 work). Wage calc reads this: `land_capital_factor(agent, Farmer)` = +0.5 if agent's household holds a Farmland plot. Switching out of Farmer when held = lost harvest claim (counted as switching cost).

### Phase 3 — Earnings memory + wage signal + gossip

Per-agent:
- `Earnings(VecDeque<EarningEntry>)` ring, cap 16. Each entry `{ job_kind, profession_used, amount, tick }`. Pushed by Phase 0's `job_payout_system`.

Per-faction:
- `FactionData.wage_signal: AHashMap<JobKind, WageEMA>` where `WageEMA { ema_per_tick: f32, last_update: u32 }`.
- `faction_wage_signal_system` (Economy, daily) folds the day's payouts into each kind's EMA with half-life ~5 game-days.

Cross-faction (information friction):
- Reuse gossip (`src/simulation/memory.rs` / `gossip.rs`). Agents who interact (Socialize task, market visit, raid prisoner exchange) merge each other's `Earnings` summaries into their `PerceivedFactionWages: AHashMap<(faction_id, JobKind), f32>`.
- Perceived wages are stale-noisy estimates of *other* factions' rates; own faction's rates are read directly. Migration & cross-faction job claims (future) consume the perceived view.

### Phase 4 — Expected-value profession choice

Replace the threshold cores of `faction_profession_system` and `faction_hunter_assignment_system` with a unified per-agent calc, runs at `TICKS_PER_DAY/4` (existing cadence):

```
expected_wage(agent, p) = faction.wage_signal[job_kind(p)].per_tick
                       * skill_competence(agent.Skills[p])
                       * (tool_capital_factor + workshop_capital_factor + land_capital_factor) / 3

skill_competence(s) = 0.2 + (s / SKILL_MAX) * 0.8    // floor at 0.2 so newbies aren't useless

expected_value(agent, p) = expected_wage(p) * EXPECTED_TENURE_TICKS
                         - switching_cost(agent.current, p)

switching_cost(current, target) =
      apprenticeship_opportunity_cost(agent, current, target)   // 0 if not needed
    + capital_abandonment_loss(agent, current, target)          // tools/workshop/land tied to current
    + skill_regret(agent.SkillPeaks[current])                   // present-value of decayed future earnings
```

Decision rule (replaces fixed thresholds):
1. Compute `expected_value(p)` for every `p ∈ Profession` plus the `None` option (opportunistic labor / paid postings).
2. **Survival override:** if faction `food_per_head < FARMER_SURVIVAL_FLOOR`, force one Farmer promotion regardless of wages (preserves the bug-fix shipped in autonomous-food-labor-market-completion Phase 1).
3. Pick argmax `p*`. Switch only if `expected_value(p*) > expected_value(current) * 1.20` (20% hysteresis margin).
4. If target requires apprenticeship (Phase 5), set `Profession::Apprentice { … }` instead of direct switch.

Faction-level constraints stay: Hunter cap at `adults/2`, Bureaucrat appointment driven by `state_funds_public_works`. The faction system enforces these as post-filters on agents' individual choices.

### Phase 5 — Apprenticeship (crafts only)

`src/simulation/person.rs` — extend Profession enum:

```rust
pub enum Profession {
    None, Farmer, Hunter, Crafter, Trader, Bureaucrat, // existing
    Apprentice { master_profession: Profession, mentor: Option<Entity>,
                 progress_ticks: u32, target_ticks: u32 },
}
```

Trigger conditions (Phase 4 decision):
- Target is a *craft* profession (initially Crafter; add others as they appear).
- Agent's `Skills[target] < APPRENTICE_THRESHOLD = 30`.
- Mentor available: same-village agent with `Skills[target] >= MASTER_THRESHOLD = 100`, accepting apprentices (chief flag or self-elective).

If no mentor exists in the village → cannot enter the craft profession this tick; agent falls back to `Profession::None` (opportunistic labor); Phase 4 next tick re-evaluates.

Apprenticeship lifecycle:
- Duration: `TICKS_PER_DAY * 30` (one game-month, tunable constant).
- Daily `apprentice_progress_system` (Economy set):
  - Auto-emits paired `Task::Read { tech: associated }` and `Task::Teach` if both apprentice + mentor are idle (reuses `src/simulation/teaching.rs`).
  - Apprentice earns `WAGE_FRACTION_APPRENTICE = 0.4` of normal posting rewards (modify `job_claim_system` payout path to scale).
  - 10% of apprentice's earnings routed to mentor's `EconomicAgent.currency` as mentor fee.
  - Skill XP gained at 2× standard rate (deliberate practice).
- On completion: `Skills[target] = max(current, APPRENTICE_THRESHOLD)`, `Profession = master_profession`. Mentor relationship dissolved.
- Abort: apprentice can be drafted away by survival override; abort wastes proportional progress.

For non-craft professions (Farmer/Hunter/Labor) — no apprenticeship; switch is immediate but `skill_competence` starts at the 0.2 floor and ramps via XP, so a fresh farmer is genuinely worse than a seasoned one for many days.

### Phase 6 — HTN scorers + UI

- New `EarnIncomeScorer` in the scorer registry (from `autonomous-food-labor-market-completion.md` Phase 4): score = `posting.reward * skill_competence(p) - travel_cost`. Folds into the scorer-registry architecture cleanly instead of a separate goal variant.
- Inspector (`src/ui/inspector.rs` or wherever the person panel lives): show Skills + SkillPeaks bars, expected_wage[p] table, current Profession (with apprentice progress if applicable), Earnings totals last-30-days.
- Activity log: emit on wage payout, profession change, apprenticeship start/end.

### Phase 7 — Documentation

- `src/simulation/CLAUDE.md` — replace profession-assignment paragraph with the new expected-value model; document skill decay; document apprenticeship state.
- `CLAUDE.md` — economy-mode section gains: "Market/Mixed: agents pick profession by expected-value (wage × skill × capital − switching cost). Skills decay slowly when unused. Crafts require apprenticeship below skill 30."

## Phasing & rollout

**Ship order (recommended):** 0 → 3 → 1 → 4 → 2 → 5 → 6 → 7. Phase 0 unblocks measurement; Phase 3 instruments before behavior changes (so Phase 4's effect is observable). Phase 1 (decay) before Phase 4 (which reads peaks). Phase 2 (capital) before/with Phase 4 (which reads capital factors). Phase 5 (apprenticeship) is the heaviest single piece — easiest after the wage loop is closed and visible.

Each phase is independently testable and releases value:
- After 0: wages actually flow; existing trader/chief economy starts working as intended.
- After 0+3: dashboard of who earned what.
- After 0+1+3: skill decay observable.
- After 0+1+2+3+4: agents reassign professions by wage; immediate visible churn in market mode.
- After 5: craft-profession transitions go through mentor pairing.

## Files to modify

- `src/simulation/jobs.rs` — Phase 0 (`job_payout_system`); Phase 4 (faction profession refactor consumers); Phase 5 (apprentice wage scaling in claim payout).
- `src/economy/transactions.rs` — Phase 0 wire-up; `pay_from_escrow` helper.
- `src/simulation/skills.rs` — Phase 1 (`SkillPeaks`, `skill_decay_system`, `SKILL_MAX`, `last_used_tick`).
- `src/simulation/items.rs` — Phase 2 (`tool_profession` mapping).
- `src/simulation/construction.rs` — Phase 2 (allow household-faction workshop construction posters).
- `src/simulation/faction.rs:172, 972` — Phase 4 (rewrite cores of hunter/farmer assignment into `profession_choice_system`).
- `src/simulation/person.rs` — Phase 5 (Apprentice variant), Phase 1 (component bundle update).
- `src/simulation/teaching.rs` — Phase 5 (apprentice progress system uses Read/Teach tasks).
- `src/simulation/memory.rs` + `gossip.rs` — Phase 3 (wage gossip).
- `src/simulation/goals.rs` — Phase 6 (EarnIncomeScorer registration, depends on prior plan's Phase 4 scorer registry).
- `src/ui/inspector.rs` — Phase 6 (UI panels).
- `src/simulation/CLAUDE.md`, `CLAUDE.md` — Phase 7.

**Reuse, do not reinvent:** `pay()` (`src/economy/transactions.rs:14`), `JobEscrow` (`src/simulation/jobs.rs:380`), `SettlementMarket::sell_item` (`src/economy/market.rs:145`), `Task::Read`/`Task::Teach` (`src/simulation/teaching.rs`), `WorkbenchMap.by_faction`/`LoomMap.by_faction` (construction.rs), `Plot::tenure` + `household_land_acquisition_system` (land.rs), existing gossip merge (memory.rs), existing scorer registry (autonomous-food-labor-market-completion.md Phase 4).

## Verification

1. **Build/tests**: `cargo check && cargo test --bin civgame`. Current 440+ tests stay green.
2. **Phase 0 acceptance**: `cargo run`, post a paid contract, observe worker's `EconomicAgent.currency` rises by `reward` on completion; poster's escrow doesn't refund.
3. **Phase 1 acceptance**: spawn an agent, max one skill via repeated tasks, switch them off it for 30 in-game days; observe skill drift down to peak·0.3 (or 30 if peak crossed mastery), never below.
4. **Phase 4 acceptance** (the real test): `cargo run`, Market preset, era ≥ Neolithic. Spawn ≥ 12 founders; let it run a game-month. Inspector should show:
   - Heterogeneous professions reflecting wages (not all-Farmer fallback).
   - Agents with high `tool_capital_factor` or `land_capital_factor` for a profession stay in it through wage dips that would flip a tool-less agent.
   - At least one agent in `Profession::Apprentice` with a paired mentor.
   - Earnings log nonzero for paid postings.
5. **Subsistence regression**: same run, Subsistence preset. Profession churn should look like master — survival floor dominates, no wage-driven flipping. `wage_signal` stays empty.
6. **Mixed regression**: chief postings still claimed by their professions first; household private contracts populate the wage signal; apprenticeship triggers only when a craft household first appears.
7. **Determinism**: seeded runs produce identical profession-history per agent across replays (modulo Disposition ε-greedy which is itself seeded).

## Out of scope (explicit deferrals)

- Tool durability/wear — model exists in this plan as static affinity; durability is a separate add (touches every gather/craft executor).
- Cross-household workshop rental contracts — mechanically supported by existing job postings, but no auto-posting system here.
- Skill decay with biological aging (elder agents losing dexterity) — independent dimension.
- Inter-generational profession inheritance — children gain childhood XP from parents' professions; can be a follow-up that hooks `pregnancy_system` and childhood maturation.
- Faction-level capital investment decisions (which workshop to build next) — currently chief-driven via `chief_directive_system`; wage signal could feed it later.
