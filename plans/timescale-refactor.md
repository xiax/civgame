# Timescale Refactor: 7200 Ticks Per Day

## Summary
- Change the canonical game day from `3600` to `7200` fixed ticks.
- Keep the fixed sim cadence at `20 Hz`; at normal speed, one game day becomes 360 real seconds.
- Preserve per-game-day balance: hunger, thirst, sleep, willpower, animal needs, and similar continuous physiology should accumulate the same total amount per game day as before.
- Make future timescale edits mostly require changing `TICKS_PER_DAY` in one place.

## Time API Changes
- In `src/world/seasons.rs`, make this module the only source of calendar/timescale constants:
  - `SIM_TICKS_PER_SECOND: u32 = 20`
  - `FIXED_TIMESTEP_SECS: f32 = 1.0 / SIM_TICKS_PER_SECOND as f32`
  - `TICKS_PER_DAY: u32 = 7_200`
  - `DAYS_PER_SEASON: u32 = 5`
  - `DAYS_PER_YEAR: u32 = DAYS_PER_SEASON * 4`
  - `TICKS_PER_SEASON: u32 = TICKS_PER_DAY * DAYS_PER_SEASON`
  - `TICKS_PER_YEAR: u32 = TICKS_PER_DAY * DAYS_PER_YEAR`
  - `SECONDS_PER_GAME_DAY: f32 = TICKS_PER_DAY as f32 * FIXED_TIMESTEP_SECS`
- Add helper functions in `seasons.rs`:
  - `ticks_per_days(days: u32) -> u32`
  - `ticks_per_days_u64(days: u64) -> u64`
  - `per_game_day_rate(amount_per_day: f32) -> f32`, returning units per real second for systems using `time.delta_secs()`.
- Update `Calendar::default`, rollover tests, and comments to use these shared constants.

## Continuous Balance Refactor
- In `src/simulation/needs.rs`, replace per-second constants with per-game-day target constants, then derive runtime rates with `per_game_day_rate`.
  - Preserve current daily totals from the old 3600-tick day:
    - Hunger: `360` per game day.
    - Thirst: `720` per game day.
    - Sleep: `216` per game day.
    - Shelter/safety/reproduction/social/willpower use their current old-day totals.
  - Keep thresholds such as `EAT_TRIGGER_HUNGER`, `THIRST_TRIGGER`, `SLEEP_WAKE_THRESHOLD`, and utility curves unchanged.
- Apply the same pattern to:
  - `src/simulation/sleep.rs`: sleep and willpower recovery rates.
  - `src/simulation/tasks.rs`: play/social recovery rates.
  - `src/simulation/animals.rs`: hunger, thirst, sleep, reproduction, sickness decay, sleep recovery.
  - `src/simulation/energy.rs`: labor drain, idle recovery, sleep recovery.
- Do not scale movement speeds, per-tile energy movement cost, attack costs, construction work ticks, task durations, or combat cooldowns unless they already explicitly use game-day constants.

## Hardcoded Day Cleanup
- Replace duplicated `3600` day literals with `crate::world::seasons::TICKS_PER_DAY` or helper constants in:
  - `src/ui/activity_log.rs`
  - `src/net/bootstrap.rs`
  - `src/net/protocol.rs`
  - `src/simulation/technology_adoption.rs`
  - `src/simulation/shared_knowledge.rs`
  - `src/simulation/memory.rs`
  - `src/simulation/jobs.rs` tablet posting cadence
  - `src/simulation/plants.rs` planting reservation GC
  - `src/simulation/knowledge.rs` study ticks per complexity
  - `src/simulation/utility_curves.rs` age/year placeholders
- Update comments/tests that mention specific old boundaries like `0, 3600, 7200` so they describe `TICKS_PER_DAY` boundaries instead.

## Documentation
- Update root `AGENTS.md`:
  - Fixed update remains `20 Hz`.
  - A day is now `7200` ticks.
  - Continuous physiology is calibrated per game day through `world::seasons` helpers.
- Update `src/simulation/CLAUDE.md`:
  - Replace old `180 real sec` need calibration with the new 360-second day.
  - Replace explicit `3600` daily cadence notes with `TICKS_PER_DAY`.
  - Note that future timescale changes should adjust `world::seasons::TICKS_PER_DAY` and avoid new local day constants.

## Test Plan
- Add or update `world::seasons` tests for:
  - `TICKS_PER_DAY == 7_200`.
  - `SECONDS_PER_GAME_DAY == 360.0`.
  - `TICKS_PER_SEASON == 36_000`.
  - `TICKS_PER_YEAR == 144_000`.
  - `per_game_day_rate(360.0) == 1.0` under the new timescale.
- Add focused physiology tests where practical:
  - One full game day of person hunger accrues about `360` before clamping.
  - One full game day of sleep need accrues about `216`.
  - One full game day of animal thirst/reproduction matches the old daily totals.
- Run:
  - `cargo test --bin civgame world::seasons`
  - `cargo test --bin civgame simulation::needs`
  - `cargo test --bin civgame simulation::animals`
  - `cargo test --bin civgame`

## Assumptions
- Preserve daily balance is the chosen rule: a 7200-tick day should not make people twice as hungry per game day.
- Network tick cadence remains `50 ms` and must continue matching the fixed update rate.
- Existing saved/network calendar wire data can still carry `ticks_per_day`; tests should use the shared constant rather than literals.
- No new crates are needed.
