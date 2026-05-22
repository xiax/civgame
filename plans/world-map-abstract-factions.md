# World-map abstract factions

## Context

Players report factions starting "way too close" to each other. Investigation
showed the cause is architectural, not a scoring bug:

- The **Globe** is the real world map: 512×256 climate cells = 1024×512 chunks =
  **32,768×16,384 tiles**.
- But `spawn_population` (`src/simulation/person.rs`) crams **all 10 factions**
  into a **1024×1024-tile window** (the `WORLD_CHUNKS_X=32`-chunk pre-gen window,
  2×2 mega-chunks) centred on the player's selected mega-chunk. 10 factions fight
  over one corner → clustering.
- It is boxed because a faction can only be placed on **generated terrain**, and
  only that 32×32-chunk window is pre-genned at `OnEnter(Playing)` (by
  `spawn_world_system`, terrain.rs:1013-1062).
- There is **no off-map faction system**. `world_sim_system` only updates Globe
  cells that *already* carry a `faction_id` — it never creates distant
  civilizations. Nothing materializes factions as the player travels.

**Decided scope** (user): the player + **2-3 rival factions** start near the
player as full detailed factions; the remaining factions live as **abstract
Globe civilizations** that **materialize into real entities when the player
travels near them** and dematerialize back to abstract data when the player
leaves.

This is a real new system, delivered in phases. **Phase 1 alone fixes the
immediate "too close" complaint.** Phases 2-4 build the world-map model.

## Design overview

- **`FactionData` exists for all ~10 factions from game start** (registry
  entries are cheap). A new `FactionData.materialized: bool` distinguishes the
  two states. Treasury, techs, faction relationships, raid state live on
  `FactionData` permanently and survive round-trips — only `Person` /
  `Settlement` / structure **entities** materialize and despawn.
- **Near factions** (player + 2-3 rivals, `materialized = true`): spawned as full
  entity groups in the pre-genned window, properly spaced.
- **Abstract factions** (`materialized = false`): a new `AbstractFactions`
  resource holds `AbstractFaction { faction_id, home_tile, home_megachunk,
  population, food_stock }`. Their `population`/`food_stock` mirror onto the
  home Globe cell so the existing `world_sim_system` keeps ticking them.
- **Materialize**: when chunk DATA for an abstract faction's home region loads
  (player approaches), spawn its band from the abstract record; flip
  `materialized = true`.
- **Dematerialize**: when the home region's chunks unload (player leaves),
  aggregate live members back into an `AbstractFaction` record, despawn the
  entities, flip `materialized = false`.

---

## Phase 1 — Near factions only, properly spaced (immediate fix) — ✅ SHIPPED

Implemented in `person.rs`: `NEARBY_RIVAL_COUNT`/`NEAR_FACTION_TARGET_SPACING`
constants, `faction_spacing_score` free fn, `near_factions` loop bound,
`score_home_candidate` rewritten, 6 pure-fn tests in `person::tests`. CLAUDE.md
updated. Phases 2-4 below remain to be built.

`spawn_population` (`person.rs` ~305-737) spawns only the player + `NEARBY_RIVAL_COUNT`
rivals as full factions; it no longer iterates all `num_groups`.

- Add constants near person.rs:301-303:
  ```rust
  const NEARBY_RIVAL_COUNT: u32 = 3;          // rivals spawned near the player
  const NEAR_FACTION_TARGET_SPACING: f32 = 280.0; // tiles; spacing reward saturates here
  ```
- Loop `for group_idx in 0..=NEARBY_RIVAL_COUNT` instead of `0..num_groups`.
- Replace the binary `too_close` term in `score_home_candidate` (person.rs
  ~371-388) with a continuous **farthest-point** reward — extract a testable
  free function:
  ```rust
  /// 0 when coincident with an existing home, saturating to +100 once the
  /// candidate is >= NEAR_FACTION_TARGET_SPACING from every placed home.
  fn faction_spacing_score(tx: i32, ty: i32, others: &[(i32, i32)]) -> i32 {
      let min_dist = others.iter()
          .map(|(hx, hy)| {
              let (dx, dy) = ((tx - hx) as f32, (ty - hy) as f32);
              (dx * dx + dy * dy).sqrt()
          })
          .fold(f32::INFINITY, f32::min);
      ((min_dist / NEAR_FACTION_TARGET_SPACING).min(1.0) * 100.0) as i32
  }
  ```
  `score_home_candidate = faction_spacing_score(..) + river_score` (river curve
  unchanged). Continuous scoring spreads the handful of near factions maximally
  even when the window is tight; no silent cliff degradation.
