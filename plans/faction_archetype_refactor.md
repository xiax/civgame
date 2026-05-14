# Faction Archetype Refactor — Unifying Lifestyle × Economy

## Context

The codebase models civilization variation along two axes that are currently encoded as enum dispatch with early-return branches at every consumer:

- **Lifestyle** (`Settled` / `Nomadic`) — ~22 `is_nomadic()` sites; mostly early-returns that skip settled-only machinery (`auto_found_default_settlements_system`, `chief_job_posting_system`, `chief_directive_system`, `carve_plots_system`, `WithdrawMaterialFromStorageMethod`, etc.).
- **Economy** (`Subsistence` / `Mixed` / `Market`) — `apply_preset` + `land_policy_for` stamp flags, but downstream systems still match on the preset enum directly.

The two axes interact incoherently. Concrete clashes from the audit:

| Problem | Symptom |
|---|---|
| Storage dual-pass | Settled reads `FactionStorageTile`; nomads pool member inventories. HTN methods only know about tiles. |
| `home_tile` semantics | Settled treats it as immutable anchor; nomads mutate on migration. Every system silently assumes "stable home_tile". |
| `Lifestyle × Preset` orthogonality holes | Nomadic + Market gives capitalist `economic_policy` and `state_sells_land=true` flags, but no plots and no postings exist — flags inert. |
| Sedentarization carryover | A nomadic faction sedentarizing keeps its old (default-Subsistence) `LandPolicy`; the new settlement never lists plots. |
| Income skim leak | `split_market_earnings_with_household` skims 10% unconditionally; subsistence households accumulate treasury and start posting craft jobs against design intent. |
| Evicted-plot orphans | When `rent_collection_system` evicts a plot, structures stay in place ownerless. |
| Asymmetric sub-faction inheritance | `spawn_household` has hardcoded `if parent_is_capitalist` for `economic_policy`; lifestyle/land/treasury inherit by different rules. |
| Hardcoded preset extension cost | Adding a 4th model (Feudal, Caravan, Tribal Gift) requires touching `game_state.rs`, `policy.rs`, `person.rs`, `jobs.rs`, `construction.rs`, `nomad.rs`. |

**Goal**: a data-driven **FactionArchetype** model where every site that currently branches on `Lifestyle` or `EconomyPreset` instead consults a per-faction `FactionCapabilities` struct. Archetypes load from RON. Adding Feudal or Caravan-Hybrid becomes a content edit, not a code edit. Critically, **Market-mode free-agent labor remains first-class** — the design must not promote chief-as-allocator to a hidden default.

## Design

### Pillar 1 — `FactionCapabilities` replaces enum dispatch

A capability bundle stored on `FactionData`, computed once at faction creation from its archetype, and re-applied on lifecycle transitions (sedentarize, conquest, archetype swap):

```rust
// new: src/simulation/archetype.rs
pub struct FactionCapabilities {
    pub home: HomeMobility,                // Anchored | Mobile { migration_period_min_days }
    pub storage: StorageBackend,           // FactionTile | MemberPool | Hybrid | CaravanBundles
    pub shelter: ShelterMode,              // Permanent | Portable | Mixed
    pub settlement: SettlementMode,        // FullSettlement | Camp | None
    pub posting: PostingMode,              // ChiefAllocates | FreeAgent | Hybrid | Disabled
    pub land: LandModalities,              // existing LandPolicy + plot-carving toggle
    pub economic_policy: AHashMap<ResourceId, ResourceControlPolicy>,
    pub income: IncomeFlow,                // skim pct, who collects
    pub inheritance: InheritanceSpec,      // how sub-factions derive their archetype
    pub archetype_key: String,             // for inspector / save / re-apply
}
```

Critical: `PostingMode` is the *coordination* layer, not chief-vs-nomad. It admits four states so Market mode doesn't lose free-agent semantics:

- `ChiefAllocates` — current settled-Subsistence/Mixed default (chief posts food/wood/build/etc.)
- `FreeAgent` — current Market-mode households post their own contracts
- `Hybrid` — Mixed: chief posts staples, households post crafts
- `Disabled` — no posting layer (current nomads). Members satisfy needs autonomously.

Every `is_nomadic()` / `match preset` site collapses to a capability check:

```rust
// before (jobs.rs:908)
if faction.lifestyle.is_nomadic() { continue; }
// after
if !faction.caps.posting.posts_chief_jobs() { continue; }
```

### Pillar 2 — RON archetype registry

