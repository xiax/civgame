# Era-Specific Construction Fallbacks and Procurement

## Status: SHIPPED (2026-05-17)

All 9 steps implemented; `cargo test --bin civgame` = 734 pass, 1 fail
(`bronze_start_scales_beds_to_larger_player_population` — pre-existing
load-sensitive flake on the `members==60` spawn assertion, passes isolated,
orthogonal to this work).

- Selector/classifier unit tests (`classify_*`, `select_*`) + Step-4
  `market_escrow_purchase_pool_is_invariant_safe`.
- E2E: `neolithic_emergency_outskirts_bed_when_no_materials`,
  `market_procurement_preserves_currency_invariant` (invariant sampled mid
  buy→return), `market_haul_requires_state_public_works`,
  `subsistence_faction_no_market_haul`.

Deviations from plan (all sound, lower-risk):
- Market-haul worker dispatch uses the existing in-hand-fast-path **direct
  dispatch** (no registry method) → avoided 55-site `PlannerCtx` churn.
  Market node+tile resolved at classify time onto
  `FactionData.procurement_market` (dodges the 16-param dispatcher ceiling);
  `ClaimTarget.haul_source` carries the price ceiling (kept off `Task` so
  `Task: Eq` holds).
- `JobEscrow` gained `purchase_pool` (not a wage/pool struct split); hook +
  `total_escrowed_currency` use `held()` → existing payout-despawn path
  already reconciles Market capital (Step 6 became verify-only).
- Step 7 needed a **cold-start fix not in the plan**: defer post-PERM
  shelter one classifier window when `material_view` is empty (both
  `pressure_to_intent` and `generate_candidates`) — otherwise doomed
  higher-tier huts spawn before scarcity is known, stall, and fill the
  per-faction concurrency cap so the emergency Bed can never be selected.
- `find_emergency_bed_tile` is one era-parameterised finder (annulus = the
  era distinction), not three near-duplicates.
- `neolithic_runtime_no_paleo_beds_or_excess_hearths` left unchanged: its
  seeded fixture has no runtime deficit so no emergency fires there; the
  emergency path is covered by the dedicated new test.
- Root `CLAUDE.md` had no currency-invariant line → only
  `src/simulation/CLAUDE.md` updated.

## Context

When a construction blueprint's wall/door material can't be obtained, `best_wall_material()`
(`construction.rs:996`, purely tech-gated, ignores stock) still picks the top tech tier and
construction **stalls indefinitely** — a Haul posting is emitted but never satisfied because
storage is empty and the gather pipeline can't find the resource. There is **no** existing
"older-era fallback": the Paleo crescent-bed branch is already hard era-gated
(`construction.rs:3424`, `matches!(era, Paleolithic|Mesolithic)`), so Neolithic+ never
degrades to Paleo beds — it just goes shelter-less. This plan **adds** a scarcity-aware
response layer where today there is a silent stall.

Outcome: scarce-but-buyable materials get procured via the market (keeping era-appropriate
buildings), the material ladder is walked down only when procurement is impossible, and
era-appropriate emergency shelter is emitted instead of leaving a band unsheltered forever.

Confirmed decisions: (1) all-era emergency-shelter geometry in V1; (2) procurement buys at
the faction's own market node, funded via the existing chief escrow; (3) procure-primary-
first (keep the top tech material and buy it; substitute down only when procurement fails).

## Corrections folded in from review

- Seed mode stamps materials for free (`seed_apply_intent`, no Blueprint/worker/cost) — the
  scarcity logic must **not** engage in seed mode or the wall-upgrade pass. Wrap, don't
  replace, `best_wall_material`; those two sites pass an "unconstrained" availability.
- `SharedKnowledge` clusters store **no quantity** (`estimated_count` is 0..4 rep-slot
  occupancy). `known_reserve` cannot be quantitative — reuse existing
  `chief_job_posting_system::faction_knows_cluster` as a coarse "raw-gatherable at all"
  boolean. Concrete signal is `faction.storage.stock_of` (deposited only — never count
  inventories in posting/candidate scoring).
- Doc target is `CLAUDE.md` (root + `src/simulation/CLAUDE.md`), **not** AGENTS.md.
- `trader_buy_at_settlement` fails for `Camp` (not a `Settlement` component) — nomadic
  factions need a `trader_buy_at_node` generalization or they can't procure.
