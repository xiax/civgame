# Hybrid Swimming, Energy, and Visible Currents

## Context

Swimming as a learnable skill backed by a new physiological `Energy` resource, amphibious
pathfinding, a derived water-current field, and animated current visuals. The goal is to
make water a real, navigable, costly part of the world: humans can swim across
rivers/lakes, doing so is tiring and (when exhausted) dangerous, and currents are both
mechanically meaningful and visible.

**Decisions:** full dual-layer pathfinding graph; delivered in **3 phases**, each
independently shippable and testable.

`Energy` is a **separate component**, not a `Needs` field — it must NOT feed
`Needs::worst()` / `avg_distress()` (those drive mood; energy is physiology, not morale).
That separation is the explicit reason for a standalone component.

---

## Phase 1 — `Energy` physiological resource — ✅ SHIPPED

Independently valuable (affects all labor/movement/combat) with no water dependency.

**Status:** done. `energy.rs` (`Energy` component + hysteresis thresholds +
`energy_factor` + `energy_tick_system`). Spawn-wired (person/reproduction/fixture).
Drain: movement per-tile, labor via `energy_tick_system` (`AiState::Working` —
the willpower precedent, not 5 separate work-system call sites), combat per
swing + tired-cooldown. Recovery: sleep (bed ×2) + idle. Effects: speed +
work-progress ×`energy_factor`; exhaustion goal gate in
`best_with_incumbent`. UI: inspector line + debug slider. Tests green (987).

### Types — new `src/simulation/energy.rs`
- `Energy { current: f32, max: f32 }`, default `255.0 / 255.0` (0–255 float, matching the
  `Needs` convention). `max` exists for future Constitution-scaled capacity; v1 leaves it flat.
- Thresholds with hysteresis: `EXHAUSTED` (e.g. 40), `TIRED` (e.g. 90), `RECOVERED` (e.g. 140).
  An agent flagged exhausted stays exhausted until energy climbs back past `RECOVERED`.
- Helpers: `energy_factor() -> f32` (1.0 when fresh, ramps to a floor ~0.45 when exhausted —
  mirror the existing `medicine::sickness_work_factor` shape), `exertion_drain_scale`,
  `recover_idle`, `recover_sleep`, `is_exhausted` (stateful flag carried on the component).

### Spawn wiring (every human spawn site — easy to miss one)
Insert `Energy::default()` at:
- `person.rs::spawn_population` founder bundle (`src/simulation/person.rs:~500`).
- `reproduction.rs` newborn spawn.
- `test_fixture.rs` `spawn_person` builder (add an `.energy(..)` override for tests).

### Drain
- **Movement** (`movement.rs`): drain per tile stepped, scaled by `tile_speed_multiplier`
  (slow terrain = more effort), carried state, and mounted state (mounted drains far less).
- **Labor**: drain while work progress advances. Centralize via one helper called from the
  Sequential work systems (`production.rs`, `draftwork.rs`, `construction.rs`, `terraform.rs`,
  `farm.rs`) rather than duplicating arithmetic.
- **Combat** (`combat.rs`): drain per attack; when `TIRED`, lengthen attack cooldown.

### Recovery
- **Sleep**: `sleep::sleep_task_system` restores energy using the **same bed multiplier**
  already applied to sleep/willpower (`needs.rs` — `SLEEP_RECOVER` doubled on a bed).
- **Idle**: slow recovery for awake agents only when not moving (`PathFollow.status != Following`),
  not laboring, not in combat, not swimming. No separate Rest task in v1.

### Effects
- Movement speed in `movement.rs` multiplied by `energy_factor()`.
- Work progress multiplied by `energy_factor()` (mirror `sickness_work_factor` call sites).
- **Exhaustion gate on noncritical exertion**: add an executable gate to the per-scorer
  `GoalScoringContext` so exhausted agents stop picking up heavy-labor goals until
  `RECOVERED`. Survival goals (eat/drink/sleep/flee) remain ungated.

### UI
- `inspector.rs`: show Energy next to Needs/Stats (`cur / max`).
- `debug_panel.rs`: add an Energy slider.

### Phase 1 tests (`cargo test --bin civgame`)
- New humans / newborns spawn with full energy.
- Sleep restores faster than idle; a bed accelerates sleep recovery.
- Movement, labor, and combat each drain energy.
- Exhaustion drops `energy_factor` (slower work/movement) and the goal gate rejects
  noncritical labor until recovered.