`assets/data/factions/archetypes/*.ron` defines named bundles. Loaded at `WorldPlugin::build` into a `FactionArchetypeRegistry` resource (mirrors `ResourceCatalog`). Game-start UI picks an archetype by name, not by `(Lifestyle, EconomyPreset)` cross-product.

```ron
// assets/data/factions/archetypes/settled_subsistence.ron
FactionArchetype(
    key: "settled_subsistence",
    home: Anchored,
    storage: FactionTile,
    shelter: Permanent,
    settlement: FullSettlement(carves_plots: false),
    posting: ChiefAllocates,
    land: ( /* defaults: state-owned, no transfers */ ),
    economic_policy_preset: AllCommunist,
    income: ( household_skim_pct: 0.0 ),
    inheritance: ( child_archetype: "household_subsistence", seed_treasury: 0.0 ),
)
```

Three RON files reproduce the current 6-cell `Lifestyle × Preset` matrix that's actually exercised: `settled_subsistence`, `settled_mixed`, `settled_market`, `nomadic_subsistence`. (Other cross-products were inert/buggy and are simply not authored.) Two new RON files unlock the requested future models:

- `caravan_hybrid.ron` — `home: Anchored`, but `storage: Hybrid(home_tile + caravan_bundles)`, plus a `child_archetype: "caravan_crew"` whose own archetype is `home: Mobile, storage: MemberPool, settlement: None`. Crew sub-factions cycle out from home, return, transfer storage on arrival via lifecycle event.
- `feudal_lord.ron` — `inheritance.child_archetype: "vassal"`; vassal's child is `"feudal_household"`. Rent flows along the parent chain via existing `root_faction()` walking; `Plot.holder` extended with `Vassal { faction_id }`. Tribute pipeline already exists (`tribute_payment_system`) — feudal config just routes household rent up the chain instead of into the immediate landlord's treasury.

### Pillar 3 — `StorageBackend` trait kills the dual-pass

The largest mechanical win. Today `compute_faction_storage_system` runs a settled-tile pass *plus* a nomadic-pool pass; HTN's `WithdrawMaterialFromStorageMethod` and deposit dispatchers only understand tile storage.

```rust
// new: src/economy/storage_backend.rs
pub trait StorageBackend {
    fn rollup(&self, world: &World, faction: Entity) -> AHashMap<ResourceId, u32>;
    fn nearest_deposit(&self, world: &World, faction: Entity, near: (i32,i32)) -> Option<DepositTarget>;
    fn nearest_withdraw(&self, world: &World, faction: Entity, rid: ResourceId, near: (i32,i32)) -> Option<WithdrawSource>;
}
```

Concrete impls: `FactionTileBackend`, `MemberPoolBackend`, `HybridBackend { tile, members }`, `CaravanBundleBackend`. `FactionData.caps.storage` carries the variant; `compute_faction_storage_system` dispatches once. HTN methods consult the backend instead of `StorageTileMap` directly. This makes nomads first-class storage citizens and unlocks caravan hybrid for free.

Withdraw / deposit `Target` types are enums, not entity IDs, so the HTN can route to "member X's inventory" or "ground tile (a,b)" or "pack bundle Y" without method-level branching.

### Pillar 4 — Settlement lifecycle as events, not early-returns

Replace ad-hoc lifestyle branches in `auto_found_default_settlements_system`, `seed_starting_buildings_system`, `nomad_migration_commit_system`, and `nomad_sedentarize_system` with a single event queue:

```rust
// new: src/simulation/lifecycle.rs
pub enum SettlementLifecycleEvent {
    Establish { faction, tile, archetype: String },
    Abandon { faction, tile, refund_drops: bool },
    Migrate { faction, from, to },
    SwitchArchetype { faction, new_key: String, at_tile: (i32,i32) },
}
```

`process_settlement_lifecycle_system` (Sequential, exclusive World) drains the queue and:
1. Re-applies the new archetype's capability bundle on `SwitchArchetype` (fixes sedentarize policy carryover — the new settled archetype's `LandPolicy` and `economic_policy` overwrite the stale nomadic ones).
2. Despawns or re-seeds structures via the right backend (camp seed for nomadic, plot carve + seed for settled).
3. Emits `ActivityEntryKind` so the UI log already wires through.

Existing systems become *emitters*: `nomad_migration_system` emits `Migrate`; `nomad_sedentarize_system` emits `SwitchArchetype`. They no longer mutate world state directly. This collapses three "lifestyle-aware" systems into one declarative pipeline.

