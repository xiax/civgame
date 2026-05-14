# Tilted Elevation View

## Status
Shipped (v1):
- core scaffolding, `V` toggle, HUD button, projection helpers, auto-attached `ProjectedAnchor` covering tiles + walls + ground items + every world-living marker type
- mode-toggle camera anchor (`camera_recenter_on_mode_change_system`) so the same logical tile stays under the visual centre on TopDown↔Tilted flip
- camera focus through `camera_view_to_logical` (chunk loader)
- world-map bookmark jumps + activity-log focus through `camera_view_to_logical` / `logical_to_view_camera` (bookmarks stored as logical coords so they survive mode toggles)
- elevation skirts as `Sprite` quads (south face only, per-cliff sibling spawned in `spawn_chunk_sprites`, sized per frame from `z_step_px`, hidden in TopDown)
- chunk-seam back-fill: `attach_late_south_skirts_system` listens for `ChunkLoadedEvent` and back-fills the northern row's skirts via `TileSpriteIndex.skirt_by_tile`
- bumped `z_step_px` 3 → 6 px / Z for visible relief
- elevation-aware cursor picking via `CursorParams` SystemParam (`hover_info_system`, `right_click_context_menu_system`, `military_right_click_system`) — uses `pick_cliff_aware`, walking every Z-band candidate and selecting the tile actually projecting under the cursor (closest tile_y to camera wins ties)
- projected drag-select + selection rings (`selection_input_system`, `selection_gizmo_system`)
- debug gizmos via `LogicalProjector` SystemParam (path debug overlays, zone overlay) project all logical positions before drawing
- camera pan-Y normalization (WASD / middle-drag / scroll-pan multiply Y deltas by `y_scale` in tilted mode so a keypress moves the same logical-tile distance per second in either mode)
- day-night `z = 90` regression test (`tilted_dz_below_day_night`) sweeps the full Z range and asserts projected depth offset stays well below the overlay layer

TopDown remains bit-exact (689 tests pass, including 5 projection unit tests).

