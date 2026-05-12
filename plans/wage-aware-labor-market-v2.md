# Wage-Aware Labor Market — v2

Comprehensive replacement for `plans/wage-aware-labor-market.md`, after auditing every claim against the current tree.

## Context

The existing plan diagnoses the right problem (profession assignment ignores wages/skills/capital/switching costs) and the right precondition bug (`JobCompletedEvent` has zero consumers, so workers are never paid). But several claims are stale or wrong, and one whole sub-system (`Profession::Crafter` + craft-profession dispatch) is assumed to exist when it doesn't. This v2 fixes those, adds an explicit chief-wage-source phase (chosen: faction treasury, Mixed/Market only), and broadens apprenticeship to crafts + medicine per user direction.

## Audit findings — what the original got wrong

| Claim in v1 plan | Reality |
|---|---|
| `Profession::Crafter` exists | **No.** Only `None / Farmer / Hunter / Bureaucrat / Trader` (`person.rs:52-70`). Craft work is goal-driven via `AgentGoal::Craft` + CraftOrder, no profession gate. Phase 5 must *introduce* Crafter before apprenticeship makes sense. |
| `has_tool()` at `gather.rs:572` | **No such function.** Equipment checks go through `Equipment::has_resource(rid)` against `core_ids::weapon() / shield() / armor()`. No profession-affine tool concept yet. |
| Granular tools (Hoe, Bow, Hammer, Awl, Loom-shuttle) | **No.** `core.ron` has generic `"tools"` and `"weapon"` resources. Affinity must come from catalog tags or new resource entries, not enum variants on `Item`. |
| `WorkbenchMap.by_faction[household_id]` | **No `by_faction` index.** `WorkbenchMap(AHashMap<(i32,i32), Entity>)` is tile-keyed (`construction.rs:78`). Workshops carry no `OwnedBy` component either; they aggregate to whatever faction posted the blueprint, but nothing reads that today. |
| `BuildIntent::Workbench / Forge / Loom` | **No.** Workshops are posted as `BuildIntent::Single(BuildSiteKind::Workbench)` (`construction.rs:1931-1949`). No dedicated intent for household workshop posting. |
| Phase 6 `EarnIncomeScorer` lands cleanly | **Hard dependency.** `GoalScorer` trait and `GoalRegistry` resource don't exist; they are deferred Phase 4 of `autonomous-food-labor-market-completion.md`. `Disposition` same story (its Phase 5). v2 lists Phase 6 as conditional on those shipping first. |

What the original got right (verified): the `JobCompletedEvent` ghost, `JobEscrow.on_remove` refund semantics (`jobs.rs:410-431`), `pay()` signature (`transactions.rs:165`), profession-system anchor points (`faction.rs:172 / 329 / 972`), `Skills([u32; 8])` shape, sharecrop yield routing live in `gather.rs` + `land.rs`, and the catalog `trade_base_value` source-of-truth pattern.

## Design choices baked in (user-confirmed)

- **Chief wage source:** Mixed/Market chief postings pull from `FactionData.treasury`; Subsistence chief stays unpaid (`reward = 0`). Couples coordination to fiscal health — bankrupt factions can't direct paid labor and fall back to communal/autonomous work.
- **Apprenticeship scope:** Crafts + Medicine (deliberate-practice professions). Farmer/Hunter/Builder/Trader/Bureaucrat learn by doing.
- **Skill decay:** slow drift to a peak-derived floor; daily linear at one unit/day is too aggressive — switched to a half-life model.
- **Capital ownership:** add a real `OwnedBy(u32)` component on workshop entities + a `WorkshopOwnership` resource indexed by faction. Household-keyed workshops then work without a special case.

## Phasing

Ship order: **0 → 1 → 2 → 3 → 4a → 4b → 5a → 5b → 6 → 7**. Each is independently testable and lands measurable value.

### Phase 0 — Wage payout (precondition; everything else is dead weight without this)

The bug, precise form:

- `JobCompletedEvent { job_id, faction_id, kind }` is emitted at 6 sites (`jobs.rs:1266, 2624, 2759, 2832, 2933` + `construction.rs:3652`). No `EventReader<JobCompletedEvent>`.
- `JobEscrow` lives on a sidecar entity. `JobEscrow.on_remove` (`jobs.rs:410-431`) refunds `beneficiary` (the poster) when `amount > 0` at despawn. Successful payout is the responsibility of the system that *fires* completion: zero `amount` and despawn the sidecar.
- The completion event doesn't carry a worker entity. `JobPosting.claimants: Vec<Entity>` does (`jobs.rs:249-275`); the worker is the claimant who actually completed (one or more depending on multi-worker postings).

**Implementation: `job_payout_system`** (`jobs.rs`, Sequential set, exclusive `&mut World` — `pay()` is `&mut World`):

1. Drain `Events<JobCompletedEvent>` into a local `Vec` (avoid event-reader vs world-write conflict).
2. For each event, look up `JobPosting` via `JobIndex` or `world.entity(job_id)`. Read `claimants` + the sidecar `JobEscrow.amount, beneficiary`.
3. **Split rule** (drives multi-worker postings — Build, Stockpile, Farm):
   - Total payout = `escrow.amount`.
   - For each completing claimant, pay `amount / completed_claimants` via `pay(world, beneficiary, worker, share)`.
   - Apprentice claimants (Phase 5) get `WAGE_FRACTION_APPRENTICE = 0.4` of their share, with the remaining 0.6 going to mentor if `MentorOf` link exists, else returns to `beneficiary`.
4. Decrement `escrow.amount` by total paid. Despawn sidecar — the `on_remove` hook sees `amount == 0` and no-ops.
5. Push an `Earnings` entry on each paid worker (Phase 3 component, introduced here as a stub `VecDeque<EarningEntry>` cap 16: `{ job_kind, target_rid: Option<ResourceId>, amount, tick }`).
6. Emit `ActivityEntryKind::WagePaid { worker, kind, amount }` for UI surfacing.

**Currency invariant** stays intact: poster→escrow→worker is a closed transfer; the existing `CurrencySnapshot::capture` assertion in `test_fixture.rs` already covers `JobEscrow.amount` so the invariant holds end-to-end through payout.

**Where completion fires today:** each of the 6 sites already runs in Sequential or Economy ordering. `job_payout_system` runs after **all** of them by ordering it `.after()` each producer system (or just `last_in_set(Sequential)` since the `Events<>` ring delivers across schedules).

**Test gate:** new fixture helper `TestSim::post_paid_contract(faction, poster, recipe, qty, reward)` + `assert_currency_delta(worker, +reward)` after running through completion. Currency-snapshot invariant assertion across the whole flow.

### Phase 1 — Skill model: peaks, ceiling, half-life decay

`skills.rs` (51 lines today) gets a careful upgrade:

```rust
pub const SKILL_MAX: u32 = 255;
pub const SKILL_FLOOR_BASE: u32 = 5;
pub const SKILL_MASTERY_LINE: u32 = 80;
pub const SKILL_MASTERED_FLOOR: u32 = 30;
pub const SKILL_PEAK_FLOOR_FRACTION: f32 = 0.30;
pub const SKILL_DECAY_HALF_LIFE_DAYS: u32 = 90;   // *unused* time to halve toward floor

#[derive(Component, Clone, Copy, Default)]
pub struct SkillPeaks(pub [u32; SKILL_COUNT]);

#[derive(Component, Clone, Copy)]
pub struct SkillUseTicks(pub [u32; SKILL_COUNT]); // last tick each skill earned XP
```

- `Skills::gain_xp(...)` rewritten to `Skills::practice(&mut self, peaks, use_ticks, kind, amount, tick)` so every XP grant atomically (a) clamps to `SKILL_MAX`, (b) bumps peak, (c) stamps `use_ticks[kind] = tick`. Migrate every call site via grep — there are ~12 of them across gather/dig/construction/crafting/teaching/combat.
- `skill_decay_system` (Economy set, every `TICKS_PER_DAY`):
  - For each agent, each skill: if `now - use_ticks[kind] >= TICKS_PER_DAY`,
  - `floor = peak >= SKILL_MASTERY_LINE ? SKILL_MASTERED_FLOOR : max(SKILL_FLOOR_BASE, (peak as f32 * 0.30) as u32)`,
  - `skill = floor + ((skill - floor) as f32 * 0.5_f32.powf(1.0 / SKILL_DECAY_HALF_LIFE_DAYS as f32)) as u32` (slow exponential decay).
  - Skill stays at or above `floor` always.
