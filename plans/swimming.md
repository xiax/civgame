# Hybrid Swimming, Energy, And Visible Currents

## Summary
Add swimming as a learnable human skill, powered by a new per-human physiological `Energy` resource. Energy is separate from `Needs.willpower`: willpower remains morale/recreation, while energy represents bodily stamina. All exertion drains energy; sleep restores it quickly, idle/non-labor rest restores it slowly, and swimming is one of the highest-drain activities.

## Public Interfaces And Types
- Add `simulation/energy.rs`:
  - `Energy { current: f32, max: f32 }`, default `255/255`.
  - Helpers: `energy_factor`, `exertion_drain_scale`, `recover_idle`, `recover_sleep`, `can_begin_exertion`.
  - Thresholds: `TIRED`, `EXHAUSTED`, `RECOVERED`, with hysteresis so agents do not flicker between work/rest.
- Extend `Skills`:
  - `SKILL_COUNT = 9`, `SkillKind::Swimming = 8`, `SkillKind::ALL`.
  - Update debug/inspector/cohort skill iteration to use `SkillKind::ALL`, avoiding unsafe transmute.
- Add `WaterCurrentField`:
  - Derived, non-persistent cache keyed by tile.
  - Stores direction, speed, and source classification for rendering and swimming.
- Extend pathing:
  - Add `TraversalLayer::{Dry, Amphibious}` and `TraversalProfile`.
  - `PathRequest`/`PathFollow` carry traversal profile data.
  - Existing dry APIs remain default wrappers.

## Implementation Changes
- Spawn and recovery:
  - Insert `Energy::default()` for all humans in founder spawn, reproduction, sandbox/test fixtures.
  - `sleep_task_system` restores energy with the same bed multiplier used for sleep/willpower.
  - Add idle recovery for awake agents only when not moving, not laboring, not fighting, and not swimming.
  - Low energy slows movement/work; exhausted agents stop accepting noncritical exertion until recovered.

- Exertion drain:
  - Movement drains energy by distance, terrain effort, carried weight, and mounted/unmounted state.
  - Labor drains energy while work progress advances.
  - Combat drains energy on attacks and scales cooldown when tired.
  - Swimming drains energy by water depth, current resistance, carried weight, and skill/strength/constitution.

- Swimming:
  - Add `SwimmingState { wet_ticks, exhausted_ticks, last_xp_tick, last_safe_tile }`.
  - Swimming speed/control combines `SkillKind::Swimming`, `Stats.strength`, `Stats.constitution`, current energy, load, and current vector.
  - Swimming grants XP while in water, with extra XP for meaningful current resistance.
  - Fatigue-first risk: energy loss and slowdown happen first; drowning/injury only starts after sustained exhaustion in deep or strong-current water.
  - Emergency behavior retargets exhausted swimmers to the nearest reachable bank.

- Pathfinding:
  - Dry traversal still treats `Water`/`River` as impassable.
  - Amphibious human traversal can enter water-surface swim nodes, but mounted humans and animals stay dry-only.
  - Water path cost includes estimated swim time, projected energy cost, current assist/resistance, depth, and contiguous swim distance.
  - Bridges and dams remain dry routes and should usually be preferred.
  - Full AI routing may choose swimming when cheaper, but energy/risk costs prevent casual broad-lake shortcuts.

- Current field and visuals:
  - Build `WaterCurrentField` from loaded river topology, water depth, source rate, and runtime water-surface gradients.
  - River channels get directional flow from river polylines/discharge; runtime dam/pool flows derive from local height gradients; still lakes are near calm.
  - Add translucent flow streak/chevron overlay sprites on visible wet tiles, with rotation and animation speed from current vector.
  - Add hover/debug display for water depth and current speed/direction.

- UI and docs:
  - Inspector shows Energy beside Needs/Stats.
  - Hover can show “Energy” for humans and “Current” for wet tiles.
  - Update `AGENTS.md`, `src/simulation/CLAUDE.md`, `src/pathfinding/CLAUDE.md`, `src/world/CLAUDE.md`, and `src/ui/CLAUDE.md`.

## Test Plan
- Energy tests:
  - New humans spawn with full energy.
  - Sleep restores energy faster than idle; beds improve sleep recovery.
  - Labor, movement, combat, and swimming drain energy.
  - Exhaustion slows work/movement and blocks new noncritical exertion until recovered.

- Swimming/pathing tests:
  - Dry pathing still rejects water.
  - Amphibious human pathing can cross narrow rivers.
  - Weak/tired/encumbered swimmers avoid or fail risky crossings.
  - Skilled/strong swimmers cross faster and lose less energy.
  - Drowning damage only begins after exhaustion grace period.

- Current/render tests:
  - Current field is deterministic.
  - Rivers produce downstream vectors; calm lakes produce near-zero vectors.
  - Runtime water slope creates current toward lower surface.
  - Flow overlays spawn/despawn with chunk load/unload and wet/dry tile changes.

- Verification:
  - `cargo test --bin civgame`
  - Manual `cargo run -- --sandbox` smoke test: order humans across water, inspect Energy/Swimming XP, and verify flow cues render.

## Assumptions
- Energy is general stamina, not swimming-only.
- Energy recovers through sleep and idle rest; no separate Rest task in v1.
- Swimming is innate and not tech-gated.
- Only humans swim in this pass.
- Boats, rescues, aquatic animals, underwater work, and save-persisted current vectors are out of scope.