### Pillar 5 — Inheritance spec, not hardcoded household branching

Today `spawn_household` (`faction.rs:1634-1687`) has implicit rules: lifestyle copies, `land_policy` copies, `economic_policy` does an `if parent_is_capitalist` branch. Replace with archetype-driven `InheritanceSpec`:

```rust
pub struct InheritanceSpec {
    pub child_archetype: Option<String>,  // sub-faction looks up its own archetype
    pub seed_treasury: f32,
    pub seed_storage_tile: bool,           // explicit, not "if Market preset"
}
```

`spawn_household` looks up `parent.caps.inheritance.child_archetype`, applies that archetype's full capability bundle to the child. Subsistence parents declare `child_archetype: "household_subsistence"` (treasury seed = 0, income skim = 0, no posting); Market parents declare `child_archetype: "household_capitalist"` (treasury seed = 15, posting = FreeAgent). Feudal parents can declare a `vassal` child archetype that itself has `child_archetype: "feudal_household"`, supporting the multi-tier chain.

### Pillar 6 — Income flow as a capability

`split_market_earnings_with_household` is currently unconditional. Move the skim percentage to `caps.income.household_skim_pct`; default 0.0; archetypes that explicitly authorize household income (Market, Mixed, Caravan) set 0.10. This fixes the subsistence income leak and makes Feudal-style upward income flow (skim to lord, not household) a one-line variant: `income.upward_skim_to_parent_pct`.

### Pillar 7 — Plot eviction cleanup

Eviction in `rent_collection_system` emits `PlotEvictedEvent { plot, structures }` instead of just flipping tenure. New `evicted_plot_cleanup_system` reads the event and, based on `caps.land.eviction_policy` (in `LandModalities`):
- `LeaveStructures` (current behavior, default for sandbox/test factions)
- `RevertToState` (mark structures `state_owned`, queue for reuse via existing chief flow)
- `Demolish` (despawn + drop refund stacks via the existing `Deployable::compute_refund_drop` path that nomadic teardown already uses).

## Phasing

| Phase | Scope | Status |
|---|---|---|
| **1. Capabilities + adapter** | `FactionCapabilities` + `derive_from_legacy`; every `is_nomadic()` / `match preset` site migrated. | **shipped** |
| **2. StorageBackend trait** | `economy/storage_backend.rs` with `WithdrawSource` / `DepositTarget`; `compute_faction_storage_system` backend-symmetric. | **shipped** |
| **3. Lifecycle events** | `SettlementLifecycleEvent` queue + `process_settlement_lifecycle_system`; nomad migrate / sedentarize / reverse-collapse emit through it. | **shipped** |
| **4. RON archetype registry** | `FactionArchetypeRegistry` loads `assets/data/factions/archetypes/*.ron` (`settled_subsistence` / `settled_mixed` / `settled_market` / `nomadic_subsistence`). | **shipped** |
| **5. Bug fixes** | `caps.income.household_skim_pct` plumbed; `evicted_plot_cleanup_system` consumes `PlotEvictedEvent` with `caps.land.eviction_policy`. | **shipped** |
| **6. Future-model proofs** | Author `caravan_hybrid.ron` and `feudal_lord.ron`; implement `CaravanBundleBackend` and `Holder::Vassal`. Caravan crew round-trip and feudal rent chain need new tests but no existing-system rewrites. | **deferred** |

## Critical files to modify

