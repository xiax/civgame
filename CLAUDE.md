# CLAUDE.md

Guidance for Claude Code working in this repository.

## Commands

```bash
cargo run                  # Run the game
cargo run -- --sandbox     # Sandbox mode (5×5 chunk map, one of every entity)
cargo build --release      # Optimized build
cargo check                # Fast type check
cargo test --bin civgame   # Run tests (binary crate — `cargo test` alone errors)
```

## Game-start options

`GameStartOptions` (resource in `game_state.rs`) drives the spawn-select screen and is read once by `spawn_population` + `seed_starting_buildings_system`:
- `era: Era` — every spawned member starts with all techs through this era Aware+Learned (`PersonKnowledge::seeded_through_era`); structures and walls scale up accordingly.
- `player_population: u32` — group size for `group_idx == 0` (player faction). Other factions stay at hardcoded `GROUP_SIZE=20`.
- `economy: EconomyPreset` — `Subsistence` (empty policy map = all-communist), `Mixed` (`mixed()` on non-staples; chief still allocates food/wood/stone), `Market` (`capitalist()` on every catalog resource). Applied per-faction in `spawn_population` via `policy::apply_preset`.
- `seed_buildings: bool` — sandbox sets false to skip pre-built seeding.

Chief assignment fix: `spawn_population` now sets `FactionData.chief_entity` and inserts `FactionChief` on the first spawned member. Without this, chief-driven systems waited for an unrelated runtime bonding event.

## Architecture

CivGame is a Dwarf Fortress-style civilization simulation on **Bevy 0.15** (ECS). Six plugins:

| Plugin | Directory | Responsibility |
|--------|-----------|----------------|
| `WorldPlugin` | `src/world/` | Procedural terrain (Z-levels -16..+15), chunk streaming (32×32 tiles), biomes, calendar, `SpatialIndex`, `ResourceCatalog` |
| `SimulationPlugin` | `src/simulation/` | Agent AI, needs, combat, reproduction, factions, technology, memory/gossip, plants, raids |
| `EconomyPlugin` | `src/economy/` | Markets, goods, prices, transactions |
| `PathfindingPlugin` | `src/pathfinding/` | Component-typed chunk graph, hotspot flow fields |
| `RenderingPlugin` | `src/rendering/` | 16×16 pixel art tiles, camera, chunk streaming visuals, entity sprites |
| `UiPlugin` | `src/ui/` | All UI via `bevy_egui`: inspector, HUD, world map, right-click menu, activity log |

Per-directory `CLAUDE.md` files cover subsystem detail. Claude Code auto-loads them when reading/editing in those trees.

## Construction Overhaul (`~/.claude/plans/take-a-look-at-lively-nebula.md`)

All six phases shipped. v1 delivers spine + lots + frontage placement + civic milestones + child farms + composite-shape helpers; the actual `BuildIntent::Composite` wiring of L/U-shaped templates remains a future iteration.