- The player faction (`group_idx == 0`) is unchanged — placed first via
  `region::pick_player_home_in_megachunk`, pushed to `spawned_homes` so rivals
  avoid it.
- The remaining factions (`num_groups - 1 - NEARBY_RIVAL_COUNT`) are **not**
  spawned here — Phase 2 seeds them abstractly. Until Phase 2 ships they simply
  don't exist; that is acceptable and still a strict improvement.

**Tests** (`person.rs`, new `#[cfg(test)] mod tests`, pure-function style
mirroring `region.rs`): `faction_spacing_score` empty→100, coincident→0,
saturates at spacing→100, monotonic, picks the nearest home; plus a behavioural
loop test asserting 4 factions placed via the scorer end up pairwise well-separated.

After Phase 1: starting area has player + 3 spread rivals. Ships independently.

---

## Phase 2 — Abstract faction seeding across the Globe — ✅ SHIPPED

Shipped: `abstract_faction.rs` (`AbstractFaction`, `AbstractFactions` resource,
`seed_abstract_factions_system`, `cell_spacing_score` farthest-point picker);
`FactionData.materialized` flag (`faction.rs`, default `true`);
`auto_found_default_settlements_system` / `auto_found_default_camps_system` /
`faction_decision_system` / `pick_raid_target` gated on `materialized`;
resource + OnEnter system registered in `SimulationPlugin`. 4 pure-fn tests;
full suite green (1022). World-sim integration needed no change — it already
ticks claimed unloaded cells, and `seed_abstract_factions_system` stamps the
home cells; abstract `population`/`food` live on the `WorldCell`, not duplicated
on the record. Phases 3-4 below remain.

New module `src/simulation/abstract_faction.rs`.

- `struct AbstractFaction { faction_id: u32, home_tile: (i32,i32),
  home_megachunk: MegaChunkCoord, population: u16, food_stock: f32 }`.
- `#[derive(Resource)] struct AbstractFactions { by_id: AHashMap<u32, AbstractFaction>,
  by_megachunk: AHashMap<MegaChunkCoord, u32> }`.
- `seed_abstract_factions_system` (`OnEnter(Playing)`, after `spawn_population`):
  - For each not-yet-spawned faction slot, pick a **habitable** Globe cell
    (`WorldCell.biome.is_habitable()`, `globe.rs`) far from the player's pre-gen
    window and from already-placed abstract homes — farthest-point selection
    over Globe cells (reuse the Phase 1 scoring idea at cell granularity).
  - `registry.create_faction(home_tile)`; apply economy/era like the near
    factions; set `FactionData.materialized = false`.
  - Derive starting `population` (≈ `GROUP_SIZE`) and `food_stock`; insert the
    `AbstractFaction`; stamp the home `WorldCell.{faction_id, population,
    food_stock}` so `world_sim_system` ticks it.
- `world_sim_system` already simulates claimed unloaded cells (population/food/
  raids). Add a small apply-loop that mirrors each cell delta back into the
  matching `AbstractFactions` record (or refactor `world_sim` to tick
  `AbstractFactions` directly — preferred if low-risk).
- `FactionData.materialized` field added in `faction.rs`; near factions set it
  `true` in `spawn_population`.

After Phase 2: the world map carries ~6 abstract civilizations that grow/shrink
and raid each other off-screen. They are not yet visitable.

---

## Phase 3 — Materialization on approach — ✅ SHIPPED

Shipped: `person::spawn_faction_band` + `FactionBandSpawn` extracted from
`spawn_population` (storage tile + reachable-flood members + chief; advances
`clock.population`/`bucket_size` per member like `reproduction.rs`), called by
both `spawn_population` and the new `materialize_abstract_faction_system`
(`abstract_faction.rs`, FixedUpdate after `chunk_streaming_system`). The system
triggers on a `ChunkLoadedEvent` for an abstract faction's home chunk,
re-anchors the home onto passable ground (`nearest_passable_tile`), spawns up
to `MAX_MATERIALIZED_MEMBERS = 40`, flips `materialized = true`, clears the
Globe cell, drops the `AbstractFactions` entry. Full suite green (1022); 4
abstract_faction tests. **Phase 4 below remains** — until then a materialised
faction stays materialised (entities persist, Dormant when far; no double-sim
since the cell was cleared). The original Phase 3 sketch follows.