- Spawn sites: `PersonBundle` (in `person.rs`) gets `SkillPeaks::default()` + `SkillUseTicks([0; 8])`. Newborn inheritance unchanged (only `Skills` matters at birth; peaks reset to current).

### Phase 2 — Capital recognition (tools, workshops, land)

#### 2a. Profession-affine tools via catalog tags

The clean version: catalog-driven, not enum-driven.

- `assets/data/resources/core.ron`: split today's generic `"tools"` into kind-specific entries: `hoe`, `awl`, `loom_shuttle`, `hammer`, `bow`. Each carries `tags: ["tool", "<profession-tag>"]`. `weapon` is already separate.
- `core_ids::hoe()`, `core_ids::awl()`, etc. (accessor pattern matches existing weapon/shield).
- `ResourceId::tool_profession(catalog) -> Option<Profession>` — reads tags. `"tool", "farming"` → Farmer; `"tool", "crafting"` → Crafter; `"tool", "building"` → Builder (skill-based; no profession yet, factor falls back); `"weapon", "hunting"` → Hunter.
- Existing craft recipes that produce `"tools"` get updated to produce specific tool kinds, gated on tech where appropriate (e.g. Hoe gated on `AGRICULTURE` or similar).
- `tool_capital_factor(agent_inventory, profession) -> f32`: 1.0 base; +0.5 if any inventory item maps to `profession` via `tool_profession`. **No durability** — explicitly deferred.

#### 2b. Workshop ownership and affinity

- New component `OwnedBy(u32)` (faction_id) on workshop tiles (Workbench, Forge, Loom, Granary, Shrine, Market, Barracks, Monument). Set at spawn from `BuildIntent.poster_faction_id` (today implicit via `chief_directive_system`; thread it through).
- New resource `WorkshopOwnership` indexing `AHashMap<u32, Vec<Entity>>` (faction → workshop entities). Populated by `on_add` hook for `OwnedBy`, drained by `on_remove`.
- `workshop_capital_factor(agent, profession) -> f32`:
  - 1.0 base.
  - +0.5 if agent's village faction owns a profession-affine workshop within `WORKSHOP_AFFINITY_RADIUS = 12` of the agent.
  - +1.0 if agent's household sub-faction (per `HouseholdMember`) owns one.
  - Profession→workshop map: Crafter→Workbench/Loom/Forge, Bureaucrat→Market, Healer (future)→Shrine.

#### 2c. Land affinity (already shipped)

`land_capital_factor(agent, Farmer)` = 1.0 + 0.5 if agent's `HouseholdMember.household_id` holds (`Tenure::Freehold | Sharecropping`) any `ZoneKind::Agricultural` plot, queried via `PlotIndex.by_faction_hash` + tenure walk. Sharecrop already routes yield correctly (`gather::lookup_sharecrop_split`, `land.rs:835-856`) so this is pure read-side.

### Phase 3 — Earnings memory + faction wage signal + gossip

#### Per-agent
- `Earnings(VecDeque<EarningEntry>)` ring component, cap 16. Pushed by Phase 0. Read by inspector + wage-signal aggregator.

#### Per-faction
- `FactionData.wage_signal: AHashMap<(JobKind, Option<ResourceId>), WageEMA>`
- `WageEMA { ema_per_tick: f32, last_update: u32 }`
- Keying by `(kind, rid)` separates `Stockpile{wheat}` from `Stockpile{wood}` (the v1 plan conflated these).
- `faction_wage_signal_system` (Economy, daily): for each faction, sums each `(kind,rid)` payout from that day's Earnings entries across members, divides by total ticks worked (approx: # claimants × duration → approximate via `JobClaim.posted_tick` to completion), folds into EMA with **α = 1 − 0.5^(1/5) ≈ 0.129** (5-day half-life). EMA decays toward zero when no payouts arrive.