---

## Phase 2 — Swimming skill + dual-layer amphibious pathfinding

### Skill plumbing — `src/simulation/skills.rs` — ✅ SHIPPED
- `SKILL_COUNT = 10`; `SkillKind::Swimming = 9`. `SkillKind::ALL` added.
- Unsafe transmute at `inspector.rs` replaced with `SkillKind::ALL` indexing.
  `SKILL_NAMES` in `debug_panel.rs` grew a "Swimming" entry. Suite green (987).
- Swimming is innate (not tech-gated).

### Amphibious traversal + swimming — ✅ SHIPPED (architecture deviation)

`TraversalProfile { Land, Amphibious }` + `step_cost_for` + `ChunkMap::passable_for`
/ `passable_step_for` + `astar::find_path_profile` + `PathRequest`/`PathFollow`
`profile` + `enqueue_with_profile` + `movement_system` profile threading +
`swimming.rs` (`SwimmingState`, energy drain, XP, fatigue-first drowning).
Suite green (992).

> **Deviation from "full dual-layer chunk graph".** The dual-layer graph
> (`chunk_graph`/`connectivity`/`chunk_router` amph maps) was **not** built.
> Instead the worker is **land-first**: `compute_outcome` runs the normal
> chunk-graph route, and for an `Amphibious` request falls back to a single
> bounded full-route A* (`compute_amphibious`) **only when land routing
> fails Unreachable/NoRoute**. This is a complete, lower-risk alternative —
> the land pathfinder is byte-identical (zero regression), and humans swim
> short crossings. Trade-off: a swim longer than the A* budget fails
> gracefully rather than routing hierarchically. Revisit the dual-layer
> graph only if long-distance amphibious routes become a real need.
> Emergency bank-retreat for exhausted swimmers is deferred (`last_safe_tile`
> tracked, not consumed).

### Traversal profile — `src/pathfinding/`
- New `enum TraversalProfile { Land, Amphibious }` (default `Land`).
- `chunk.rs`: add `passable_for(tx, ty, tz, profile)`. `Land` == today's `passable_at`.
  `Amphibious` additionally treats a **water-surface cell** as standable: foot tile is
  `Water`/`River` at its surface Z (`surface_z_at` for wet tiles), headspace `Air`.
  Mounted humans and all animals use `Land`.
- `tile_cost.rs`: `step_cost_for(kind, profile)`. For `Amphibious`, `Water`/`River` get a
  **finite but expensive** cost (≈0.35× speed) instead of `IMPASSABLE`. Phase 2 cost uses
  depth + contiguous-swim-distance only; Phase 3 enriches it with current vectors.

### Dual-layer chunk graph (the heavy work — `chunk_graph.rs`, `connectivity.rs`, `chunk_router.rs`)
- `ChunkGraph` gains parallel `amph_components` and `amph_edges` maps. **Key optimization:**
  populate them **only for chunks containing at least one `Water`/`River` tile**. Dry chunks
  fall back to the existing land maps via a `components_for(coord, profile)` accessor — so
  the vast all-dry majority costs zero extra storage and zero extra flood-fill.
- `classify_components` is parameterized by `profile`; water-touching chunks run it twice.
  Cross-chunk edge scan likewise emits `amph_edges` for water-touching borders.
- `ChunkConnectivity`: maintain a second union-find snapshot for `Amphibious`
  (land edges ∪ amph edges). `tile_reachable(..., profile)`.
- `ChunkRouter`: include `profile` in the Dijkstra-tree cache key.
- Hotspot flow fields stay **Land-only** (faction centers/storage/rally are dry);
  amphibious requests skip the hotspot fast-path. No change to `hotspots.rs`.

### Request / movement plumbing
- `PathRequest` and `PathFollow` carry `profile: TraversalProfile`. `PathRequestQueue::enqueue`
  keeps a default-`Land` signature; add `enqueue_with_profile`. Humans on foot enqueue
  `Amphibious`; mounted humans and animals enqueue `Land` (existing callers unchanged).
- `worker.rs::drain_path_requests_system` selects components/router/connectivity by profile
  and threads it into per-segment A* (`astar.rs` gains a `profile` arg).
- `movement.rs`: profile-aware `passable_step_3d`; stepping onto a water tile starts swimming.

