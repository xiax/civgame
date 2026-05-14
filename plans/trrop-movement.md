# Multi-Unit Military Move Formation

## Context

When the player issues `MilitaryMove` on multiple drafted units, the current dispatcher at `src/simulation/player_command.rs:913-942` calls `assign_task_with_routing(..., target: tile, ...)` once per actor with the **identical clicked tile** for every selected unit. All actors share `ai.dest_tile`, so they pile onto the rally tile and `military_task_system` (`src/simulation/military.rs:99-103`) flips each to `Idle` the moment one of them lands there. The fix is to expand the single clicked tile into per-actor slot tiles around an anchor, route each actor to its own slot, and gate completion on per-actor arrival.

This pass is scoped to the bug fix: no squad UI, no persistent squad/registry types, no changes to `MilitaryAttack`.

## Design

### Slot generation — pure planner

New `src/simulation/military/formation.rs` (promote `military.rs` → `military/` module; current contents move to `military/mod.rs`). Single public entry:

```rust
pub fn plan_compact_ring(
    anchor: (i32, i32),
    z: i8,
    n: usize,
    is_passable: impl Fn((i32, i32)) -> bool,
) -> Vec<(i32, i32)>
```

Walks Chebyshev rings outward from `anchor` in deterministic cardinal-then-diagonal order (the same pattern already used inline by `find_clear_tile_in_zone` at `construction.rs:1354-1422`), yields up to `n` passable tiles. Anchor itself is slot 0. Pure; no ECS access.

`is_passable` closure passed in by the caller, built from `ChunkMap::is_passable`; the closure also returns `false` for tiles occupied by non-selected agents (looked up via `SpatialIndex`) and for `DoormatReservations`. Selected actors' own current tiles are treated as passable so a tight group can reshuffle without self-blocking.

### Actor-to-slot assignment — greedy nearest

In the dispatcher: sort selected actors by Chebyshev distance from `anchor` (closer first). For each actor in that order, pick the unassigned slot minimising Chebyshev distance from the *actor*. This is O(N·M) at N ≤ 32, simpler than Hungarian, and avoids the crossing-paths failure mode of naive zip-assignment.

### Where slot/group metadata lives

New component, transient — lives only while the formation move is active:

```rust
#[derive(Component, Debug, Clone, Copy)]
pub struct MilitaryFormationSlot {
    pub anchor: (i32, i32),
    pub slot_index: u8,
    pub group: u32,
}
```

- `group` is allocated from a new `Resource MilitaryFormationGroupGen { next: u32 }` (wrapping `u32`), one id per multi-actor dispatch.
- The slot **tile** itself doesn't need to live on the component: `assign_task_with_routing(..., target: slot_tile, ...)` writes it to `ai.dest_tile`, so `military_task_system`'s existing arrival check (`(cur_tx, cur_ty) == (dest.0, dest.1)`) works unmodified.
- Inserted in the dispatcher after successful routing. Removed by `player_command_lifecycle_system` when the per-actor `Commanded` reaches a terminal status, and on `dispatch_player_command_system`'s pre-overwrite cleanup so a fresh `MilitaryMove` clears stale slots.

`WalkReason::MilitaryMove` and `Task::WalkTo { tile, z, why }` are **unchanged** — slot info rides on the component, not the task variant. Reduces blast radius on `goal_dispatch_system` / `record_abandoned_method_system` and keeps `Task` enum size stable.

### Dispatcher changes (`player_command.rs:913-942`)

Today the `MilitaryMove` arm runs per-actor inside `dispatch_one`. The fix needs cross-actor coordination, so split into two phases:

1. **Pre-dispatch (`drain_player_command_events_system` or a new `expand_military_move_system` running just before `dispatch_player_command_system`)**: when an inbound `MilitaryMove` event carries `actors.len() > 1`, allocate a `group` id, call `plan_compact_ring(...)`, run greedy assignment, attach `(actor, slot_tile)` to a side-table (e.g. `Resource PendingFormationSlots: AHashMap<(u32, Entity), (i32,i32)>`) keyed by `(command_id, actor)`. Actors that received no reachable slot get marked `Failed(Unreachable)` immediately — they never enter dispatch.
2. **Per-actor dispatch arm**: read `PendingFormationSlots.get(&(command_id, actor))`, use the slot tile (falling back to `tile` for single-actor moves), call `assign_task_with_routing(..., target: slot_tile, ...)`, insert `MilitaryFormationSlot`. Anchor (not slot) is what gets registered in `ActiveRallyPoints` so the hotspot flow field is shared by the whole group.

Side-table cleared once the command is fully dispatched (drain at end of `dispatch_player_command_system` tick, or use `Local<...>` since both systems run in the same `Input → ParallelB` sequence).

Single-unit `MilitaryMove` short-circuits the planner and behaves exactly as today — no slot component, no group id, anchor == destination.

### Reachability

Reuse `assign_task_with_routing`'s existing reachability check (`src/simulation/tasks.rs:544-646`) — it already runs `chunk_connectivity`-based same-component validation with an in-chunk fallback. The planner only needs `is_passable` (Step 1) plus optional same-chunk-component filtering against the actor's chunk; do **not** run a fresh A* per (actor, slot). For each (actor, assigned slot), the dispatcher's existing `routed = assign_task_with_routing(...)` return value is the authoritative reachability gate. If it returns false, that actor returns `DispatchOutcome::Failed(CommandFailure::Unreachable)` — other actors in the group continue normally.

### Arrival and lifecycle

`military_task_system` (`military.rs:40-168`) is unchanged — arrival fires when `(cur_tx, cur_ty) == ai.dest_tile`, and `ai.dest_tile` now holds the slot tile.

