# Objective
Prevent early-era settlements from spawning too close to rivers, ensuring their initial ~10-tile radius fits cleanly on one bank. This avoids the settlement straddling the river before bridge-building technology is available.

# Key Files & Context
- `src/world/terrain.rs`: Controls `RIVER_FEATHER_DIST`, which dictates how far away from a river the `surface_river_distance` field is calculated.
- `src/simulation/person.rs`: Evaluates AI faction starting locations (`score_home_candidate`).
- `src/simulation/region.rs`: Evaluates player faction starting locations (`score_tile`).

# Implementation Steps

1. **Increase River Visibility Range:**
   In `src/world/terrain.rs`, increase `RIVER_FEATHER_DIST` to allow the distance field to stretch far enough to detect rivers from the ideal settlement center distance.
   ```rust
   pub const RIVER_FEATHER_DIST: u8 = 12;
   ```
   *Note: This strictly increases the distance array values and doesn't change `topsoil_kind` bounds, which correctly hardcodes a check for `<= 5`.*

2. **Update AI Faction Spawn Scoring:**
   In `src/simulation/person.rs`, update `score_home_candidate` to strongly prefer distances of 10-12 and heavily penalize distances 0-5.
   ```rust
        let river_score = match chunk_map.river_distance_at(tx, ty) {
            0..=5 => -50,
            6..=9 => 30,
            10..=12 => 60,
            13..=15 => 30,
            _ => 0,
        };
   ```

3. **Update Player Faction Spawn Scoring:**
   In `src/simulation/region.rs`, update `score_tile` similarly so the center-pull fallback logic also adheres to the wider river distance requirement.
   ```rust
        let river_score = match river_d {
            0..=5 => -50,
            6..=9 => 30,
            10..=12 => 60,
            13..=15 => 30,
            _ => 0,
        };
   ```

# Verification & Testing
1. Boot into a new game map.
2. Ensure the player's spawn preview places the initial tile roughly 10-12 tiles away from any visible river.
3. Validate AI factions also spawn ~10-12 tiles away by using the debug viewer.
4. Let the game run for several days to verify that the settlement dynamically expands towards the river (via `water_bonus` in organic settlement generation).