- **Phase 0:** `Settlement.peak_population` (maintained by `settlement_peak_population_system`, monotonic max of owner-faction's `member_count`); `StreetSpine` + `StreetSegment` + `StreetTier` types laid down. Seed-vs-grow contract documented at `seed_starting_buildings_system`: seeded buildings define initial conditions, milestone table (Phase 5) gates growth only.
- **Phase 1:** `SettlementPlan.road_spine: Vec<Axis>` (unused metadata) replaced with `spine: StreetSpine`. `generate_streetspine(home, layout, era, base_r)` produces variants per layout × era — Paleo/Meso → `None`; Neolithic+ Linear/Compact → `Linear`; Neolithic+ Radial/Sprawling/Citadel → `Spokes`; Chalcolithic+ Sprawling/Citadel → `Grid` with diagonals. Plaza node is `home_tile` so spines never bisect the seeded campfire/market_tile. `settlement_planner_system` enqueues spine segments into `RoadCarveQueue` once per `culture_hash` bump; `road_carve_system` drains them as Bresenham road tiles.
- **Phase 2:** `Plot` extended with `frontage_edge: Option<TileEdge>` and `access_tile: Option<(i32, i32)>`. `carve_plots_system` populates them via `frontage_for_rect`, probing up to 4 tiles outward from each edge midpoint and matching against `point_on_segment` per spine segment. Inspector hover surfaces `Frontage: {edge} → (access_tile)`.
- **Phase 3:** Residential placement (Hut/Longhouse) consults `find_footprint_at_frontage_lot` before `find_footprint_in_zone`. Walks vacant residential plots with frontage info owned by the faction, sorted by chebyshev distance to home; anchors the centre toward the frontage edge so doors face the road. Falls back to zone-area scoring when no vacant frontage lot exists. Civic placement keeps the prior zone-tile scan. `generate_candidates` threads `&PlotIndex` + `&Query<&Plot>`.
- **Phase 5:** New `civic_milestones.rs`: `(Era, peak_pop) → bool` table replaces `bed_count >= N` proxies for Granary/Shrine/Market/Barracks/Monument. Thresholds: Granary @ Neolithic+8, Shrine @ Neolithic+20, Market @ Chalcolithic+40, Barracks @ Chalcolithic+30, Monument @ BronzeAge+80. `chief_directive_system` reads `Settlement.peak_population` (falling back to `faction.member_count` pre-settlement-spawn) and threads it into `generate_candidates`. `seed_starting_buildings_system` bypasses the milestone gate — seeded structures define initial conditions; the table gates growth only.
- **Phase 4:** `building_template.rs` with `FootprintShape::{Rect, LShape, UShape}`, `Rotation` (4-way), `OpeningSide`, `shape_tiles`, `shape_z_stats`, `rotate_offset`. Type lay-down + helpers — `BuildIntent::Hut`/`Longhouse` continue using the rectangular `footprint_z_stats` path. Future `BuildIntent::Composite { template_id, anchor, rotation }` variants consume these helpers.
- **Seeding follow-on (per-era catalogs + house-per-founder + size-aware walls):** `seed_starting_buildings_system` builds `SettlementPlan` inline at seed time and routes each structure through `pick_seed_tile`, which prefers a clear tile inside the matching zone (Granary → Storage, Shrine/Monument → Sacred, Workbench/Loom → Crafting, Campfire → Civic, Market → Market, Barracks → Defense). House seeding diverges per era:
  - **Paleolithic / Mesolithic:** multi-hearth band camp via `paleolithic_hearth_positions(faction_id, home, members)`. One `Campfire` per hearth offset, each surrounded by a bed crescent of `target_beds / n_hearths` beds (radial-ring placement at radius 2..=5).
  - **Neolithic+:** founders housed in walled **Huts** (3×3 perimeter walls + door + 1 bed) seeded via `seed_walled_house_at`, mirroring `BuildIntent::Hut`'s shape but built fully (no Blueprint pipeline). Each hut gets a 2×2 attached **Farmland** yard (`seed_farmstead_yard`) on the first open side (east → west → south → north).
  - **Chalcolithic+:** Hut / Longhouse alternation (Longhouse = 5×3 perimeter + door + 2 beds), so couples / two-person households share a roof. Walls of Chalcolithic seed in `WallMaterial::Palisade`; Bronze upgrades to `Mudbrick`.
  - **Bronze Age:** as Chalcolithic, plus the yard patch grows to 3×3.
  - **Bed count = `max(faction.member_count, era_floor)`** so every founder has a bed, with the legacy 4 / 6 / 8 floors preserving small-faction camps.
  - **Walls follow actual settlement extent.** After every structure is placed, the `used`-tile bounding box is expanded by 2 tiles and unioned with the planned `Defense` zone rect; `seed_perimeter_rect` lays walls + an east-side door along the union. Falls back to the legacy r=5 ring only when both bbox and Defense zone are missing.
- **Phase 6:** `Plot.parent_plot: Option<PlotId>` added. When `household_land_acquisition_system` hands a Residential plot to a household, it also claims the nearest unowned same-village Agricultural plot within 12 chebyshev tiles as a child — same household holder, mirrored tenure (Lease child has `rent_per_month=0` since rent flows through the parent). `land_listing_system` and `rent_collection_system` skip `parent_plot.is_some()` plots. Inspector hover surfaces the parent link. Planter-task bias toward the household's child farm is deferred until Phase 4 lands (would touch `production.rs::planter_task`).

Test fixture updated: `TestSim::new` now calls `AsyncComputeTaskPool::get_or_init(...)` so spine carving's downstream chunk-graph rebuild doesn't panic in the headless harness.

## Nomadic mode (`~/.claude/plans/i-want-to-add-snappy-manatee.md`)

Foundation phases shipped (1-3, 6); behavior phases (4, 5, 7, 8, 10, 11) pending.

- **Phase 1: Lifestyle scaffolding.** `Lifestyle::{Settled (default), Nomadic}` enum on `FactionData`. `GameStartOptions.lifestyle` plumbed via spawn-select radio; only the player faction reads the option (AI factions stay Settled). Households inherit parent village's lifestyle via `spawn_household` alongside `land_policy`.
- **Phase 2: Pack/deploy abstraction + Bedroll.** `pack_deploy.rs` introduces the `Deployable` component (`{ packed_form: Option<ResourceId>, refund_pct: f32 }`) — every nomadic structure carries one. `Deployable::fully_packable(rid)` for full-carry shelter (Bedroll/Yurt); `Deployable::refund_only(0.5)` for sticks-and-leaves teardown (Tent). `Good::Bedroll` resource added to catalog (`assets/data/resources/core.ron`) + `core_ids::bedroll()`. `BuildSiteKind::Bedroll` (recipe: 1 skin + 2 wood, no tech gate, `work_ticks: 30`) finalizes as `(Bed { tier: Crude }, Deployable, StructureLabel("Bedroll"))` and inserts into `BedMap` so existing sleep dispatch (`SleepMethod`) finds it untouched.
- **Phase 3: Tent + Yurt structures.** `BuildSiteKind::{Tent, Yurt}` plus a `TentShelter { tier: ShelterTier::{Tent, Yurt} }` marker component. Tent recipe (6 wood + 3 skin, no gate) finalizes with `Deployable::refund_only(0.5)` — half the wood drops as `GroundItem`s on migration teardown. Yurt recipe (8 wood + 6 skin, gated on new `PORTABLE_DWELLINGS` tech, Workbench station) finalizes with `Deployable::fully_packable(packed_yurt)` — packs into the new `packed_yurt` good (catalog: `bulk: two_hand`, `weight_g: 80000` so two pack horses saturate one yurt). New Neolithic-era tech `PORTABLE_DWELLINGS` (id 43, prereq `LOOM_WEAVING`); `TECH_COUNT` bumped 43 → 44. Inspector / hover already surface Tent/Yurt via `StructureLabel`.
- **Phase 4: Storage backend split.** `compute_faction_storage_system` runs a *second pass* after the legacy `FactionStorageTile`-anchored ground-item rollup: for every faction whose `lifestyle.is_nomadic()`, sum each member's `EconomicAgent.inventory` into `faction.storage.totals`. Settled factions skip the second pass (and never see a nomadic-flagged faction in the first), so their `storage.totals` is bit-for-bit identical to pre-refactor numbers (regression invariant for the existing 411-test suite). Pack-animal inventories + `PackBundle` ground tiles fold in here once Phases 5 / 8 land. Withdraw-path rewiring (`StorageLocator { SettledTile | Member | PackBundle }`) is deferred — for now nomadic factions skip `WithdrawMaterialFromStorageMethod` automatically because they have no `FactionStorageTile`.
- **Phase 6: Skip Settlement + seed nomadic camp.** `auto_found_default_settlements_system` short-circuits when `faction.lifestyle.is_nomadic()`. `spawn_population` skips the per-faction `FactionStorageTile` spawn for nomads. `seed_starting_buildings_system` branches *before* the era ladder: nomadic factions go through `seed_nomadic_camp(...)` which reuses `paleolithic_hearth_positions` to lay down hearths for any era, then `seed_bedrolls_around_hearth` (radial 2..=5, one bedroll per founder), `seed_tents_around_hearth` (outer ring 5..=7, ~1 tent per 4 founders), and `seed_yurts_around_hearth` (Neolithic+, inner ring 3..=5, capped at 2 per camp).
- **Phase 7 (minimal): Chief gates.** Both `chief_directive_system` (construction) and `chief_job_posting_system` (jobs) early-out per-faction when `lifestyle.is_nomadic()`. Nomadic bands stop queueing Hut/Wall/Granary blueprints and stop posting Stockpile/Farm/Build/Craft/Haul jobs. Members still get autonomous need-driven goals (AcquireFood, Sleep) and personal-need gathering. Phase 7 follow-on (`nomad_chief_directives` queueing Tent/Yurt/Bedroll/Campfire replenishment) is deferred until Phase 8's migration commit re-seeds structures at new camps and we can tell what's missing.
- **Pending behavior phases.** Phase 5 (`PackSaddle` + tamed-animal carry capacity + cohesion to camp); Phase 7 follow-on (`nomad_chief_directives`); Phase 8 (`nomad.rs` migration trigger on `TICKS_PER_SEASON` cadence with desperation override; `Task::PackCamp`/`DeployItem`; seasonal target picker); Phase 9 (UI surfaces — debug panel lifestyle/migration display, hover for Deployable); Phase 10 (wild herd LOD simulation); Phase 11 (sedentarization).

## Land ownership (`src/simulation/land.rs`)

Plot-based ownership layer over the existing `SettlementPlan` zones. Phases 1–6 shipped: data model, carving, valuation, the `LandPolicy` preset wire-up, the building-tenure gate, plot listings (sale / lease / sharecrop), household leasing/freehold/sharecrop acquisition, rent collection, eviction, and sharecrop harvest split. Settlement-expansion plot diff (Phase 8) land later (see `~/.claude/plans/i-want-to-add-starry-conway.md`).

- **`Plot`** (Component): `id`, `settlement_id`, `faction_id`, `rect: TileRect`, `z: i8`, `zone_kind`, `tenure`, `holder`, `base_value`, `last_valued_tick`, `missed_payments`, `frontage_edge`, `access_tile`, `parent_plot`.
- **`Tenure`**: `StateOwned` (default), `Leased { rent_per_month, period_days, paid_through_tick }`, `Sharecropping { share_to_landlord, paid_through_tick }`, `Freehold`.
- **`TenureHolder`**: `State { faction_id }`, `Household { faction_id }`. New variants slot in here.
- **`PlotIndex`** (Resource): `by_id` / `by_settlement` / `by_tile (i32, i32) → PlotId` / `by_faction_hash` (idempotency check) / `next_id`. `plot_at(x, y)` is the hot lookup.
- **`LandListings`** (Resource): `for_sale` / `for_lease` Vec stubs. Populated in Phase 4+.
- **`carve_plots_system`** (FixedUpdate, Economy, after `settlement_planner_system`, before `chief_directive_system`): subdivides each faction's `SettlementPlan` zones into plots when the plan's `culture_hash` differs from the last carved hash. Sizes by zone kind: Residential 6×6, Crafting/Storage 4×4, Agricultural 10×10; Civic/Sacred/Market/Defense remain whole-zone single plots. Each carved plot is valued via `compute_plot_value` and replans tear down + re-carve (Phase 1 simplicity; diff path lands later).
- **`compute_plot_value`** (`PLOT_BASE_VALUE = 50.0`): `BASE * zone_mul * (centre_factor + home_factor) * terrain_factor`. Distance factors fall off with Chebyshev distance to `Settlement.market_tile` and `FactionData.home_tile`; zone multiplier ranges 0.6 (agricultural — bulk farmland) to 1.5 (Market). Terrain factor samples fertility at the plot's centre + four corners; Agricultural plots reweight fertility harder. Workplace-proximity premium is deferred until job-posting integration.
- **`LandPolicy`** (`src/economy/policy.rs`): faction-level governance — `state_sells_land`, `state_rents_land`, `state_sharecrops`, `private_freehold_allowed`, `default_lease_period_days`, `rent_yield_pct`, `default_share_to_landlord`. Default = all-false (Subsistence). `land_policy_for(EconomyPreset)` flips Mixed → rent + sharecrop; Market → adds outright sale + freehold. Applied per-faction in `spawn_population` alongside `apply_preset`. Households inherit parent village's `LandPolicy` via `FactionRegistry::spawn_household`.
- **`tile_buildable_by(plot_index, plot_q, tile, faction_id, requesting_household)`**: tenure gate consumed by the chief / household builders. Wild tile (no plot) ⇒ permitted; State plot ⇒ owning faction only; Household plot ⇒ matching household only. Civic blueprints pass `requesting_household = None`; personal blueprints pass the owner's household id. Wired into `chief_directive_system` between candidate selection and `spawn_intent`. Pure-logic core in `holder_permits_build` for unit testing.
- **`land_listing_system`** (Economy, every `TICKS_PER_DAY/4`): publishes `Listing { plot_id, asking, kind, listed_tick, unsold_days }` entries into `LandListings.for_sale` / `for_lease` for `StateOwned` plots whose owning faction's `LandPolicy` permits transfers. Civic / Sacred / Defense / Market zones are state-retained and never listed. Cap: `TARGET_LISTINGS_PER_FACTION = 8`. Sale asking = `plot.base_value`; lease asking = `base_value * rent_yield_pct` (≈ 4% of value/month). Sharecrop listings remain a `ListingKind` variant but stay deferred until Phase 6.
- **`household_land_acquisition_system`** (Economy, every `TICKS_PER_DAY`): household sub-factions with no plot yet and `treasury ≥ HOUSEHOLD_MIN_TREASURY_FOR_LEASE` browse listings owned by their parent village. Affordability bounds `HOUSEHOLD_LEASE_AFFORDABILITY = 40 %` and `HOUSEHOLD_BUY_AFFORDABILITY = 70 %` of treasury. Preference: cheapest affordable Sale → cheapest affordable Lease. Transaction = atomic faction-treasury debit/credit (not `pay()` — that's agent-to-agent), `Plot.tenure` flips to `Freehold` / `Leased { rent_per_month, period_days, paid_through_tick }`, `Plot.holder` to `Household { faction_id }`, listing removed. Currency invariant preserved.
- **`rent_collection_system`** (Economy, every `TICKS_PER_DAY * 30`): for each `Tenure::Leased` plot whose `paid_through_tick` has expired, debit `rent_per_month` from the household sub-faction's treasury and credit the landlord faction. Success advances `paid_through_tick` by one period and resets `missed_payments`. Failure (treasury < rent) increments `missed_payments` and still advances the cycle so the system doesn't re-bill the same overdue month. Once `missed_payments` reaches `EVICTION_MISS_THRESHOLD = 2`, the plot evicts: `tenure → StateOwned`, `holder → State { faction_id: original_landlord }`. Phase 5 minimal: structures on the evicted plot stay in place; downstream component cleanup lands later.
- **Sharecropping (Phase 6)**: agricultural plots in factions with `state_sharecrops` get a `ListingKind::Sharecrop` entry alongside any sale/lease offering. Acquisition preference: Sale > Lease > Sharecrop (households would rather buy than rent than sharecrop). Sharecrop has zero upfront cost and sets `Tenure::Sharecropping { share_to_landlord, paid_through_tick }`. **Harvest split**: `gather_system` checks each harvest tile via `lookup_sharecrop_split`; on a Sharecropping plot, the landlord's share (computed by `split_sharecrop_yield`, rounded down in tenant's favour) is dropped at the landlord's nearest `FactionStorageTile` while the tenant routes their cut through the standard `route_yield` path. `SharecropResources` SystemParam in `land.rs` bundles the `PlotIndex` + plot query + `SpatialIndex` + `GroundItem` query needed by the hook so `gather_system` stays under Bevy's 16-param ceiling.
- All plots start `Tenure::StateOwned` held by the settlement's owning faction. Inspector hover surfaces plot info — id, zone, rect, tenure, holder, value (`src/ui/hover.rs`).

## Simulation scheduling (`SimulationSet`)

```
ParallelA → ParallelB → Sequential → Economy
```

- **ParallelA** — read-heavy (needs, mood, LOD, goal updates, animal sensing)
- **ParallelB** — HTN dispatchers; `goal_dispatch_system` is the stale-reset / Explore-cleanup catch-all
- **Sequential** — mutating, ordered: `gather` → `dig`/`construction` → `movement` → `combat` → `production`
- **Economy** — gossip, faction storage rollup, reproduction, raids, technology, market prices

## Spatial / tile / rendering conventions

- World tiles: `(i32, i32)`; convert with `tile_to_world()`.
- Chunks: `ChunkCoord::from_world()` (uses `div_euclid()`).
- Z-levels: `i8`, `Z_MIN=-16`, `Z_MAX=15`.
- Fixed update: **20 Hz** (`main.rs`).
- After mutating a tile, emit `TileChangedEvent { tile }`; `refresh_changed_tiles_system` (PostUpdate) rebuilds sprites.
- `CameraViewZ` defaults to `i32::MAX` (surface); lower it to peer underground.
- `TileMaterials`/`FogTileMaterials` keyed by `(TileKind, OreKind, z_bucket)`. Ore tiles fan out via `RENDERABLE_ORES`; colors in `color_map.rs::ore_tile_color`.
- **`sprite_library.rs`:** procedural pixel art from a 32-color palette via `ascii_to_image`. Reuse the palette/helpers — don't introduce new color systems.
- **PNG textures** in `assets/textures/` toggled by `entity_sprites::toggle_art_mode`.
- **`AnimalTextures`:** 8-direction PNGs for Wolf/Deer/Horse loaded at Startup from `assets/textures/<species>/rotations/{south,...}.png` (48×48). `ArtMode::Pixel` uses these; `ArtMode::Ascii` falls back to procedural sprites. `FacingDirection` is 8-way; `cardinal_str()` collapses to 4-way for the procedural library used by other animals. `animate_{wolves,deer,horses}_system` swaps the directional PNG and applies bob/sway on `VisualChild`.
- **GroundItem sprites:** `entity_sprites::spawn_ground_item_sprites` reactively attaches a child sprite. Currently only `Good::Stone`; add a match arm for more.
- **`SpatialIndex` (`world/spatial.rs`):** maintained incrementally. Every indexed entity carries `Indexed { kind, tile, z }`. `sync_indexed_after_move_system` (Sequential, after movement systems) handles add+move via `Or<(Changed<Transform>, Added<Indexed>)>`. Despawn uses an `on_remove` hook on `Indexed` (registered in `WorldPlugin::build`). `IndexedKind` covers Person/Wolf/Deer/Horse (mobile, also in `agent_counts`) plus Plant/GroundItem/Bed (static, 2D only). When converting an animal to a `Corpse` in `combat.rs::death_system`, also `remove::<Indexed>()`. New spawn sites for indexed kinds **must** include `Indexed::new(...)`. Sites that mutate `PersonAI.current_z` without mutating `Transform` must call `transform.set_changed()`.

## Constraints

- **ECS:** logic in Systems; Components hold data only. No OO inheritance.
- **UI:** `bevy_egui` for panels (avoid `bevy_ui` except specific overlays).
- **Hashing/randomness:** `ahash::AHashMap` (not `std::HashMap`). `fastrand` in hot paths, `rand` for init.
- **No new crates** without explicit permission.
- **Error handling:** avoid `unwrap()` in core systems; use `match`/`if let`.
- **Mutable aliasing:** be careful with Bevy query aliasing; test empirically.
- **Doc updates:** when behaviour changes, update the matching `CLAUDE.md`. Subsystem-local changes in `src/<dir>/CLAUDE.md`; cross-cutting in this file.