`player_command_lifecycle_system`'s `MilitaryMove` arm currently does a Chebyshev-arrival check on `tile` (the anchor). Update it to read `MilitaryFormationSlot.slot_tile` if present (or recover from `ai.dest_tile` since the slot tile lives there), so completion gates on slot arrival rather than anchor proximity. `reap_terminal_commands_system` already strips `Commanded`; the `MilitaryFormationSlot` component is removed there too (via the lifecycle arm or an `on_remove` hook on `Commanded` for `MilitaryMove`-bearing entities).

### Supersede

A new `MilitaryMove` from the same player to the same actor while a formation slot is active: the existing `dispatch_player_command_system` pre-overwrite path already calls `aq.cancel()` and rewrites `Commanded`. Add a `commands.entity(actor).remove::<MilitaryFormationSlot>()` next to that path. The new dispatch then runs through the planner cleanly.

## Files to touch

- `src/simulation/military.rs` → split into `src/simulation/military/mod.rs` (existing `ActiveRallyPoints`, `military_task_system`, `expire_rally_points_system`) + `src/simulation/military/formation.rs` (new `plan_compact_ring`, `MilitaryFormationSlot`, `MilitaryFormationGroupGen`, `PendingFormationSlots`).
- `src/simulation/player_command.rs:913-942` — replace per-actor identical-target loop with planner-then-assign flow; add `expand_military_move_system` (or fold into `drain_player_command_events_system`) ahead of `dispatch_player_command_system`; clean up `MilitaryFormationSlot` on overwrite at the `Commanded` rewrite path.
- `src/simulation/player_command.rs::player_command_lifecycle_system` (the `MilitaryMove` arm) — read slot tile for completion check; remove `MilitaryFormationSlot` at terminal status.
- `src/simulation/mod.rs` — register the new resources (`MilitaryFormationGroupGen`, `PendingFormationSlots`) and system ordering for `expand_military_move_system` (before `dispatch_player_command_system` in `Input` / `ParallelB`).
- `src/simulation/CLAUDE.md` — document `MilitaryFormationSlot`, the planner, anchor-vs-slot semantics, and the explicit non-change to `MilitaryAttack`.

Reused pieces: Chebyshev spiral pattern from `find_clear_tile_in_zone` (`construction.rs:1354-1422`); `assign_task_with_routing` (`tasks.rs:544-646`); `CommandFailure::Unreachable` (`player_command.rs:198-208`); `ActiveRallyPoints` (`military.rs`).

## Test plan

Unit tests (in `formation.rs`, pure function — no `App`):

- `plan_compact_ring(anchor, z, 1, all_passable) == vec![anchor]`.
- `plan_compact_ring(anchor, z, 8, all_passable)` — 8 unique tiles, all within Chebyshev 1 of anchor (anchor + 8 neighbours), deterministic order across runs.
- `plan_compact_ring(anchor, z, 20, all_passable)` — 20 unique tiles, monotonically non-decreasing Chebyshev radius.
- Blocked terrain: every cardinal of anchor returns `false` → planner falls through to ring 2 without duplicating slots; returned slots all pass the closure.
- Insufficient passability: closure returns `false` for everything beyond 3 tiles → result is the reachable subset, length < n.
- Greedy assignment determinism: same actor positions + same anchor → same `(actor, slot)` mapping.

Integration tests via `TestSim` (the `Without<Drafted>` filter is gone from `nomad_migration_dispatch_system` per the CLAUDE.md note — drafted units run normally):

- 8 drafted units co-located at one tile, issue `MilitaryMove` → after one tick of dispatch, all 8 have distinct `ai.dest_tile`s within Chebyshev 2 of the anchor, all carry `MilitaryFormationSlot` with the same `group`, distinct `slot_index`.
- Walk to completion (`tick_n(...)` until all eight reach their slots) → all eight `Commanded` reach `CompletionStatus::Completed`, all `MilitaryFormationSlot` removed, no unit shares a tile with another.
- Single drafted unit, issue `MilitaryMove` → behaviour byte-equal to today (no `MilitaryFormationSlot` inserted; `dest_tile == anchor`).
- Issue a second `MilitaryMove` to a different anchor mid-walk → old `MilitaryFormationSlot`s removed, new `group` id allocated, new slot assignments don't reference stale anchor.
- Anchor on a tile in a one-tile pocket surrounded by walls (M < N) → first actor lands on the anchor, remaining actors return `Failed(Unreachable)`, no panic, dispatched actors complete normally.

Manual verification (`cargo run`):

- Draft ~8 hunters, click a tile, observe them spread into a compact ring rather than stacking.
- Same scenario near a wall — units fill the reachable side, the rest fail cleanly without freezing.
- Issue a fresh order while the previous formation is still walking — units retarget without hanging.

## Out of scope (deliberate)

- `SquadId` / `SquadMember` / `SquadRegistry` persistent types and the squad UI/hotkey layer. The current bug needs transient per-command state, not persistent squad entities; adding unused types now violates the "no abstractions beyond what the task requires" rule. Re-add when a real squad feature lands.
- `MilitaryAttack` slot logic. The attack arm at `player_command.rs:944-969` is a distinct dispatch path; it should *not* spread units across slots around a target. Untouched in this pass.
- Tactical formation pickers (line, wedge, skirmish). `CompactRing` is the only formation; the planner takes `n` and an anchor — adding a `FormationKind` enum is trivial later but not needed now.
- `UShape` building-template parallels. Slot generation here is independent of structure footprints.