- The currency invariant is the highest risk; the worker must never carry job purchase-capital
  across a tick boundary.

## Approach

### 1. Types

In `construction.rs` (next to `best_wall_material:996`):

```rust
pub struct ResourceAvailability { stored: u32, inventory: u32, market_stock: f32,
    market_price: f32, affordable_qty: u32, raw_gatherable: bool, scarcity: Scarcity }
pub enum Scarcity { Available, Tight, Scarce, Unavailable }
pub enum WallSelection { Material { mat: WallMaterial, source: HaulSource }, EmergencyShelter }
```

Classification (`need` = recipe input qty for one structure):
- `Available`: `stored >= need`.
- `Tight`: `stored < need` but `raw_gatherable` — existing gather path resolves it, no change.
- `Scarce`: not stored, not gatherable, but `affordable_qty >= need` at the node → **procure**.
- `Unavailable`: none of the above → substitute down ladder, then era-fallback.

In `jobs.rs` (next to `JobProgress`): `enum HaulSource { Storage, Market { max_unit_price: f32 } }`.

### 2. Era-aware selector (wrap `best_wall_material`, route all 3 call sites)

`select_wall_material(techs, avail: Option<&MaterialAvailabilityView>) -> WallSelection`:
- `None` → `Material { best_wall_material(techs), Storage }` verbatim (seed `from_era:1106`,
  wall-upgrade `:4983` pass `None` — the carve-out).
- `Some` → procure-primary-first walk from the tech-top rung
  (`Palisade<WattleDaub<Mudbrick<Stone<CutStone`):
  1. top rung inputs `Available`/`Tight` → keep, `Storage`.
  2. any input `Scarce` → **keep top rung**, `Market { max_unit_price = node.price_of(rid) }`.
  3. any input `Unavailable` → step down one rung, re-evaluate from (1).
  4. all rungs `Unavailable` → `EmergencyShelter`.

Runtime `generate_candidates:3266` passes `Some(view)` when not seed-mode (line 3264
`seed_techs.is_some()`), else `None`.

### 3. Classification at chief cadence (not per-tick)

New `classify_construction_materials(faction, node, knows_cluster_fn) -> MaterialAvailabilityView`,
computed once per chief tick, stored on `FactionData.procurement_plan: AHashMap<ResourceId,
HaulSource>`. Reuses `faction.storage.stock_of`, `faction.supply_of` (informational
`inventory` only), `chief_job_posting_system::faction_knows_cluster`, `faction_market_node` +
`SettlementMarket::price_of/stock_of` (global `Market` fallback). `affordable_qty =
min(floor(treasury_budget/price), market_stock)`.

Phase 3c Haul posting (`jobs.rs:2443-2505`) reads `procurement_plan` and stamps
`HaulSource::Market` on a slot **only when**: existing `policy_for(rid).chief_allocates_labor`
gate passes AND blueprint `poster_class` ∈ {`Chief`,`Architect`} AND
`FactionData.state_funds_public_works` (consistent with the Build-posting carve-out
`jobs.rs:2116`). Household/Individual blueprints stay `Storage`-only — preserves Market-mode
free-agent labor (`reward>0` U_bid path untouched).

### 4. Procurement funding (currency-invariant — critical)

Invariant: `EconomicAgent.currency + FactionData.treasury + Settlement.treasury +
JobEscrow.amount` conserved.

- Split `JobEscrow` into `wage: f32` + `purchase_pool: f32` (or add `purchase_pool`); update
  `total_escrowed_currency` (`jobs.rs:463`) + `assert_total_currency_invariant` snapshot to
  sum both.
- `chief_wage_for` (`jobs.rs:727`) for `Haul { source: Market{max_unit_price}, target, .. }`
  → `wage` (unchanged transport wage) + `purchase_pool = max_unit_price * target`.
  `chief_post_funding_system` (`jobs.rs:784`) debits treasury for the sum. Add a
  `treasury_remaining` running tally (mirror `storage_remaining` `jobs.rs:2426`) so concurrent
  Market hauls don't over-commit one treasury. Subsistence skip is inherited (`jobs.rs:797`).
