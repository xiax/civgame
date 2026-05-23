**Lookout And Base Vision**

**Summary**
Add a reusable vision-source layer so normal agents keep 15-tile vision, active lookouts use 50-tile vision, and player-owned bases reveal by era: Paleolithic 16, Mesolithic 20, Neolithic 25, Chalcolithic 30, Bronze Age 40. Lookout is not hard-gated by elevation; terrain LOS naturally limits low-ground scans. Own-faction constructed walls and closed doors will not block fog/vision scans, while natural rock, enemy structures, combat LOS, and sound LOS remain unchanged.

**Key Changes**
- Centralize vision constants and helpers:
  - `STANDARD_VIEW_RADIUS = 15`
  - `LOOKOUT_VIEW_RADIUS = 50`
  - `base_vision_radius_for_era(Era) -> 16/20/25/30/40`
  - faction-aware LOS helper used only by fog/resource vision.
- Add lookout state and task plumbing:
  - `ActiveLookout { anchor_tile, anchor_z, radius, expires_tick }`
  - `TaskKind::Lookout` and `Task::Lookout`
  - `PlayerCommand::Lookout { tile, z }`
  - manual lookouts hold indefinitely until superseded or physically moved; autonomous lookouts last 120 fixed ticks.
- Wire player UI:
  - Add “Lookout here” to the right-click tile actions for passable tiles.
  - The command routes the selected worker(s) to the tile, starts lookout on arrival, and keeps the agent standing there.
- Wire autonomous behavior:
  - Existing Explore fallbacks can transition into a short lookout pause after arrival, so scouts/foragers naturally use vantage points while exploring.
  - The scan radius lives on `ActiveLookout`, so future telescopes can extend it by changing the radius source rather than rewriting fog/vision loops.
- Wire base vision:
  - Player `Settlement` and `Camp` anchors contribute owner-faction vision to `FogMap`.
  - Base rays use the same faction-aware LOS so own walls/doors do not black out the inside of a base.
- Track constructed wall ownership:
  - Extend `Wall` with owner faction metadata for built/seeded walls; generated natural wall entities remain unowned and continue blocking vision.

**Tests**
- LOS tests: own wall/closed own door ignored by vision LOS, foreign/natural wall still blocks, existing combat `has_los` behavior unchanged.
- Vision tests: normal agent cannot see a resource at 40 tiles; active lookout can; moving off the anchor removes lookout and restores normal range.
- Player command tests: `Lookout` routes, starts `ActiveLookout`, holds the command while standing, and clears when superseded/moved.
- Base vision tests: era radii match 16/20/25/30/40 and own structures do not block the base reveal.
- Explore tests: an Explore arrival starts a 120-tick autonomous lookout pause, then resumes autonomy.
- Run `cargo test --bin civgame` and `cargo check`.

**Assumptions**
- “Anything can trigger” means no explicit elevation eligibility check.
- “Broken when they move” means the active lookout ends when the agent’s tile or Z differs from its anchor.
- “Vision through faction structures” applies to fog/resource vision for the owner faction only, not to combat targeting or sound propagation.
- Documentation should be updated in the simulation/UI notes after implementation.
