# Player-Locked Nomad Migration

## Summary
Make player-driven packed migration strict by default: after `Pack Camp`, workers wait or follow explicit player orders instead of choosing autonomous goals. Add a deliberate “autonomy” toggle so the player can temporarily allow the old forage/basic-needs behavior.

## Key Changes
- Add `PackedMigrationAutonomy::{Hold, Forage}` to faction state, defaulting to `Hold`.
- Add `PlayerCommand::SetPackedAutonomy { mode }`, routed through `PendingCampOps` and applied in Sequential like migration intent changes.
- On every `PackCamp`, reset the faction’s packed autonomy to `Hold`.
- Change packed-state goal gating:
  - For player nomads with `Hold`, cancel non-command autonomous tasks and set idle workers to `FollowingPlayerCommand` / “Awaiting Orders.”
  - Allow pack labor, direct `Commanded` orders, `PitchCamp`, and explicit manual `SendScout`.
  - For player nomads with `Forage`, preserve the current packed behavior: food/sleep/social/play/defense/care/scout may dispatch.
  - Leave AI nomad autopilot behavior unchanged.
- Add HUD and Migration panel controls showing `Hold` vs `Forage`; clicking toggles via `PlayerCommand::SetPackedAutonomy`.
- Update migration UI/help text and repo notes to remove the old “free-form workers may forage by default” wording.

## Test Plan
- Packed player nomads in `Hold` do not turn hunger/idle state into `GatherFood`, `Survive`, `Sleep`, `Socialize`, or wandering tasks.
- Packed `Hold` workers still obey `Move` orders and return to waiting after the order completes.
- `PackCamp` resets a prior `Forage` setting back to `Hold`.
- Toggling to `Forage` allows the previous autonomous packed behavior.
- Manual `SendScout` still dispatches a scout while the rest of the band remains held.
- Run `cargo test --bin civgame`.

## Assumptions
- “Strict Hold” blocks autonomous movement even for hunger/sleep; the player uses direct orders or the `Forage` toggle to release that behavior.
- `PackCamp` itself counts as explicit permission for shelter-dismantling labor.
- No new crates are needed.
