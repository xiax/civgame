# Detailed Plan: Make Gameplay Terrain Match The World Map

## Summary
The main mismatch comes from the player choosing a 16×16-chunk mega-chunk on the world map, while startup home selection currently searches the larger 32×32-chunk generated area. That lets the actual home drift into neighboring map cells, often toward rivers or better terrain. The fix will keep the player home inside the selected mega-chunk, reduce local elevation noise so gameplay height follows the globe preview more closely, and expose diagnostics on the spawn map.

## Implementation Changes
- Add shared mega-chunk bounds helpers near `MegaChunkCoord` in [src/simulation/region.rs](/Users/xiao1/civgame/src/simulation/region.rs): `tile_bounds(mx, my) -> (min_tx, min_ty, max_tx_exclusive, max_ty_exclusive)`, `contains_tile(mx, my, tx, ty)`, and possibly `chunk_bounds(mx, my)` for clearer call sites.
- Refactor player home selection in [src/simulation/person.rs](/Users/xiao1/civgame/src/simulation/person.rs) so `group_idx == 0` searches only inside `PendingSpawn`’s selected mega-chunk. It should still reject impassable, stone, water, and river-channel tiles.
- Preserve the current “best local site” behavior inside that fixed boundary: score passable candidates by river proximity, terrain suitability, fertility when available, and mild center distance, but never allow the winner outside the selected mega-chunk.
- Keep AI faction home placement using the existing broader generated region so neighboring factions can still seed around the player’s area. Only the player-selected faction gets the strict mega-chunk constraint.
- Add a deterministic fallback if no candidate is found in the selected mega-chunk: scan from the mega-chunk center outward for the nearest passable non-stone tile inside the same mega-chunk. If still none, log a warning and fall back to the current broader behavior as a last resort.
- In [src/world/terrain.rs](/Users/xiao1/civgame/src/world/terrain.rs), reduce `LOCAL_DETAIL_AMP` from `0.20` to a tighter value, recommended `0.10`, so `surface_v` is visibly anchored to `Globe::sample_climate`.
- Make local detail biome-sensitive: keep lowlands/coasts/deserts closer to macro elevation, while allowing mountains and badlands slightly stronger variation, capped below the current `0.20`.
- Keep river and lake stamping authoritative: gameplay rivers continue to come from `Globe.rivers.edge_polylines`, and the world-map river overlay should remain the preview of the same polyline data.
- Add spawn-map diagnostics in [src/ui/spawn_select.rs](/Users/xiao1/civgame/src/ui/spawn_select.rs): tooltip should show selected mega-chunk bounds, center tile, predicted player home tile, dominant biome, center elevation, min/max sampled elevation, and nearest river distance.
- Add a small marker overlay on the spawn map: one marker for mega-chunk center and one for predicted home candidate. The home marker should use the same helper/scoring path as `spawn_population`, so preview and gameplay agree.
- Update [src/ui/CLAUDE.md](/Users/xiao1/civgame/src/ui/CLAUDE.md), [src/world/CLAUDE.md](/Users/xiao1/civgame/src/world/CLAUDE.md), and root [AGENTS.md](/Users/xiao1/civgame/AGENTS.md) to document the stricter player-start constraint and tighter globe-anchored terrain.

## Interfaces And Data Flow
- Introduce a small pure helper for selecting a player home candidate from `ChunkMap`, selected mega-chunk, and scoring options. Both spawn-select diagnostics and `spawn_population` should call this helper or equivalent shared logic.
- The helper should return enough data for UI diagnostics: chosen tile, score, river distance, elevation, biome, and fallback mode used.
- Keep public gameplay resources unchanged: `PendingSpawn` remains `Option<(i32, i32)>` in mega-chunk coordinates, and no save format or serialized `Globe` layout changes are needed.
- Avoid adding dependencies. Use existing `ChunkMap`, `Globe::sample_climate`, `biome::classify_at_tile`, `MegaChunkCoord`, and tile helpers.

## Edge Cases
- Selected mega-chunk contains mostly ocean or mountains: UI already blocks dominant non-habitable picks, but the home selector still needs a robust fallback inside the tile bounds.
- Rivers at the edge of the selected cell: the player may spawn near that river only if the river is actually inside the selected mega-chunk.
- Elevation boundaries: local noise can nudge tile-level height, but the max deviation should be small enough that a world-map lowland no longer becomes a tall plateau.
- Coordinate orientation: preserve the existing north-up world-map Y flip in UI, but add tests so pixel/grid click conversion maps to the same mega-chunk used by gameplay.

## Test Plan
- Add unit tests for `MegaChunkCoord::tile_bounds`, `center_tile`, `from_tile`, and `contains_tile`, including edge tiles and exclusive max bounds.
- Add a player-home selection test proving a selected mega-chunk never produces a home outside its bounds.
- Add a regression test for the current bug shape: selected mega-chunk centered in a larger generated region with an attractive river in a neighboring mega-chunk must still choose a home inside the selected mega-chunk.
- Add terrain consistency tests comparing `Globe::sample_climate(tx, ty)` to `surface_v(tx, ty)` or generated `surface_z`, asserting the deviation stays within the new tighter detail cap.
- Add a UI coordinate conversion test, if practical without egui rendering, for screen/grid Y-flip math used by spawn-select.
- Run `cargo test --bin civgame`.

## Assumptions And Defaults
- Use “best local site” as chosen: optimize within the selected mega-chunk, never across its borders.
- Use `LOCAL_DETAIL_AMP = 0.10` as the default tighter-match value unless testing shows terrain becomes too flat.
- Keep AI faction placement broad to avoid overpacking all factions into the player’s selected cell.
- Diagnostics are tooltip and marker overlays only; no expensive full gameplay-resolution spawn-map render in this pass.
