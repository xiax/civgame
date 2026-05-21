# Align Hunger Goals With Food Tasks

## Summary
- Root cause: `SurvivalHungerScorer` emits `AgentGoal::Survive` for empty-handed agents at hunger `> 150`, but the live food dispatchers refuse to act until `EAT_TRIGGER_HUNGER == 180`.
- This creates a `Survival`-class goal with no executable task, so workers can abandon claimed work early and idle until hunger reaches 180.
- This is a contract mismatch, not just tuning. The 150 cliff means “food concern,” while `Survive` currently means “execute emergency food task now.”

## Key Changes
- Gate hunger-driven `Survive` on `needs.hunger >= EAT_TRIGGER_HUNGER as f32` in [goal_scorers.rs](/Users/xiao1/civgame/src/simulation/goal_scorers.rs:674).
- Apply the same rule to the SOLO/fallback cascade in [goals.rs](/Users/xiao1/civgame/src/simulation/goals.rs:43), or retire `HUNGER_FORAGE_REQUIRED` from `Survive` selection.
- Keep anticipatory food work in `GatherFood`, `StockpileFood`, `Farm`, and chief postings. Do not lower task-side eating/acquire gates to 150 unless we intentionally want earlier food consumption.
- Replace hunger literals in scorer/fallback with shared constants or a small helper so goal scoring and HTN gates cannot drift from [htn.rs](/Users/xiao1/civgame/src/simulation/htn.rs:3943).
- Update `src/simulation/CLAUDE.md` with the invariant: survival-maintenance scorers must not emit goals below their dispatcher’s executable gate.

## Test Plan
- Update tests that currently encode the bug: hunger `155`/`175` with no held food should not choose `Survive`.
- Add boundary tests for no-food and held-food cases at `179.9`, `180`, and `200`.
- Add a claimed-worker regression: a worker with a `JobClaim` at hunger `175` keeps/resumes the job; at `180+`, `Survive` may preempt and dispatch `Eat`, `WithdrawFood`, `Scavenge`, `Gather`, or terminal `Explore`.
- Keep existing end-to-end tests for `180+` hungry agents foraging, withdrawing, scavenging, and eating.

## Assumptions
- Desired behavior is that workers do not enter personal emergency `Survive` until food tasks can actually run.
- Hunger preparation before 180 remains a faction/economy responsibility, not a personal `Survive` responsibility.
