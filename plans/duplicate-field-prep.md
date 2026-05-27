# Fix Duplicate Field Preparation

## Summary
- Treat `PrepareField` as exclusive per tile: one worker can prepare a tile at a time, and extra farmers should choose other unprepared tiles.
- Prevent stale duplicate completions from crediting `FieldWork` progress after another worker already prepared the tile.
- Keep parallel farm preparation for large plots, but cap small/near-complete postings so the job system stops over-claiming impossible work.

## Interface And Type Changes
- Add `farm::PrepareFieldReservations` as a Bevy `Resource`, mirroring `plants::PlantingReservations`:
  - `by_tile: AHashMap<(i32, i32), PrepareFieldReservation>`
  - `PrepareFieldReservation { worker: Entity, job_id: JobId, reserved_tick: u64 }`
  - methods: `is_reserved`, `try_reserve(tile, worker, job_id, now)`, `release(tile)`, `release_for_worker(worker)`
- Change `farm::find_nearest_unprepared_in_rect(...)` to accept `&PrepareFieldReservations` and skip reserved tiles.
- No new crates. Existing saves start with an empty reservation resource, so no migration is needed.

## Implementation Changes
- Register `PrepareFieldReservations::default()` in `SimulationPlugin`, next to `FieldTileIndex` / `PlantingReservations`.
- In `htn_prepare_field_dispatch_system`:
  - Request `ResMut<PrepareFieldReservations>`.
  - Find the nearest unprepared, unreserved tile.
  - Reserve the tile before routing with `try_reserve(tile, actor, claim.job_id, now)`.
  - If routing dispatch fails, immediately release the reservation.
  - If no unreserved tile exists, keep the existing eager Farm-claim release behavior.
- In `prepare_field_task_system`:
  - Request `ResMut<PrepareFieldReservations>`.
  - On completion, re-read live tile state and `FieldTileIndex` before doing any work.
  - Credit `record_fieldwork_progress`, grant XP, emit `TileChangedEvent`, and bump nutrients only when the tile still needs preparation: `kind != Cropland || nutrients < EXHAUSTED_FLOOR`.
  - If the tile no longer needs preparation, release the reservation and finish the task without XP or job credit.
  - Release the reservation on every executor-owned exit for a valid `PrepareField { tile }`.
- Add `farm::prepare_field_reservation_gc_system`, scheduled in the same Economy GC block as `planting_reservation_gc_system`:
  - Run daily using `TICKS_PER_DAY`.
  - Drop reservations whose worker despawned, whose current/queued task is no longer `Task::PrepareField { tile }`, or whose reservation exceeded one day.
- Refine `jobs::posting_target_workers` for `JobProgress::FieldWork`:
  - Keep the existing large-plot scaling, capped at 12.
  - Also cap by remaining tile/plant work: `target.saturating_sub(completed).max(1)`.
  - This preserves parallelism on large fields while preventing 3 farmers from claiming a 1-tile prepare job.
- Update `src/simulation/CLAUDE.md` seasonal farming notes to document prepare reservations, stale duplicate no-credit behavior, and the remaining-work worker cap.

## Test Plan
- Add a focused unit test for `find_nearest_unprepared_in_rect`:
  - Given two unprepared tiles, reserving the nearest tile causes the helper to return the next nearest tile.
  - Given all unprepared tiles reserved, it returns `None`.
- Add/extend an integration test around `htn_prepare_field_dispatch_system`:
  - Two workers claiming the same Prepare `FieldWork` posting in a two-tile plot dispatch to different `Task::PrepareField` targets.
  - The reservation map contains both tiles after dispatch.
- Add a stale-completion regression test:
  - Worker A and Worker B are manually set to prepare the same tile.
  - Worker A completes and credits the posting.
  - Worker B completes after the tile is already `Cropland` with sufficient nutrients.
  - Assert Worker B releases/finishes but does not increment `FieldWork.completed`, does not grant duplicate XP, and does not remove the posting early.
- Add a small-posting worker-cap test:
  - `FieldWork { target: 1, completed: 0 }` admits 1 worker.
  - `FieldWork { target: 16, completed: 0 }` still admits 3 workers.
  - `FieldWork { target: 256, completed: 0 }` still admits 12 workers.
  - `FieldWork { target: 16, completed: 15 }` admits 1 worker.
- Run:
  - `cargo test --bin civgame prepare_field`
  - `cargo test --bin civgame posting_target_workers`
  - `cargo test --bin civgame`

## Assumptions
- Preparing a field tile is not intended to be shared progress; faster plot preparation should come from workers spreading over distinct tiles.
- A daily GC is acceptable for rare leaked prepare reservations because dispatch and executor exits cover normal paths, matching the planting reservation model.
- The fix should avoid changing planting, harvesting, plowing, or farm posting creation beyond the FieldWork worker-count cap.