- New `Task::BuyMaterialAtMarket { resource_id, qty, node, max_unit_price }` (separate from
  `WithdrawMaterial` — market buy is exclusive `&mut World`, storage withdraw is `Query`).
  Executor `buy_material_task_system` (exclusive). **In one atomic invocation**: advance
  `min(max_unit_price*qty, purchase_pool)` from escrow → worker currency; call
  `trader_buy_at_node` (worker currency → node treasury, stock↓, goods→worker); immediately
  return `advance - actual_spent` to escrow `purchase_pool`. Worker carries **only goods**
  across the tick boundary — so all 25 existing `aq.cancel()` sites stay correct unchanged.
  Add `debug_assert!` no worker holds job purchase-capital at tick boundary.
  If `node.price_of(rid) > max_unit_price` at execution → treat as failure (cancel chain,
  record failure, refund).
- `trader_buy_at_node(world, worker, MarketNodeRef, rid, qty)` thin dispatch over
  Settlement (`trader_buy_at_settlement:transactions.rs:23`) vs Camp (mirror, camp.rs has
  symmetric `camp_price_update_system`).
- Deposit leg is the **shared** `HaulToBlueprint` path (`construction_system:5271-5344`,
  source-agnostic). Factor the `finish_withdraw_material` HaulToBlueprint routing tail
  (`production.rs:650-681`) into `route_haul_to_blueprint_tail(...)`; both finishers reuse it.
- `job_payout_system` (`jobs.rs:519`) completion: pay `wage` across claimants, then refund
  residual `purchase_pool` to beneficiary (chief→treasury, matching `on_job_escrow_remove`),
  despawn with fields zeroed so the `on_remove` hook (`jobs.rs:437`) no-ops. Failure path:
  unchanged hook refunds full `wage+purchase_pool` (worker holds no capital by construction).
- HTN: `BuyAndHaulToBlueprintMethod` sibling of `WithdrawAndHaulToBlueprintMethod`
  (`htn.rs:1743`), expands `[BuyMaterialAtMarket, HaulToBlueprint]`. `PlannerCtx` (`htn.rs:894`)
  + `ClaimTarget` gain `haul_source: Option<HaulSource>` + `market_node: Option<Entity>`.
  Wire `Task::BuyMaterialAtMarket` into `typed_task.rs` variant list, `task_kind_for`,
  `aq` defence-in-depth fallback, and a `(Haul, BuyMaterialAtMarket)` stale-reset preserve-arm
  (chain is `MF_UNINTERRUPTIBLE`).

### 5. Era fallback geometry (all eras, V1)

New finders in `construction.rs` near `find_clear_tile_in_zone:1922` /
`find_unfilled_civic_zone_tile:1997` / `is_clear_footprint:2212` / `find_bed_tile_around_hearth:2081`.
All reuse those primitives + consult `DoormatReservations`/`BedMap`/`BlueprintMap`/passability,
reject `TileKind::Road`. Determinism via `fastrand::Rng::with_seed(SettlementBrain.layout_hash)`
(organic_settlement.rs:189; pattern at construction.rs:3470).

- `find_outskirts_bed_tile(...)` — Neolithic: spiral beyond the residential ring.
- `find_workyard_bunk_tile(...)` — Chalcolithic: bunk rows near Crafting/Storage zones.
- `find_civic_overflow_bed_tile(...)` — Bronze: bed rows packed against the Civic zone.

Hook at `generate_candidates:3423` residential branch. Emit an emergency-flagged bare
`BuildSiteKind::Bed` candidate (low score `100.0 + bed_deficit*20.0`, below normal residential
so it yields once any real material arrives) iff:

> `bed_deficit > 0.0 && !matches!(era, Paleolithic|Mesolithic) &&
>  select_wall_material(techs, Some(view)) == EmergencyShelter`

Mark the `Blueprint` with an `emergency: bool` flag (cheap, distinguishes from forbidden
Paleo-crescent beds for the regression). Non-shelter (defense/civic/craft) `Unavailable` +
not procurable → **defer with reason** (no candidate, log reason); no previous-era substitute.

## Critical files

- `src/simulation/construction.rs` — selector + classification + era finders;
  `best_wall_material:996`, `generate_candidates:3266/3423`, finders `:1922/:1997/:2081/:2212`,
  recipes `:795`, `recipe_for:1528`, deposit `:5271`.
- `src/simulation/jobs.rs` — `HaulSource`, `JobProgress::Haul:117`, `chief_wage_for:727`,
  `chief_post_funding_system:784`, `job_payout_system:519`, `on_job_escrow_remove:437`,
  Phase 3c `:2443`, `JobEscrow:432`, `faction_knows_cluster`.
