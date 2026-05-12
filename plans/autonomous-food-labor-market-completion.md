# Autonomous Food Labor Market — Completing Track B

## Context

Two prior workstreams set up the architecture this plan completes:

- **Pluralist Economy R0–R13** (shipped): `JobPosting.poster_class { Chief | Bureaucrat | HouseholdHead | Individual }`, `reward: f32`, `JobEscrow`, `U_bid` scoring, per-resource `ResourceControlPolicy { chief_allocates_labor, private_actors_allowed }`, `Subsistence` / `Mixed` / `Market` presets.
- **HTN Robustness Track B** (`plans/htn_robustness_posting_market.md`, partially shipped): `worker_self_post_stockpile_system` self-funds `Stockpile{Wood|Stone}` postings in Mixed/Market when chief is policy-gated off; `htn_acquire_good_dispatch_system` dispatches autonomous `GatherWood`/`GatherStone` without requiring a claim.

The current bug — workers stuck Idle under autonomous `GatherFood` — exists because Calories was not folded into either of the above paths. The dispatch path for food (`htn_stockpile_food_dispatch_system`) hard-requires a `JobClaim::Stockpile{food}`; the self-posting system explicitly iterates only `[wood_id, stone_id]` (`jobs.rs:818`). In Market preset this strands every worker in a low-food faction: chief skips Calories postings (`policy_for(Fruit).chief_allocates_labor = false`), no worker posts one either, dispatcher returns silently, `goal_update_system`'s 200-tick cadence re-selects `GatherFood` next cycle, and `chronic_failure_release_system` can't bite because no `MethodHistory` failure ever stamps.

The simplest patch (drop the claim gate) was tried and reverted: it fires terminal-`Explore { AnyEdible }` during *every* test fixture's warm-up `tick_n` (any faction below seasonal food cap triggers `prioritize_food` → autonomous `GatherFood`), breaking 17 baseline tests that pin a goal after warm-up and expect the agent Idle. The breakage is symptomatic of a deeper asymmetry: GatherWood/GatherStone also have this property in production, but Wood/Stone tests happen to be written around `JobClaim`-pinned setups while food tests rely on Idle warmup.

## Goal

Land symmetric autonomous-labor behavior for Wood / Stone / **Food** across all three economy presets, without breaking the test suite. After this plan:

- **Subsistence:** chief posts Calories / Wood / Stone (no behavior change); autonomous fallback remains dormant because chief postings consume the workforce.
- **Mixed:** chief posts staples; workers self-post non-staples (no behavior change).
- **Market:** chief abstains; workers self-post Calories *and* Wood/Stone, claim their own contracts, dispatch via the claim path (no orphan autonomous path needed).

The asymmetry between food and materials disappears.

## Design

### Recommended approach: route Market-mode food work through self-posting, not autonomous dispatch

Rather than relax `htn_stockpile_food_dispatch_system`'s claim gate (which would re-introduce the 17 test failures), make the food work route entirely through claims by extending `worker_self_post_stockpile_system` to cover Calories. The autonomous `GatherFood` goal then becomes the *trigger* for self-posting, not a separate dispatch path.

This is exactly the shape `worker_self_post_stockpile_system` already implements for Wood/Stone. Calories is the missing branch.

### Phase A — Calorie self-posting

`worker_self_post_stockpile_system` (`jobs.rs:774–917`) gains a third loop iteration for `Calories`.

Two design questions specific to Calories:

1. **Resource selection.** Wood/Stone are concrete `ResourceId`s; Calories is a category. The chief Calories branch uses `JobProgress::Calories { deposited, target }` (aggregate calorie counter) with `ClaimKind::AnyEdible`. **Use the same shape for the self-post**: post one `JobKind::Stockpile` with `JobProgress::Calories { target }` and a `ClaimTarget { kind: AnyEdible }`. Dispatch already handles `AnyEdible` correctly (`htn_stockpile_food_dispatch_system::scavenge` resolves the good per-pickup).
2. **Wage formula.** `self_post_wage(catalog, rid, qty)` requires a concrete `rid`. For Calories: pick a representative staple (Fruit) — cheap, always available in the catalog — and price as `trade_base_value(Fruit) * (target_calories / Fruit.calories_per_unit) * SELF_POST_MARGIN`. Optionally hoist this into a `food_self_post_wage(catalog, target_calories)` helper.

Gates mirror Wood/Stone:
- `policy_for(Fruit).chief_allocates_labor == false` (skip in Subsistence).
- No live Calories posting in the faction.
- Faction-tier `MemoryKind::AnyEdible` cluster within 16 tiles of `home_tile` (uses existing `faction_knows_cluster`).
- Author's currency ≥ `WORKER_SELF_POST_MIN_CURRENCY (20.0)`.
- Real deficit: `food_total < target_supply`.

Target qty: `WORKER_SELF_POST_FOOD_CALORIES = 240` (≈ 2 fruits per faction member × 4 members, scaled by `member_count` like the chief branch).

### Phase B — Bootstrap path for Market factions with no wealthy worker

Even after Phase A, Market factions in the very early game can deadlock: every worker has < 20 currency, so nobody self-posts, so nobody earns income. Two cheap escapes already exist; pick the lighter one:

- **Option B1 (recommended):** seed each Market-preset adult with `MARKET_BOOTSTRAP_CURRENCY = 30.0` at spawn (one slot above `WORKER_SELF_POST_MIN_CURRENCY`). `seed_market_households` (`person.rs`) already runs only for Market preset and already inserts `HOUSEHOLD_SEED_TREASURY = 15.0` — extend it to set per-adult `EconomicAgent.currency` floor at spawn.
- **Option B2:** add an "emergency" branch where, if no member can afford the wage *and* food_total has been below threshold for one game-day, the faction treasury covers the post. Heavier; defer unless B1 proves insufficient.

This preserves the currency invariant (the bootstrap currency comes from `MARKET_BOOTSTRAP_CURRENCY` being added to the system on spawn, recorded by extending `CurrencySnapshot::capture` and `total_system_currency`'s baseline; the helper `assert_total_currency_invariant` already takes a baseline parameter).

### Phase C — Drop the autonomous-food dispatch path entirely

Once Phase A guarantees a Calories posting exists in any Market faction with food deficit + cluster knowledge + minimum currency, `htn_stockpile_food_dispatch_system`'s claim gate becomes load-bearing in the right way: claimless `GatherFood` is genuinely a transient state, and returning early is correct.

**Do not relax the claim gate.** Keep the comment at `htn.rs:4981–4990` accurate.

### Phase D — Validation tests

Three new behavioural tests covering the regimes:

1. `worker_self_post_stockpile_system::tests::market_faction_self_posts_calories_when_chief_abstains` — spawn a Market faction with food deficit + visible berry cluster + adult with 30 currency, tick a day, assert a `JobPosting { kind: Stockpile, progress: Calories { .. }, poster_class: Individual }` materialised and was claimed.
2. `worker_self_post_stockpile_system::tests::subsistence_faction_does_not_self_post_calories` — Subsistence preset, no worker self-posts even with deficit (chief handles it).
3. `htn::tests::autonomous_gather_food_without_claim_returns_early` — explicit assertion of the *unchanged* dispatcher behavior (regression guard against accidental future relaxation).

### Phase E (deferred — M4/M5) — Buy-orders + `EarnIncome` goal + `market_sell_system` gating

Out of scope for this plan; the Phase A path lands Calories self-posting using the same chief-style posting (work-order, not buy-order). The fuller M4/M5 vision (chief posts buy-orders; workers self-post their labor and sell at stalls) lives in `plans/need-a-good-plan-noble-cascade.md` and can build on top.

## Why this design

- **Symmetric with Wood/Stone end state.** After Phase A, every autonomous-gathering branch in `goal_update_system` has a matching self-posting branch in `worker_self_post_stockpile_system`. The "autonomous dispatch without claim" path becomes vestigial — kept for crisis-mode `Survive` (which has its own dispatcher and shouldn't go through the market) but no longer load-bearing for productive labor.
- **No test fallout.** `htn_stockpile_food_dispatch_system` keeps the claim gate; warm-up `tick_n` calls in fixtures continue to leave agents Idle. The new Calories self-posting only fires for factions with `chief_allocates_labor=false` for Fruit (i.e., Market preset) and minimum currency — most test fixtures use default `FactionData` (Subsistence), which is unaffected.
- **Bootstrap-safe.** Phase B's spawn-time currency seed costs zero scheduling complexity and is naturally scoped to the Market preset.
- **Incremental on shipped infrastructure.** `JobProgress::Calories`, `ClaimKind::AnyEdible`, `self_post_wage`, `JobEscrow`, `htn_stockpile_food_dispatch_system` already understand the Calories case — we only wire the missing branch.
- **Respects "Support diverse economic models" memory** — Subsistence is bit-for-bit identical; Mixed gains Calories self-posting only for non-staple food types; Market gets the full pluralist behavior.

## Critical files

- `src/simulation/jobs.rs:774–917` — `worker_self_post_stockpile_system`, add Calories branch.
- `src/simulation/jobs.rs:2113–2128` — `self_post_wage`; consider `food_self_post_wage` helper.
- `src/simulation/jobs.rs:1346–1411` — `chief_job_posting_system` Calories branch (reference for posting shape).
- `src/simulation/person.rs::seed_market_households` — Phase B currency floor.
- `src/simulation/test_fixture.rs::CurrencySnapshot` — extend baseline accounting for bootstrap currency.
- `src/simulation/htn.rs:4981–5004` — leave alone; preserve claim gate + add a doc-comment line crediting Phase A for closing the upstream gap.
- `src/simulation/CLAUDE.md` `Worker self-post system` paragraph — extend to mention Calories branch.
- Top-level `CLAUDE.md` is unaffected (the change is sub-system local).

## Verification

- `cargo check && cargo test --bin civgame` — 480-test suite must remain green.
- `cargo run` with default (Subsistence) preset for 5 in-game days — agents work, inspector shows chief-authored postings, no `Individual` Calories postings appear. Behaviour unchanged.
- `cargo run` with Market preset (when wired into the spawn-select UI; otherwise force via test or temporary `GameStartOptions` default) for 5 in-game days — agents work, inspector shows `Individual`-authored Calories postings claimed by self-author. Food stock stays above zero, no idle workers under autonomous `GatherFood`.
- Visual: inspector hover on a Market-preset adult should show a `JobClaim::Stockpile` with `JobEscrow` debited from their wallet.
