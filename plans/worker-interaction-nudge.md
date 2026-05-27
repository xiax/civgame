**Worker Interaction Visual Nudge**

**Summary**
- Make workers appear closer to their work targets by nudging only their rendered sprite a few pixels toward `ai.dest_tile` while they are actively working.
- Keep pathfinding, stand-tile reservations, collision, `Transform`, and spatial indexing unchanged.

**Key Changes**
- Update `src/rendering/animations.rs` so `update_animations` computes an additional small offset for `Person` visual children when:
  - `PersonAI.state == AiState::Working`
  - the current task is both `task_interacts_from_adjacent(...)` and `task_is_labor(...)`
- Use a conservative constant, around `5.0px` (`~0.3` tile), normalized from the person’s logical position toward `tile_to_world(ai.dest_tile)`.
- Apply the nudge to every `VisualChild` layer for the person body/clothing/hair, preserving the existing `(0, -8)` base alignment and combat animation behavior.
- Do not apply the nudge to social/play/combat/non-labor interactions, buildings, plants, vehicles, labels, or the entity’s parent `Transform`.

**Test Plan**
- Add focused unit coverage for the pure nudge helper:
  - working adjacent labor task nudges toward the destination
  - idle/seeking workers get zero offset
  - non-labor adjacent tasks get zero offset
  - same-tile or zero-length direction gets zero offset
- Run `cargo test --bin civgame` or a narrower rendering/animation test filter, then `cargo check`.
- Manual sanity pass in sandbox: observe gathering/building/farming/storage work and confirm sprites look closer without agents changing tiles or crowding differently.

**Assumptions**
- The desired change is visual feel only, not stricter routing.
- `5px` is the first-pass tuning value; it should be small enough to avoid crossing tile boundaries while making work contact read better.