#### Cross-faction (information friction)
- `PerceivedFactionWages` Component on agents: `AHashMap<(u32 /*fid*/, JobKind, Option<ResourceId>), (f32, u32 /*tick*/)>`.
- Gossip merge: piggyback `Socialize` task in `social_fill_system` / `awareness_gossip_system` (`memory.rs`/`gossip.rs`). When two agents socialize, each merges the other's most recent ≤4 wage entries with `tick` decay penalty.
- Migration and cross-faction posting choices (future) read `PerceivedFactionWages`; same-faction work reads `wage_signal` directly. **No global broadcast** — wage spread between settlements is real economic friction.

### Phase 4 — Chief-funded posting wages + EV profession choice

#### 4a. Chief postings pay from faction treasury (Mixed/Market only)

`chief_job_posting_system` (`jobs.rs`) is the only point of authority. Today `reward = 0` unconditionally. After:

- For each posting type the chief is about to create, consult `faction.economic_policy[target_rid].chief_allocates_labor`. (Already used elsewhere — see policy_gate.)
- If chief is allocating but `caps.income.household_skim_pct > 0` (Mixed/Market), set `reward = chief_wage_for(kind, target_rid, qty, &faction.wage_signal, &catalog)` and debit `faction.first_settlement_or_treasury(...)` by that amount, creating a `JobEscrow` with `beneficiary = chief_entity`. If treasury can't cover the wage, fall back to `reward = 0` (free communal labor — represents fiscal distress).
- `chief_wage_for(...)`:
  - For Stockpile{rid}: `catalog.trade_base_value(rid) * qty * CHIEF_MARGIN (0.5)` — chief pays half of market value to encourage uptake without bidding wars.
  - For Build: fixed `CHIEF_BUILD_WAGE_PER_DAY (3.0) * expected_days` where expected_days = sum of slot quantities / typical fill rate. Bounded by `CHIEF_BUILD_WAGE_CAP (30.0)`.
  - For Craft: `recipe.output_value(catalog) * CHIEF_MARGIN`.
  - For Farm: fixed `CHIEF_FARM_WAGE_PER_DAY * expected_days`.
- Subsistence factions (policy map empty) → unchanged, `reward = 0`. No bootstrapping fragility.

Currency invariant grows by `treasury.currency` flow into escrow — already covered by snapshot.

#### 4b. Unified `profession_choice_system`

Replaces the *cores* (not the cadence wrappers) of `faction_profession_system`, `faction_hunter_assignment_system`, and `chief_bureaucrat_appointment_system`. Each cadence wrapper stays as the trigger; the inner allocation logic delegates to a shared per-agent EV computation.

Per agent, per allocation pass:

```
skill_competence(s) = 0.2 + (s.min(SKILL_MAX) as f32 / SKILL_MAX as f32) * 0.8
                           // floor 0.2 so newbies aren't useless

capital_factor(agent, p) =
    (tool_capital_factor(agent, p) + workshop_capital_factor(agent, p) + land_capital_factor(agent, p)) / 3.0

expected_wage(agent, p, faction) =
    faction.wage_signal.get_aggregate(job_kinds_for(p))
        * skill_competence(skill_of(p))
        * capital_factor(agent, p)

expected_value(agent, p) =
    expected_wage(agent, p) * EXPECTED_TENURE_TICKS         // 60 game-days
    - switching_cost(agent.current, p)

switching_cost(current, target) =
      apprenticeship_opportunity_cost(target)               // 0 unless target is Crafter/Healer + skill < 30
    + capital_abandonment(agent, current, target)           // sum of tied capital factor * EV_TIED
    + skill_regret(agent.skill_peaks[skill_of(current)])    // pv of decayed earnings if peak forgotten
```

**Decision rule:**

1. Compute EV for every `p ∈ {None, Farmer, Hunter, Crafter, Trader, Bureaucrat, Healer}` (Healer = Phase 5b stretch).
2. **Survival override** (preserve `faction_profession_system` semantics): if `faction.storage.food_total() / member_count < FARMER_SURVIVAL_FLOOR (16.0)`, force at least one Farmer; if no current Farmer exists, promote the agent with max `skill_competence(Farming)` regardless of EV.
3. **Faction caps** (post-filter): Hunter ≤ `adults/2`; Bureaucrat ≤ `member_count * BUREAUCRAT_MAX_RATIO` and gated on `state_funds_public_works`; Crafter ≤ `adults/3`. Caps applied by sorting agents by EV(target) desc and accepting until cap reached.
4. **Hysteresis**: switch only if `EV(p*) > EV(current) * 1.20` AND `EV(p*) > 0`.
5. **Apprenticeship gate** (Phase 5): if target ∈ {Crafter, Healer} and `skill < APPRENTICE_THRESHOLD = 30`, route through Apprentice variant instead of direct switch.

