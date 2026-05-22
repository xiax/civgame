# Fix Stale Wall Entity Panic

## Summary
- Root cause: `spawn_chunk_sprites` records `Wall` entities in `TileSpriteIndex.by_chunk`, so chunk unload despawns them while `WallMap` intentionally keeps their entity IDs. Later, `refresh_changed_tiles_system` tries to despawn that stale `WallMap` entity and panics.
- Preserve the intended invariant from the repo docs: walls survive chunk streaming; transient terrain sprites unload with chunks.

## Key Changes
- In `src/world/chunk_streaming.rs`, stop adding `Wall` entities to `TileSpriteIndex.by_chunk` in both wall spawn paths:
  - chunk sprite load path
  - tile refresh path that creates a fallback natural-bedrock wall
- Keep `TileSpriteIndex.by_chunk` limited to unloadable terrain tile sprites and elevation skirts.
- Make wall despawn during tile refresh defensive:
  - remove the `WallMap` entry when the tile is no longer `TileKind::Wall`
  - call `commands.get_entity(wall_entity)` before `despawn_recursive()` so a stale entity ID cannot panic
  - still remove any legacy occurrence from the chunk entity list
- Optionally apply the same `commands.get_entity` guard to chunk unload’s indexed sprite despawns, so stale rendering indexes fail closed instead of crashing.

## Test Plan
- Add a focused regression test around `chunk_streaming.rs`:
  - create a wall entity in `WallMap`
  - simulate it being present in a chunk sprite list
  - despawn/unload the chunk path
  - emit a `TileChangedEvent` where the tile is now non-wall
  - assert `refresh_changed_tiles_system` does not panic and removes the stale `WallMap` entry
- Add/adjust a unit assertion that wall entities spawned for `TileKind::Wall` are not registered in `TileSpriteIndex.by_chunk`.
- Run `cargo test --bin civgame`.

## Assumptions
- `WallMap` is intended to be durable across chunk streaming, matching the existing comments and `src/simulation/CLAUDE.md`.
- This fix targets the panic without changing construction, deconstruction, mining, or wall material selection behavior.