Deferred (intentional, won't fix):
- east/west cliff skirts (pure oblique-no-skew projection collapses east/west faces to 0-px-wide seams; south skirts cover all visible cliff faces)

## Summary
Add a toggleable 2.5D **oblique** projection (no X-rotation), implemented as a rendering-only layer. Simulation, pathfinding, AI, and tile storage stay on the existing logical tile grid. `V` toggles TopDown ↔ Tilted; TopDown is a bit-exact identity fallback.

## Architecture: One Projection Authority

Centralize projection rather than teaching every render site about it.

### Resources / types
- `MapViewMode::{TopDown, Tilted}` — `Resource`.
- `MapProjection { y_scale: f32 = 0.58, z_step_px: f32 = 3.0, pitch_deg: f32 = 54.0 }` — `Resource`. Oblique projection (X is preserved; Y is compressed and shifted by elevation).
- `ProjectedAnchor { tile: (i32, i32), z: i8 }` — `Component` on every visual root that should follow the view.

### Helpers
- `project(logical: Vec2, elev_z: i8, mode, proj) -> Vec3` — returns `(screen_x, screen_y, depth_z)`. `depth_z` encodes draw order; see §Y-sort below.
- `unproject_to_tile(screen: Vec2, plane_z: i8, mode, proj) -> (i32, i32)` — inverse onto a flat plane; layered elevation refinement on top (see §Picking).
- `camera_view_to_logical(camera_transform) -> Vec2` — focal ground-tile center.
- `logical_to_view_camera(logical) -> Vec2` — camera translation to center `logical` on screen.

### Projection system
`apply_view_projection_system` (PostUpdate, after movement, before rendering):
- Iterate `Query<(&ProjectedAnchor, &mut Transform)>`.
- Write `transform.translation = project(tile_center, anchor.z, mode, proj)` with depth-sorted z baked into the third component.
- In `TopDown` mode this returns the same value as the current `tile_to_world()` + per-kind layered z, so visuals are bit-exact unchanged.

### Migration shape
- Replace each spawn site's parent `Transform::from_xyz(tile_center_x, tile_center_y, kind_z)` with `ProjectedAnchor { tile, z }`. The projection system computes Transform every frame.
- `VisualChild` (anchored at `(0, -8, 0.1)`, hosts bob/sway anims) stays untouched — anim composes naturally on top of the projected parent.

## Y-Sort (Required for Tilted View to Look Right)

In tilted view, a wolf one tile south of a wall must draw on top of the wall — hardcoded `z` constants today (0.0–0.5 for entities; 90.0 for day-night) won't produce that.

Depth function (baked into `project()`):
```
depth_z = -(tile_y as f32) * 0.001 - elevation as f32 * 0.0005 + kind_bias
```
- `kind_bias`: small per-layer offset preserved from today's constants (terrain < ground items < entities < UI overlays).
- Day-night overlay stays at `z = 90.0`; depth-sort range must stay well below that.
- In TopDown mode the depth function collapses to today's hardcoded layer constants (identity).

## Elevation Skirts (Readable Relief)

For each tile whose surface Z is higher than its **southern** neighbor (south = camera-facing in oblique view), draw a 1-px dark band along the south edge from `(tile_view_pos, base_y)` down to `(tile_view_pos, base_y - z_step_px * Δz)`. Same treatment on east/west when neighbor delta exists. Implemented as `gizmos.line_2d` (cheap, no new textures).

## Mouse Picking Under Elevation

Inverse projection on a flat plane is exact, but elevation makes screen → tile ambiguous (clicking a cliff vs. the tile behind it). Routine:
1. Compute screen-ray intersection with ground plane at `z = 0` → starting tile guess.
2. Walk the nearest 3–5 candidates in front-to-back screen order (camera-facing direction first).
3. Return the first whose projected top-surface polygon contains the cursor.
4. For underground (`CameraViewZ < 0`), use the active Z-slice as the plane; skip the elevation walk.

Same routine serves hover, right-click, military command targeting, and drag-select endpoints.

## Drag-Select Semantics

In tilted mode the drag rectangle selects entities whose **projected positions** fall inside the screen-space rect — not entities whose logical tile falls in a 2D AABB. Implementation: project each candidate entity's `ProjectedAnchor` and test against the rect.

## Camera-Driven Systems

Every reader of `camera_transform.translation` that means "what tile is centered" must route through `camera_view_to_logical`:
- `update_simulation_focus_system` (`world/chunk_streaming.rs`).
- World-map bookmark jumps (`ui/world_map.rs`).
- Activity-log "focus on" (`ui/activity_log.rs`).
- LOD / streaming radii.

Camera setters (bookmarks, activity-log focus) route writes through `logical_to_view_camera`. In TopDown mode both helpers are identity, so the migration is safe.

Gizmo systems (`rendering/path_debug.rs`, zone overlay) keep using `gizmos.*_2d` but draw at projected XY rather than `tile_to_world()` directly.

## UI / Input

- HUD button next to the camera/Z display toggles mode.
- `V` hotkey (no current binding). Egui-focus-gated, same pattern as `1/2/3` speed presets and `Space` pause.
- Default = TopDown.

## Critical Files

- `src/world/terrain.rs` — keep `tile_to_world`/`world_to_tile` as-is; add `project`/`unproject_to_tile`/camera helpers here or in `src/rendering/projection.rs` (new).
- `src/rendering/mod.rs` — register `MapViewMode`, `MapProjection`, `apply_view_projection_system` (PostUpdate).
- `src/rendering/entity_sprites.rs` — ~40 spawn sites: parent `Transform::from_xyz(...)` → `ProjectedAnchor { tile, z }`. `VisualChild` untouched.
- `src/world/chunk_streaming.rs` — terrain tile spawns use `ProjectedAnchor`; `update_simulation_focus_system` reads `camera_view_to_logical`.
- `src/rendering/camera.rs` — pan/zoom unchanged; add `V` handler (egui-gated).
- `src/rendering/path_debug.rs` + zone gizmos — feed projected points to `gizmos.*_2d`.
- `src/ui/hover.rs`, `src/ui/orders.rs`, `src/ui/selection.rs` — swap `viewport_to_world_2d` paths for `unproject_to_tile` + elevation refinement; drag-select becomes screen-rect over projected positions.
- `src/ui/world_map.rs`, `src/ui/activity_log.rs` — camera jumps through `logical_to_view_camera`.
- `src/rendering/day_night.rs` — verify `z = 90.0` stays above the depth-sort range.
- `src/ui/hud.rs` — toggle button + `V` key handler.
- `CLAUDE.md` + `src/rendering/CLAUDE.md` + `src/ui/CLAUDE.md` — document `MapViewMode`, `MapProjection`, `ProjectedAnchor`, and the picking pipeline.

## Test Plan

- `cargo check`
- `cargo test --bin civgame`
- Manual `cargo run` (full map):
  - Toggle `V` repeatedly while panning/zooming; verify no flicker and tile/entity alignment.
  - Hover a tile at elevation; tooltip Z and `(tx, ty)` match what's visually under the cursor.
  - Right-click near a cliff edge; the menu's target tile is the top surface, not the tile behind.
  - Drag-select across mixed-elevation terrain; selected units match the screen-rect contents.
  - PageUp/PageDown: underground view still renders + picks correctly in tilted mode.
  - World-map bookmark jump and activity-log focus center the intended visible tile.
  - Pan to a chunk boundary in tilted mode; chunks load before they slide into view (focus uses logical center, not raw camera translation).
  - Path debug gizmos and zone overlays follow projected tile centers.
  - Day-night overlay still composites correctly above all sprites.
  - Toggle back to TopDown: visuals are bit-exact what they were before the change (regression safety).

## Assumptions

- No new crates.
- Oblique projection only (no X-rotation, no isometric/axonometric rotation) — keeps inverse projection cheap.
- This is a first 2.5D relief pass, not a mesh-based 3D terrain renderer.
- TopDown remains the compatibility fallback, so any visual/picking issue can be bypassed by toggling back.
- Update `CLAUDE.md` (root) + relevant `src/<dir>/CLAUDE.md` after behavior changes.
