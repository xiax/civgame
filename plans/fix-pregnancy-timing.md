# Fix Pregnancy Timing And Birth Visibility

## Summary
- Keep gestation at exactly **3 elapsed current seasons**: `3 * TICKS_PER_SEASON` = 15 game days with today’s calendar.
- Fix the likely bug: pregnancy currently counts down only when the mother is non-Dormant and bucket-active, so LOD/camera/bucket scheduling can stretch gestation beyond calendar time.
- Make births visible in the activity log so it is obvious when a child is born.

## Key Changes
- In `src/simulation/reproduction.rs`, make `pregnancy_system` decrement every live `Pregnancy` once per `FixedUpdate`, independent of `LodLevel` and `SimClock::is_active`.
- Preserve the existing `Pregnancy` component shape and `PREGNANCY_TICKS`; update stale comments that still mention `54,000` ticks.
- Allow due pregnancies to give birth even if the mother is Aggregate/Dormant. Spawn the child at the mother’s tile, using loaded `surface_z_at` when valid and falling back to the mother’s `PersonAI.current_z` if the chunk is unloaded.
- Set the newborn’s initial `LodLevel` to the mother’s current LOD so offscreen births do not force Full simulation until normal LOD updates.
- Add `ActivityEntryKind::ChildBorn { child, child_name }` plus clickable activity-log rendering: “mother gave birth to child”.
- Update `src/simulation/CLAUDE.md` and `src/ui/CLAUDE.md` to document calendar-based gestation and the birth log event.

## Tests
- Add a constant test asserting `PREGNANCY_TICKS == TICKS_PER_SEASON * 3`.
- Add a fixture test with a mother that is Dormant and bucket-inactive, `ticks_remaining = 3`; verify pregnancy remains before due and birth happens exactly when due.
- Keep/adjust the existing newborn household inheritance test.
- Add a birth activity-log event test for a one-tick pregnancy.
- Run:
  - `cargo test --bin civgame pregnancy`
  - `cargo test --bin civgame newborn_inherits_household_membership_from_mother`
  - `cargo check`

## Assumptions
- “3 seasons” means elapsed duration, not the third season rollover.
- We are not shortening pregnancy for playtest convenience in this fix.
- The worktree is already dirty, including reproduction/test files, so implementation must preserve existing unrelated edits.