| File | Phase | Change |
|---|---|---|
| `src/simulation/archetype.rs` (new) | 1 | `FactionCapabilities`, `HomeMobility`, `StorageBackend` enum, `PostingMode`, `IncomeFlow`, `InheritanceSpec`. |
| `src/simulation/faction.rs:1454-1687` | 1, 5 | Add `caps: FactionCapabilities` field. Rewrite `spawn_household` to drive off `caps.inheritance`. Remove `lifestyle` field (or retain as label only). |
| `src/simulation/jobs.rs:908` | 1 | Replace `is_nomadic()` early-return with `caps.posting.posts_chief_jobs()`. |
| `src/simulation/construction.rs:1919, 4781` | 1, 3 | Replace lifestyle match with capability checks on `posting` and `shelter`. Camp/settlement seeding routes through lifecycle event. |
| `src/simulation/nomad.rs` | 3 | `nomad_migration_commit_system` and `nomad_sedentarize_system` become event emitters; structure cleanup moves to `process_settlement_lifecycle_system`. |
| `src/simulation/settlement.rs:122-146` | 3 | `auto_found_default_settlements_system` emits `Establish` event; doesn't mutate directly. |
| `src/simulation/person.rs:437, 608` | 1, 4 | `spawn_population` reads archetype from `GameStartOptions.archetype_key`, applies its bundle. Drops the `if Market` branch for household seeding (archetype's `InheritanceSpec` carries it). |
| `src/economy/policy.rs:150-207` | 4 | Keep `LandPolicy`, `ResourceControlPolicy` types. `apply_preset` and `land_policy_for` retire — archetypes carry the policy directly. |
| `src/economy/storage_backend.rs` (new) | 2 | Trait + 3 (later 4) impls. |
| `src/simulation/htn.rs:1126, 1386` | 2 | `SleepMethod` and `WithdrawMaterialFromStorageMethod` consult backend, not raw maps. |
| `src/economy/transactions.rs:212` | 5 | `split_market_earnings_with_household` reads `caps.income.household_skim_pct`. |
| `src/simulation/land.rs:rent_collection_system` | 5 | Emit `PlotEvictedEvent` on eviction. |
| `src/simulation/lifecycle.rs` (new) | 3 | Event enum + `process_settlement_lifecycle_system` exclusive-World processor. |
| `src/simulation/archetype_registry.rs` (new) | 4 | RON loader + registry resource (mirrors `ResourceCatalog` pattern at `world/spatial.rs` etc.). |
| `assets/data/factions/archetypes/*.ron` (new) | 4, 6 | 4 files for current models; +2 for future caravan/feudal. |
| `src/game_state.rs:25-32` | 4 | `EconomyPreset` retired or kept as a dead label; `GameStartOptions.archetype_key: String`. |
| `src/ui/spawn_select.rs` | 4 | Reads registry; renders archetypes by `display_name` and `description` field from RON. |

## Reusing existing infrastructure

- **`ResourceCatalog` RON-load pattern** (`economy/resource_catalog.rs`) is the template for `FactionArchetypeRegistry`. Same `*.ron` directory scan, same `OnceLock` install at `WorldPlugin::build`, same deterministic-id approach.
- **`root_faction()` walking** (`faction.rs:1692-1700`) already supports multi-tier chains; feudal rent flow uses it unchanged.
- **`Deployable::compute_refund_drop`** (`pack_deploy.rs`) already implements teardown-with-refund; the new `Demolish` eviction policy reuses it directly instead of re-implementing.
- **`tribute_payment_system`** (`faction.rs:585-620`) is the existing upward-currency-flow mechanism; feudal rent reuses it by routing household lease payment to vassal, then tribute carries it to lord.
- **`policy_gate` HTN system** (`htn.rs:532-547`) is preserved verbatim — capabilities feed `economic_policy` into the same gate. Free-agent labor in Market mode keeps working.
- **`ActivityEntryKind`** (`ui/activity_log.rs`) already handles `CampMoved` and `Sedentarized`; lifecycle events emit through the same channel.

## Verification

End-to-end checks at each phase:

1. **`cargo test --bin civgame`** must stay green throughout. The 411-test suite is the regression baseline. Phase 1 (pure adapter) should pass with zero test edits.
2. **Manual `cargo run`** with each archetype:
   - `settled_subsistence` (Paleolithic) — confirms chief posting, tile storage, plot carve.
   - `settled_market` (Bronze Age) — confirms household treasury, free-agent posting, land sale listings.
   - `nomadic_subsistence` (Mesolithic) — confirms migration trigger, member-pool storage, no plots, no postings.
   - `caravan_hybrid` — confirms crew sub-faction migrates while parent stays anchored; on return, storage transfers correctly.
   - `feudal_lord` — confirms household rent flows up to vassal then to lord via existing tribute path.
3. **Inspector checks** (`ui/debug_panel.rs`): faction panel surfaces `Archetype: <key>` plus capability summary. Hover panel surfaces capability-derived labels (e.g., "camp tile" vs "home tile") instead of branching on enum.
4. **Bug-fix regression tests** added in Phase 5:
   - Subsistence household with positive trade earnings: treasury stays 0.
   - Sedentarized faction: new settlement lists plots within one `land_listing_system` cycle.
   - Evicted plot: structures cleaned up per archetype's `eviction_policy`.
   - Nomadic+Market archetype rejected at registry load (validation catches the incoherence at boot, not runtime).
5. **Save/load round-trip** if the project ships saves: archetype key persists; capability bundle re-derives on load.