- Extract a reusable `spawn_faction_band(commands, registry, faction_id,
  home_tile, member_count, catalog, ...)` from the per-group body of
  `spawn_population` (person.rs ~449-724) — the member-spawn + chief + reachable-
  pool logic. Both `spawn_population` and materialization call it.
- `materialize_abstract_faction_system` (FixedUpdate): trigger on
  `ChunkLoadedEvent` (or chunk-DATA-loaded state) for a chunk inside an
  `AbstractFactions.by_megachunk` home region. On trigger, **after the home
  chunk's terrain exists** (so `chunk_map.is_passable` is valid):
  - `member_count = min(abstract.population, MAX_MATERIALIZED_MEMBERS)` (cap so a
    grown abstract civ doesn't spawn thousands of entities; the abstract
    `population` is the whole civilization, the band is a settlement's worth).
  - Call `spawn_faction_band(...)`; seed `FactionData` storage from `food_stock`.
  - Flip `FactionData.materialized = true`; remove the `AbstractFactions` entry;
    clear the Globe cell's faction fields (or mark loaded so `world_sim` skips it
    — it already skips loaded cells).
  - Let `auto_found_default_settlements_system` (idempotent, FixedUpdate Economy)
    found the settlement and the organic settlement AI build it over time — **no
    runtime building-seed pipeline needed** (v1 simplification).

**Open question** (resolve in implementation): whether to also restamp a minimal
settlement immediately vs. let it grow organically. v1 = grow organically.

---

## Phase 4 — Dematerialization

- `dematerialize_faction_system` (FixedUpdate): when a materialized non-player
  faction's home region chunks all unload (player left — detect via chunk
  unload / `SimulationFocus` focus loss):
  - Count surviving members, sum faction food storage.
  - Despawn the faction's `Person` / `Settlement` / structure entities.
  - Re-insert an `AbstractFaction { population, food_stock }` record; re-stamp
    the home Globe cell; flip `materialized = false`.
- `FactionData` stays in the registry across the round-trip — techs, treasury,
  relationships, raid history persist.
- v1 simplification: structures are not abstractly preserved; on the next
  materialization the settlement re-seeds and the organic AI rebuilds
  deterministically. Note this in code + docs.

After Phase 4: full round-trip — the player can travel the world map, meet
distant civilizations as living factions, leave, and find them changed.

---

## Phase 5 — Deferred (sketched, actionable later)

Abstract factions founding new settlements / expanding territory on the Globe;
abstract diplomacy and tribute between off-map factions; abstract↔abstract raid
resolution richer than `world_sim`'s current intent emission. Each is additive
on the Phase 2-4 substrate. Write a follow-up plan file when Phase 4 lands.

---

## Critical files

- `src/simulation/person.rs` — Phase 1 scorer + near-only loop; extract
  `spawn_faction_band` (Phase 3).
- `src/simulation/abstract_faction.rs` *(new)* — `AbstractFactions` resource,
  seed / materialize / dematerialize systems.
- `src/simulation/faction.rs` — `FactionData.materialized` flag.
- `src/simulation/world_sim.rs` — mirror cell deltas into `AbstractFactions`.
- `src/world/globe.rs` — habitable-cell iteration helper for seeding.
- `src/simulation/region.rs` — reuse `MegaChunkCoord`; materialization trigger
  alongside `detect_edge_crossing_system` / chunk-load events.
- `SimulationPlugin` build — register the new systems.
- `src/simulation/CLAUDE.md` — document the near/abstract model and round-trip.

## Verification

- Phase 1: `cargo test --bin civgame simulation::person`; `cargo run` → confirm
  player + 3 rivals spread out, no clustering.
- Phase 2: `cargo run`, open world map UI → confirm abstract faction markers
  spread across the Globe; let it run and confirm populations evolve.
- Phase 3-4: `cargo run`, travel toward an abstract faction → confirm it spawns
  as real entities near its Globe home; travel away → confirm it despawns and
  the world-map marker returns; travel back → confirm state persisted.