- `src/economy/transactions.rs` — `trader_buy_at_settlement:23`, `pay:163`; new
  `trader_buy_at_node` (Camp generalization).
- `src/simulation/production.rs` — `withdraw_material_task_system:392`,
  `finish_withdraw_material:632` (factor shared tail); new `buy_material_task_system`.
- `src/simulation/htn.rs` — `PlannerCtx:894`, `WithdrawAndHaulToBlueprintMethod:1743`.
- `src/simulation/faction.rs` — `FactionData.procurement_plan`, `supply_of`, `storage.stock_of`.
- `src/simulation/test_fixture.rs` — `neolithic_runtime_no_paleo_beds_or_excess_hearths:13899`,
  `assert_total_currency_invariant`, `fixture_with_flat_world`, `configure_start`,
  `trigger_onenter`.
- Docs: `src/simulation/CLAUDE.md` Construction section; root `CLAUDE.md` currency-invariant
  line (escrow split is cross-cutting).

## Sequencing

1. **Types** — `ResourceAvailability`/`Scarcity`/`HaulSource`/`WallSelection`; `HaulSource`
   field on `JobProgress::Haul` (verify all matches use `..`: jobs.rs
   159/174/208/2431/2463/2482/3448/3551/3692/3863), `ClaimTarget`, `PlannerCtx`. Default
   `Storage` ⇒ no behavior change. Gate: `cargo build` + existing tests green.
2. **Selector + classification** — route 3 call sites; seed/upgrade pass `None`. Gate: unit
   tests + `onenter_era_seeding` green (proves carve-out).
3. **`procurement_plan` + Phase 3c stamping** (policy + poster_class + state-funds gated).
   Inert without workers. Gate: posting carries `Market` under right conditions.
4. **Funding** — `JobEscrow` split, `chief_wage_for`/`chief_post_funding_system` +
   `treasury_remaining`, invariant snapshot. **Highest risk.** Gate: fund-only invariant unit
   test.
5. **Worker chain** — `Task::BuyMaterialAtMarket`, exclusive `buy_material_task_system`
   (atomic advance→buy→return), `BuyAndHaulToBlueprintMethod`, `route_haul_to_blueprint_tail`,
   `trader_buy_at_node`, typed-task wiring. Gate: integration test below.
6. **Reconciliation** in `job_payout_system` (completion + failure). Gate: invariant variants.
7. **Era geometry** — finders + emit at `:3423` under §5 gate + `emergency` flag. Depends
   only on step 2; landable in parallel.
8. **Update regression** + policy tests (last, needs 2+7).
9. **Docs**.

Steps 1–2, 7 low-risk/independent. Steps 4–6 are the currency-critical core; land together
gated by the invariant integration test.

## Verification

- `cargo test --bin civgame` (binary crate — `cargo test` alone errors).
- Unit: `classify_*` (incl. `classify_not_available_when_stock0_supply_positive` —
  deposited-only rule), `select_*` (incl. `select_unconstrained_none_equals_best_wall_material`).
- Integration `market_procurement_preserves_currency_invariant` (`test_fixture.rs`): Mixed
  faction, Neolithic, `fixture_with_flat_world` (no gatherable wood/stone), pre-stock the
  faction Settlement market with stone + treasury; snapshot `assert_total_currency_invariant`
  before; ~2000 ticks; assert invariant at every 200-tick sample **including mid-chain (after
  buy, before deposit)** and a walled house finalized with the procured material. Failure
  variant: stock-out market post-posting → escrow fully refunds to treasury, invariant green.
- `neolithic_emergency_outskirts_bed_when_no_materials` + `chalcolithic_workyard_bunks_*` +
  `bronze_civic_overflow_*`: flat world, no treasury/market/gatherable → emergency-flagged bed
  at outskirts after 1600 ticks.
- Update `neolithic_runtime_no_paleo_beds_or_excess_hearths:13899`: keep "no Paleo-crescent
  bed near home"; assert the only permitted bed is the `emergency`-flagged outskirts bed at
  outskirts radius (the fixture has no treasury/market so it now legitimately emits one).
- Policy: `subsistence_faction_no_market_haul`, `household_blueprint_no_treasury_procurement`.
- Manual smoke: `cargo run` (never `--sandbox`), Mixed/Neolithic start on a resource-poor map;
  confirm market-procured walled houses build rather than stalling, and a starved band emits
  emergency outskirts beds.