**Cadence:** runs at `PROFESSION_REVIEW_CADENCE = TICKS_PER_DAY/4` (same as today's farmer/hunter cadence). Per-game-day budget per faction so it doesn't recompute for every agent every cadence cycle — round-robin a bucket of `member_count / 4` agents per tick.

**Old systems retired:** the cores of `faction_profession_system:972` and `faction_hunter_assignment_system:172` and `chief_bureaucrat_appointment_system:329` get replaced; the survival floor stays as a hard override outside the EV path. Demote-teardown plumbing (`aq.cancel()`, strip `Carrying`, reset `task_id`) is reused — extract into `demote_profession(world, entity, old_prof)` helper called by both the legacy fallback and the new system.

### Phase 5 — Introduce Crafter + Apprenticeship

#### 5a. `Profession::Crafter` + dispatch integration

`person.rs::Profession` gains `Crafter` (and stretch `Healer`):

```rust
pub enum Profession {
    None, Farmer, Hunter, Bureaucrat, Trader,
    Crafter,
    Healer,                                     // Phase 5b — Medicine apprenticeship
    Apprentice {
        target: ApprenticeTarget,               // Crafter | Healer
        mentor: Option<Entity>,
        progress_ticks: u32,
        target_ticks: u32,                      // TICKS_PER_DAY * 30
    },
}
```

**Crafter dispatch integration** — without this, the profession is decorative:

- `chief_job_posting_system` Craft branch: prefer Crafter claimants when scoring `U_bid` — add `+CRAFTER_AFFINITY_BONUS (3.0)` to bids where the claimant's profession matches.
- `htn_work_on_craft_order_dispatch_system`: existing `profession_gate` mechanism (per `htn.rs` Method trait) — add a `profession_gate: Some(Profession::Crafter)` on `WorkOnSatisfiedCraftOrderMethod` with a softer fallback (still allows non-Crafter claim, but Crafter ranks higher).
- `chief_craft_assignment_system` (new, mirrors hunter pattern): targets `max(1, adults/4)` Crafters when `wage_signal[(Craft, _)].ema > 0` (i.e., there's craft work being paid for). Demotes when signal collapses.

#### 5b. Apprenticeship — crafts + medicine

```rust
pub enum ApprenticeTarget { Crafter, Healer }
```

**Entry conditions** (Phase 4 decision):
- Target ∈ {Crafter, Healer}.
- `Skills[target_skill] < APPRENTICE_THRESHOLD = 30` where `target_skill = Crafting` for Crafter, `Medicine` for Healer.
- Mentor available: same-village agent with `Skills[target_skill] >= MASTER_THRESHOLD = 100` and `Profession ∈ {Crafter, Healer}` (accepting apprentices is implicit; per-mentor cap of 1 enforced).
- No mentor available → fall back to `Profession::None`; Phase 4 re-evaluates next cadence.

**Apprenticeship lifecycle** (`teaching.rs` extension, mirrors `TeachingPair` pattern):
- New components: `MentorOf(Entity)` on master, `ApprenticeOf(Entity)` on apprentice. One-to-one (per-mentor cap 1).
- `apprentice_progress_system` (Economy, daily):
  - If both idle: emit paired `Task::Read { tech: era_tech_for(target) }` on apprentice + `Task::Teach { tech }` on mentor.
  - Skill XP from any craft/medicine activity granted at **2.0× rate** to apprentice (deliberate-practice multiplier; multiply through `Skills::practice`).
  - `progress_ticks += TICKS_PER_DAY` per day.
  - On completion (`progress_ticks >= target_ticks = TICKS_PER_DAY*30`): set `Skills[target_skill] = max(current, APPRENTICE_THRESHOLD)`, set profession to `target`, despawn the `MentorOf`/`ApprenticeOf` links.
- **Apprentice wage scaling** (Phase 0 hook): in `job_payout_system`'s per-claimant split, if claimant has `ApprenticeOf(mentor_e)`, pay claimant `share * 0.4` and mentor `share * 0.1` (mentor fee), refund `share * 0.5` to escrow beneficiary (cheaper labor = poster keeps half).
- **Abort path:** survival override (`faction.storage.food_total < members * 4`) demotes ALL apprentices in the faction to Farmer for the duration. Progress is paused, not lost — `progress_ticks` survives. Mentor link dissolved only if abort > 1 game-week (auto-rebind on next eligibility pass).

### Phase 6 — HTN scorer + UI (depends on peer plan)

Conditional on `autonomous-food-labor-market-completion.md` Phase 4 (scorer registry) and Phase 5 (Disposition) shipping. If those don't land in time, fold Phase 6 into the existing `goal_update_system` cascade as a procedural branch — degraded but functional.

- **`EarnIncomeScorer`** (`goal_scorers.rs`): `score = best_posting.reward * skill_competence(p) * (1 + disposition.entrepreneurial / 255.0) - travel_cost`. Tier = `GoalClass::Enterprise`.
- **Inspector** (`ui/inspector.rs`): Skills + SkillPeaks bars (with floor line), per-profession `expected_wage` table, current Profession (with apprentice progress bar if applicable), last-30-days Earnings total, gossip-perceived wages for other factions.
- **Activity log**: emit on wage payout, profession change, apprenticeship start/end, mentor binding.

### Phase 7 — Documentation

- `src/simulation/CLAUDE.md` — replace profession-assignment paragraph with the EV model; document skill decay (half-life formula, peaks, mastery floor); document `OwnedBy` + `WorkshopOwnership` + new tool-resource scheme; document apprenticeship (`MentorOf`/`ApprenticeOf`, scope = Crafter+Healer).
- `CLAUDE.md` (root) — Mixed/Market section: chief postings pay from faction treasury; agents pick profession by EV (wage × skill × capital − switching cost); skills decay slowly; Crafter/Healer gated by apprenticeship below skill 30.

## Files to touch

| File | Phase | What |
|---|---|---|
| `src/simulation/jobs.rs` | 0, 4a, 5b | `job_payout_system`; `chief_job_posting_system` reward funding; apprentice payout split |
| `src/economy/transactions.rs` | 0 | Helper `pay_from_escrow(world, escrow_entity, worker, share)` |
| `src/simulation/skills.rs` | 1 | `SkillPeaks`, `SkillUseTicks`, `Skills::practice`, `SKILL_MAX`, `skill_decay_system` |
| `src/simulation/items.rs`, `assets/data/resources/core.ron`, `src/economy/resources.rs` | 2a | Tool resources, `ResourceId::tool_profession` |
| `src/simulation/construction.rs` | 2b | `OwnedBy` component on workshops, set at spawn |
| `src/simulation/faction.rs` (new file `workshop_ownership.rs`) | 2b | `WorkshopOwnership` resource + add/remove hooks |
| `src/simulation/land.rs` | 2c | `land_capital_factor` helper (read-only) |
| `src/simulation/memory.rs`, `gossip.rs` | 3 | `PerceivedFactionWages`, gossip merge in `awareness_gossip_system` |
| `src/simulation/faction.rs:172, 329, 972` (new `profession_choice.rs`) | 4b | Shared `profession_choice_system`, extracted `demote_profession()` helper |
| `src/simulation/person.rs` | 1, 5a, 5b | `SkillPeaks`/`SkillUseTicks` in `PersonBundle`; `Profession::Crafter / Healer / Apprentice` variants |
| `src/simulation/htn.rs` | 5a | Crafter affinity bonus in craft-order claim scoring |
| `src/simulation/teaching.rs` | 5b | `MentorOf`/`ApprenticeOf` components, `apprentice_progress_system` |
| `src/simulation/test_fixture.rs` | 0, 1, 4b | `post_paid_contract`, `assert_currency_delta`, `advance_days`, `seed_master_in_skill` |
| `src/simulation/goals.rs`, `goal_scorers.rs` | 6 | `EarnIncomeScorer` (conditional) |
| `src/ui/inspector.rs` | 6 | UI panels |
| `src/simulation/CLAUDE.md`, `CLAUDE.md` | 7 | Doc updates |

**Reuse, don't reinvent:** `pay()` (`transactions.rs:165`), `JobEscrow` lifecycle hook (`jobs.rs:1105`), `Task::Read`/`Task::Teach` (`teaching.rs`), `PlotIndex.by_faction_hash` (`land.rs`), existing `awareness_gossip_system` merge (`memory.rs`), existing `policy_gate` mechanism on Methods (`htn.rs`), `chief_directive_system` per-faction posting cap.

## Verification

1. **Build/tests:** `cargo check && cargo test --bin civgame`. Baseline 503 tests stays green.

2. **Phase 0 acceptance:** TestSim — spawn one agent, post a paid Craft contract with `reward = 10.0`, run through completion. Assert `worker.currency == 10.0`, escrow despawned, currency snapshot invariant holds within `eps = 0.01`.

3. **Phase 1 acceptance:** TestSim — spawn agent, grant Crafting XP repeatedly until skill > 200; switch profession (Skills::practice stops). Fast-forward 90 game-days. Assert `skill ≈ 200 * 0.5 + floor * 0.5` (one half-life). Confirm floor = max(5, 200 * 0.3) = 60.

4. **Phase 2 acceptance:** `cargo run`, Mixed preset, Neolithic. Spawn agent with `hoe` in inventory; verify EV for Farmer is observably higher than for an identical agent without. Build a household-owned Workbench; verify EV for Crafter on that household's member is higher than for a non-household member.

5. **Phase 3 acceptance:** Run Market preset 5 game-days. Inspector shows `wage_signal[(Stockpile, wheat)].ema_per_tick > 0`. Two factions with different production mixes show different signals; an agent who socialized across factions has matching `PerceivedFactionWages`.

6. **Phase 4 acceptance (the real test):** `cargo run`, Mixed preset, era ≥ Neolithic. Spawn ≥ 12 founders; let run a game-month. Inspector should show:
   - Heterogeneous professions reflecting wages (no all-Farmer collapse).
   - Faction treasury draining as chief pays workers; rebuilding from `household_skim_pct`.
   - Agents with high `tool_capital_factor` or `land_capital_factor` stay in profession through wage dips that flip a capital-light agent.
   - Survival override fires when food/head drops below 16; at least one new Farmer promoted.
   - At least one agent in `Profession::Apprentice` with a paired mentor.

7. **Subsistence regression:** Same run, Subsistence preset. Profession churn matches main — survival floor dominates, no treasury drain, `wage_signal` stays empty, no apprentices.

8. **Currency invariant across all phases:** existing `assert_total_currency_invariant` covers `EconomicAgent.currency + FactionData.treasury + Settlement.treasury + JobEscrow.amount`. Run end-to-end across a 5-game-day Market simulation, assert invariant holds within `eps = 0.5`.

9. **Determinism:** seeded runs produce identical profession trajectories per agent across replays (Disposition ε-greedy is itself seeded; without Disposition the EV path is deterministic).

## Out of scope (explicit deferrals)

- **Tool durability/wear.** Static affinity in this plan; durability touches every gather/craft executor.
- **Cross-household workshop rental contracts.** Mechanically supported by existing job postings; no auto-posting system here.
- **Biological aging × skill.** Independent dimension (elder agents losing dexterity).
- **Inter-generational profession inheritance.** Children gain childhood XP from parents' professions — follow-up plan, hooks `pregnancy_system` + childhood maturation.
- **Faction-level capital investment.** Chief decides which workshop to build next via `chief_directive_system`; wage signal could feed it later but isn't wired here.
- **Phase 6 (UI + EarnIncomeScorer) only ships if peer plan's Phase 4-5 (GoalScorer / Disposition) lands.** Otherwise, fold Phase 6 changes into the existing `goal_update_system` cascade as a procedural branch and defer the UI polish.
- **Migration triggered by wage gradient.** Cross-faction `PerceivedFactionWages` is captured but no agent acts on it yet — natural follow-up once wages are stable across factions.