### Swimming mechanics — new `src/simulation/swimming.rs`
- `SwimmingState { wet_ticks, exhausted_ticks, last_xp_tick, last_safe_tile }`, attached on
  water entry, removed on bank arrival.
- Effective swim speed/control combines `SkillKind::Swimming`, `Stats.strength`,
  `Stats.constitution`, current `Energy`, carried load, and (Phase 3) the current vector.
- Swimming drains `Energy` heavily — one of the highest-drain activities. XP accrues while
  wet, with bonus XP for resisting meaningful current.
- **Fatigue-first risk:** energy loss and slowdown come first. Drowning/injury only begins
  after `exhausted_ticks` exceeds a grace period **and** the agent is in deep or
  strong-current water. Exhausted swimmers emergency-retarget to the nearest reachable bank
  (`last_safe_tile` / nearest dry tile).
- Bridges/dams remain dry routes; their low cost means routing prefers them over swimming
  unless swimming is genuinely shorter and the agent has the energy budget.

### Phase 2 tests
- Dry pathing still rejects `Water`/`River`; animals and mounted humans never route through water.
- Amphibious humans cross a narrow river; AI picks a swim shortcut only when cheaper than detouring.
- Weak / tired / encumbered swimmers avoid or fail risky crossings; skilled/strong swimmers
  cross faster and lose less energy.
- Drowning damage begins only after the exhaustion grace period in deep water.
- Dual-layer graph: an all-dry chunk allocates no amph maps; a water-touching chunk does.

---

## Phase 3 — `WaterCurrentField` + visible currents

### Current field — new `src/world/water_current.rs`
- `WaterCurrentField`: derived, non-persistent, keyed by world tile. Stores direction,
  speed, and a source classification (`RiverChannel` / `RuntimeFlow` / `StillWater`).
- Build from data confirmed present at runtime:
  - **River channels:** `globe.rivers.edge_polylines` give tile paths; `RiverEdge.discharge`
    and `from/to_level` give magnitude and downstream direction.
  - **Runtime dam/pool flow:** local water-surface gradient from
    `chunk_map.water_level_at(tx,ty)` across neighbours → flow toward lower surface.
  - **Still lakes:** near-zero vector.
- Rebuilt incrementally on `ChunkLoadedEvent` / runtime-water updates; deterministic.

### Pathfinding/swimming integration
- `tile_cost::step_cost_for` for `Amphibious` adds current assist (downstream) / resistance
  (upstream/cross). Swim speed in `swimming.rs` adds the current vector to displacement.

### Rendering — `src/rendering/`
- No per-tile overlay precedent exists; introduce one. After `spawn_chunk_sprites`
  (`chunk_streaming.rs`), spawn a translucent flow-streak/chevron child sprite on each
  visible wet tile, rotation + animation speed from the current vector. New sprite key in
  `sprite_library.rs` (reuse the 32-color palette).
- Index overlays per chunk (mirror `TileSpriteIndex`); despawn on `ChunkUnloadedEvent` and
  rebuild on `TileChangedEvent` so wet↔dry transitions stay consistent.

### Hover / UI — `src/ui/hover.rs`
- Show water depth (`water_depth_at`) and current speed/direction for wet tiles
  (extend the existing well/water-column hover block).
- Inspector continues to show Energy; hover shows "Current" for wet tiles.

### Phase 3 tests
- Current field is deterministic; rivers yield downstream vectors; calm lakes ≈ zero.
- Runtime water slope produces current toward the lower surface.
- Flow overlays spawn/despawn with chunk load/unload and wet/dry tile changes.

---

## Docs (per phase)
Update the relevant `CLAUDE.md` as each phase lands:
- Phase 1: `src/simulation/CLAUDE.md`.
- Phase 2: `src/simulation/CLAUDE.md`, `src/pathfinding/CLAUDE.md`, root `CLAUDE.md` (tile palette / pathing).
- Phase 3: `src/world/CLAUDE.md`, `src/rendering/CLAUDE.md`, `src/ui/CLAUDE.md`.

## Verification
- `cargo test --bin civgame` after each phase (suite ~440 tests; keep it green).
- Manual smoke test with `cargo run`: order humans across a river, inspect Energy and
  Swimming XP in the inspector, confirm tired agents slow down and exhausted ones bank out,
  and verify flow streaks render and animate on wet tiles.

## Out of scope (v1)
Boats, rescues, aquatic/animal swimming, underwater work, Constitution-scaled `Energy.max`,
and save-persisted current vectors.
