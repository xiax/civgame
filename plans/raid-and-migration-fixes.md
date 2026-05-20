# Raid, Migration, and Collapse Realism Pass — Revised

## Context

The existing plan at `civgame/plans/raid-and-migration-fixes.md` correctly diagnoses three single-tick "desperation flip" failure modes (raids triggering on transient food=0; nomads migrating on weak knowledge alone; settled bands collapsing on any one weak signal) and proposes sustained-pressure decisions. Codebase verification confirms every claim in the original plan is accurate:

- `raid.rs` `faction_decision_system` triggers on `food_stock == 0 && avg_hunger >= 80` with **no cooldown** and treats arrival as exact tile match (`ai.target_tile == enemy_home`).
- `nomad.rs` `pick_migration_candidates` reads `WildHerdRegistry.herds.values()` **omnisciently** (no knowledge gate) and `score_danger` reads `MemoryKind::Prey` clusters as a proxy for predator danger — a conceptual misuse.
- `sedentary_collapse.rs` collapses on **any one** of food/pop/shelter failing for ~1 season, with bed counts via a 12-tile `BedMap` radius scan.

User feedback adds significant scope: **raids must include a preparation phase** that crafts and equips weapons before marching, and raid party selection must rank by **physical capability** (health, not hunger). The old `HUNGER_RAID_CEILING = 120` participation gate is the wrong dimension — starved-but-able raiders should still go; broken-bodied raiders should not.

This revised plan keeps the structure of the original (sustained pressure, real cooldowns, fix arrival/danger/herd bugs) and layers in a faction-level raid state machine that reuses existing crafting / job / equip infrastructure rather than inventing parallel systems.

## Design Decisions (from user)

- Raid trigger: **AND** (sustained food deficit AND sustained hunger), both ≥ 2 days. No emergency override.
- Nomad trigger: **strict AND** (food deficit AND weak local opportunity). No hunger override.
- Raid participation gate: **physical capability**, not hunger. Prioritize most capable first. Add a **preparation phase** (craft + equip weapons) before march.

## Files to Modify

| File | Role |
|------|------|
| `src/simulation/faction.rs` | Add `RaidPhase` enum + lifecycle fields on `FactionData`; per-faction migration cooldown bookkeeping |
| `src/simulation/raid.rs` | Sustained-pressure trigger, phase state machine, preparation orchestration, capability drafting, arrival/cooldown/reserve fixes |
| `src/simulation/goals.rs` | Replace hunger ceiling with capability gate; gate raid goal on `RaidPhase::Marching|Engaged` |
| `src/simulation/htn.rs` | New `PrepareForRaidMethod` (withdraw → equip); keep `RaidEnemyHomeMethod` for march phase |
| `src/simulation/typed_task.rs` / `src/simulation/items.rs` | Per-raider `last_steal_tick` field on `PersonAI` (or new component); reuse existing `Task::Equip` |
| `src/simulation/jobs.rs` | New helper `post_raid_prep_craft_jobs` that emits `JobKind::Craft` for weapons/armor with `JobReason::RaidPrep` |
| `src/simulation/nomad.rs` | Strict-AND trigger, knowledge-gated herd visibility, `score_danger` purge, ocean/impassable early-reject, survey-no-candidate retry |
| `src/simulation/sedentary_collapse.rs` | Combined-failure trigger over 2 seasons, settlement-owned bed lookup with 32-tile fallback, post-collapse state reset |
| `src/simulation/knowledge.rs` | (Read-only) confirm `SharedKnowledge` has herd-cluster support; add `KnowledgeKind::Herd` cluster if absent (see Open Questions) |
| `src/simulation/CLAUDE.md` | Document the new raid phase machine and updated trigger semantics |

## Raid System

### State Machine

Replace the bare `raid_target: Option<u32> + under_raid: bool` pair with an explicit phase enum on `FactionData`:

```rust
pub enum RaidPhase {
    Idle,
    Preparing { since_tick: u32, target: u32 },
    Marching  { since_tick: u32, target: u32 },
    Engaged   { since_tick: u32, target: u32 },
    Cooldown  { until_tick: u32 },
}
```

Keep `raid_target: Option<u32>` and `under_raid: bool` as derived projections (set from the phase) so existing HTN/goal call sites compile unchanged. Add:

- `raid_phase: RaidPhase` (source of truth)
- `raid_stolen_food: u32` (telemetry / end-condition; reset on `Idle`)
- `raid_party: SmallVec<[Entity; 16]>` (selected raiders; cleared on `Idle`)

`under_raid` is set by `raid_detection_system` on the *target* faction whenever any `raid_party` member is within 30 tiles; cleared when no raiders are within 30 for ≥ `TICKS_PER_DAY / 4`.

### Constants (`src/simulation/raid.rs`)

```rust
pub const RAID_TRIGGER_DAYS: u32 = 2;
pub const RAID_COOLDOWN_DAYS: u32 = 10;
pub const RAID_MIN_AVG_HUNGER: f32 = 140.0;
pub const RAID_CANCEL_FOOD_PER_MEMBER: f32 = 3.0;
pub const RAID_RIVAL_RESERVE_PER_MEMBER: f32 = 5.0;
pub const RAID_MAX_PARTY_FRAC: f32 = 0.35;
pub const RAID_MAX_PARTY_ABS: usize = 8;
pub const RAID_STEAL_COOLDOWN_TICKS: u32 = TICKS_PER_DAY / 8;
pub const RAID_MAX_TRAVEL_TILES: i32 = 500;
pub const RAID_ENGAGED_TIMEOUT_TICKS: u32 = TICKS_PER_DAY * 2;
pub const RAID_PREP_TIMEOUT_TICKS: u32 = TICKS_PER_DAY * 3;
pub const RAID_CAPABILITY_MIN_HEALTH_FRAC: f32 = 0.5;
pub const RAID_DESIRED_WEAPONS_PER_RAIDER: u8 = 1;
```

### Sustained-Pressure Trigger (in `faction_decision_system`)

Track per-faction streak counters (in `FactionData`):

- `food_deficit_streak_tick: u32` — last tick at which `food_total >= members * 1.0` was true; deficit started at `food_deficit_streak_tick` and the duration is `now - food_deficit_streak_tick`.
- `hunger_crisis_streak_tick: u32` — same pattern, gated on `avg_hunger >= RAID_MIN_AVG_HUNGER`.

Each tick the system runs, update both streaks (reset to `now` when the condition is false). Trigger raid only when:

```
phase == Idle
&& now >= cooldown_until
&& (now - food_deficit_streak_tick)  >= RAID_TRIGGER_DAYS * TICKS_PER_DAY
&& (now - hunger_crisis_streak_tick) >= RAID_TRIGGER_DAYS * TICKS_PER_DAY
```

(AND, per user.) On trigger, pick target and enter `Preparing`.

### Target Filter

Exclude all of:

- `SOLO_FACTION_ID`, self
- Households where one faction is parent of the other (`FactionData.parent_faction`)
- Same root (walk parent chain on both, compare roots)
- Subordinate/overlord pairs (`subordinates`, `overlord_id`)
- Targets with `food_total <= target_members * RAID_RIVAL_RESERVE_PER_MEMBER`
- Targets whose home is beyond `RAID_MAX_TRAVEL_TILES` chebyshev from our home

Among the remainder, pick the nearest (chebyshev). No target → reschedule check next tick; do **not** enter `Preparing`.

### Preparation Phase

On `Idle → Preparing { target }`:

1. **Select raid party** by capability rank (see below) up to `party_cap = min(RAID_MAX_PARTY_ABS, ceil(members * RAID_MAX_PARTY_FRAC))`. Store in `FactionData.raid_party`.
2. **Audit weapons in faction storage + party inventories**. For each raider missing a usable weapon, post a `JobKind::Craft` order for the best-available craftable weapon (Spear if `HUNTING_SPEAR` known; fallback Bow; fallback nothing). Mark posting with new `JobReason::RaidPrep` so it can be cancelled cleanly on abort.
3. **Each tick in `Preparing`**:
   - When a raider is co-located with faction storage holding an unequipped weapon, the new `PrepareForRaidMethod` (HTN) expands to `Task::WithdrawFromStorage { weapon_id } → Task::Equip { MainHand, weapon_id }`. (Re-use `equip_task_system`; no executor changes.)
   - Track readiness: a raider is "ready" if `Equipment[MainHand]` is a `ResourceClass::Weapon` OR carrying one and within 1 tile of storage (final equip is fast).
   - Transition `Preparing → Marching` when **all** raiders are ready, OR when `now - since_tick >= RAID_PREP_TIMEOUT_TICKS / 2` AND at least half the party has weapons (don't wait forever).
4. **Abort prep** (back to `Cooldown { until = now + RAID_COOLDOWN_DAYS * TICKS_PER_DAY / 2 }`, half-length) when:
   - `now - since_tick >= RAID_PREP_TIMEOUT_TICKS` and < 25% ready (genuinely can't equip).
   - Food/hunger crisis resolves (food ≥ `members * RAID_CANCEL_FOOD_PER_MEMBER` including hand inventory).
   - Target faction collapses, merges, or moves beyond `RAID_MAX_TRAVEL_TILES`.
   - On abort, cancel `JobReason::RaidPrep` postings via `jobs.cancel_by_reason(faction, RaidPrep)`.

### Capability Drafting

Replace the `goal_update_system` raid gate (`needs.hunger < HUNGER_RAID_CEILING`) with a capability score. For each non-chief, non-drafted member of the faction:

```rust
fn raid_capability(person, health, body, stats, equipment, inventory) -> Option<f32> {
    if body.is_dead() || health.current * 2 < health.max { return None; }      // > 50% HP
    if body.parts[Limb::Torso].current * 2 < body.parts[Limb::Torso].max { return None; }
    let hp_pct = health.current as f32 / health.max as f32;                     // 0.5..1.0
    let str_norm = (stats.str as f32 - 3.0) / 15.0;                             // 3d6 → ~0..1
    let armed = if equipment_has_weapon(equipment) { 1.0 }
                else if inventory_has_weapon(inventory) { 0.6 }
                else { 0.0 };
    Some(0.5 * hp_pct + 0.3 * str_norm + 0.2 * armed)
}
```

Members returning `None` are unfit; among the rest, take the top `party_cap` by score (deterministic tie-break on entity id). Chief is excluded unless the party would otherwise be < 2 members. Pre-existing `Drafted` (player-controlled) members are always excluded.

The party is fixed in `FactionData.raid_party` at `Idle → Preparing` and is the **only** set whose `goal_update_system` may assign `AgentGoal::Raid`. Non-party members continue normal foraging/work.

### March & Engagement

- `Marching`: `AgentGoal::Raid` becomes active for party members. Existing `RaidEnemyHomeMethod` expands to `Task::Raid { dest = enemy_home }`. Movement is unchanged.
- Transition `Marching → Engaged` when any raider's `chebyshev(current_tile, enemy_home) <= 1` (this fixes the existing arrival bug).
- `raid_execution_system` rewrites:
  - Arrival check uses chebyshev ≤ 1 instead of `target_tile == enemy_home`.
  - Per-raider steal cooldown: store `last_steal_tick: u32` on `PersonAI` (new field, default 0). Skip steal if `now - last_steal_tick < RAID_STEAL_COOLDOWN_TICKS`.
  - Reserve gate: skip steal if `target.food_total - 1.0 < target.members * RAID_RIVAL_RESERVE_PER_MEMBER`.
  - Successful steal increments `attacker_faction.raid_stolen_food` and sets `last_steal_tick = now`.
  - Combat unchanged.

### End Conditions (`Engaged → Cooldown`)

End when any:

- `food_total + sum_party_hand_food >= members * RAID_CANCEL_FOOD_PER_MEMBER` (recovered).
- `target.food_total <= target.members * RAID_RIVAL_RESERVE_PER_MEMBER` (drained).
- `now - since_tick >= RAID_ENGAGED_TIMEOUT_TICKS` (timeout).
- `raid_party` is reduced (deaths/desertion) below 2.

On transition: clear `raid_party`, reset `raid_stolen_food` (after logging to ActivityLog), set `raid_phase = Cooldown { until = now + RAID_COOLDOWN_DAYS * TICKS_PER_DAY }`. Raiders' goals are re-evaluated next tick (food / return-home / sleep / etc.). `under_raid` on the target falls off naturally via `raid_detection_system`.

`Cooldown → Idle` when `now >= until_tick`.

## Nomad Migration

Constants:

```rust
pub const NOMAD_TRIGGER_FOOD_DAYS: f32 = 3.0;
pub const NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER: f32 = 1.0;
pub const NOMAD_MIN_ACCEPTABLE_SITE_SCORE: f32 = 15.0;
pub const NOMAD_NO_CANDIDATE_RETRY_DAYS: u32 = 2;
```

### Trigger (strict AND)

In `nomad_migration_system` (only when `caps.home.is_mobile() && nomad_autopilot && phase == Idle`):

```
food_total < members * NOMAD_TRIGGER_FOOD_DAYS
&& local_known_food_score < members * NOMAD_KNOWN_FOOD_TARGET_PER_MEMBER
&& now >= last_migration_tick + caps.migration_period_min_days * TICKS_PER_DAY
```

`local_known_food_score` is the existing AnyEdible-cluster scoring inside `NOMAD_FORAGE_RADIUS`. Replace the literal `TICKS_PER_SEASON` cooldown with the capability-derived value.

### Surveying & Candidate Scoring

- **Herd visibility**: replace the unconditional `wild_herds.herds.values()` scan in `pick_migration_candidates` with a knowledge-gated read. Iterate `SharedKnowledge.clusters` of a new `MemoryKind::HerdSighting` (or, if extending memory kinds is heavy, reuse `MemoryKind::AnyEdible` and tag entries from herd scouts with a `source: Herd` field). See Open Questions.
- **Danger**: delete `score_danger`'s `MemoryKind::Prey` branch. `score_danger` returns 0 by default; add an explicit `MemoryKind::HostileFactionSighting` arm so future scout reports of rival war parties produce a real penalty. Document the change in `nomad.rs` so the misuse is not reintroduced.
- **Early reject**: in `pick_migration_candidates`, before scoring, drop any candidate whose center tile is `Water` / `River` / `Wall` / `Ore` or whose surface elevation is unreachable from current home (`SpatialIndex` + cheap chunk-loaded check; full A* is reserved for `migration_target_ready`). This avoids generating attractive-but-illegal candidates that fail at commit.
- **Score threshold**: when no candidate clears `NOMAD_MIN_ACCEPTABLE_SITE_SCORE`, **do not** write `pending_migration`. Return to `Idle`, set `last_phase_change_tick = now`, and set `last_migration_tick = now - cooldown + NOMAD_NO_CANDIDATE_RETRY_DAYS * TICKS_PER_DAY` so the next attempt is ~2 days out, not a full season.
- **Scout cleanup on no-candidate**: clear the `AgentGoal::Scout` goal on dispatched survey scouts when returning to `Idle`; they revert to normal idle planning.

### Commit Path

- Keep `migration_target_ready` as the final passability/connectivity gate (it does the real A* check).
- If validation fails, clear `pending_migration`, return to `Idle`, apply the same short retry delay.
- **Remove** the blind `fallback_direction` autopilot commit. Keep the field for the debug/manual `PlayerCommandEvent::CommitMigration` path so dev tooling and player nomads can still force-pick a direction.

## Sedentary Collapse

Constants:

```rust
pub const COLLAPSE_FOOD_DAYS: f32 = 3.0;
pub const COLLAPSE_TRIGGER_SEASONS: u32 = 2;
pub const COLLAPSE_MIN_BED_COVERAGE: f32 = 0.5;
pub const COLLAPSE_BED_FALLBACK_RADIUS: i32 = 32;
pub const COLLAPSE_TRIGGER_TICKS: u32 = TICKS_PER_SEASON * COLLAPSE_TRIGGER_SEASONS;
```

### Combined-Failure Trigger

In `sedentary_collapse_system`, compute three booleans per faction (settled only):

```
food_deficit  = food_total < members * COLLAPSE_FOOD_DAYS
pop_crash     = members < SEDENTARY_COLLAPSE_MIN_MEMBERS
shelter_loss  = usable_beds < ceil(members * COLLAPSE_MIN_BED_COVERAGE)
failing       = food_deficit && (pop_crash || shelter_loss)
```

`collapse_streak` increments only when `failing`; resets to 0 otherwise. Collapse fires when `collapse_streak >= COLLAPSE_TRIGGER_TICKS` (~ 2 seasons ≈ 10 in-game days at `DAYS_PER_SEASON=5`).

### Shelter Counting

`usable_beds(faction, settlement_opt)`:

- If `settlement_opt.is_some()`: count beds owned by this faction inside the settlement footprint (use `SettlementData.bounds` + `BedMap`).
- Else (true nomadic-leftover or settlement-less band): radius scan via `BedMap` at `max(COLLAPSE_BED_FALLBACK_RADIUS, OLD_CAMP_RADIUS)` around `caps.home_tile`.

Add a `BedMap::owned_by(faction_id) -> impl Iterator<Item = (tile, BedSlot)>` accessor; today `BedMap` keys by tile only. If `BedSlot` doesn't already carry `faction_id`, add it (each `spawn_bed` site has the building's owner).

### Switch Archetype

In `handle_switch_archetype`, when transitioning settled → nomadic:

- `caps.home = Mobile { migration_period_min_days: ... }` (existing).
- `camp_state = Pitched` (newly collapsed band remains in place; it has not unpacked).
- `migration_phase = Idle`.
- Clear `pending_migration`.
- `last_migration_tick = now` so the new nomadic band cannot survey immediately.
- `nomad_autopilot` unchanged: AI keeps autopilot, player keeps it off.

## Critical Files (modify)

- `src/simulation/raid.rs` — phase machine, prep orchestration, capability draft, arrival/cooldown/steal-cooldown/reserve fixes.
- `src/simulation/faction.rs` — `RaidPhase` enum + lifecycle fields on `FactionData`; deficit/hunger streak counters; per-faction `raid_party`.
- `src/simulation/goals.rs` — replace `HUNGER_RAID_CEILING` gate with capability gate; only party members in `Marching|Engaged` get `AgentGoal::Raid`.
- `src/simulation/htn.rs` — new `PrepareForRaidMethod` (`WithdrawFromStorage → Equip`); `RaidEnemyHomeMethod` unchanged but gated on `RaidPhase::Marching`.
- `src/simulation/typed_task.rs` — `Task::WithdrawFromStorage` already exists; verify it routes weapons. Reuse `Task::Equip`.
- `src/simulation/items.rs` — `valid_equip_slots` already maps weapon→MainHand. No change unless `BedSlot` ownership needs adding (under sedentary_collapse).
- `src/simulation/person.rs` — add `last_raid_steal_tick: u32` to `PersonAI` (or a tiny new `RaidStealCooldown { last: u32 }` component to keep `PersonAI` lean).
- `src/simulation/jobs.rs` — `JobReason::RaidPrep`; `post_raid_prep_craft_jobs`; `cancel_by_reason`.
- `src/simulation/nomad.rs` — trigger rewrite, knowledge gating on herds, `score_danger` purge, early-reject, retry semantics, scout cleanup.
- `src/simulation/sedentary_collapse.rs` — combined-failure trigger, settlement-aware bed counting, archetype switch reset.
- `src/simulation/CLAUDE.md` — document new phase machine and trigger semantics.

## Existing Functions to Reuse (do not reinvent)

- `JobBoard::post` / `JobClaim` (`jobs.rs`) — craft order posting and claim locking.
- `Task::Equip` / `equip_task_system` (`items.rs`) — already swaps inventory↔slot and handles displacement.
- `Task::WithdrawFromStorage` (`typed_task.rs`) — already pulls items from `StorageTileMap`.
- `valid_equip_slots(ResourceId)` (`items.rs`) — routes weapon resources to MainHand.
- `score_danger` / `score_water` / `pick_migration_candidates` (`nomad.rs`) — keep API, surgical edits.
- `BedMap` chebyshev scan (`sedentary_collapse.rs`) — keep as fallback path.
- `SettlementData.bounds` — owns settlement footprint extent.
- `Body::is_dead`, `Health.current/max`, `Stats.str` (`combat.rs`, `stats.rs`) — capability scoring inputs.

## Test Plan

All tests are in-file `#[cfg(test)] mod tests` blocks, matching existing convention in `sedentary_collapse.rs`. Use pure-logic helpers where possible; fall back to a minimal `App` fixture only when a system needs to be exercised end-to-end.

### Raid

- Food deficit alone for 2 days does **not** trigger raid (hunger normal).
- Hunger crisis alone for 2 days does **not** trigger raid (food normal).
- Both for 1 day does **not** trigger.
- Both for 2 days → `Idle → Preparing`.
- Target filter rejects: SOLO, self, parent, child, same-root, subordinate, overlord, food-poor, beyond 500 tiles.
- Party selection: 10 members with assorted health/stats produces a party that excludes <50% HP and prefers higher STR + armed.
- Party cap = `min(8, ceil(0.35 * members))` honored.
- Preparing: `JobReason::RaidPrep` craft jobs posted equal to number of unarmed raiders.
- Preparing → Marching when all raiders have MainHand weapon (or half + half timeout).
- Prep abort on food/hunger recovery cancels all `RaidPrep` jobs.
- Arrival: chebyshev=1 with `target_tile != enemy_home` flips Marching → Engaged.
- Steal: per-raider cooldown enforced; reserve gate prevents draining target below `members * 5`; `raid_stolen_food` increments.
- End on recovery (storage + party hand food), drain, or 2-day timeout; enters `Cooldown` for 10 days.
- During Cooldown, no new raid triggers even if crisis re-emerges.

### Nomad

- Food-rich faction with weak local knowledge: stays `Idle`.
- Food-poor faction with strong local knowledge (clusters score ≥ members): stays `Idle`.
- Food-poor + weak knowledge + cooldown elapsed → `Surveying`.
- `migration_period_min_days` from capability is honored (vary capability value, observe cooldown).
- `pick_migration_candidates` no longer reads ungated `WildHerdRegistry`; herd in faction knowledge generates a candidate, ungated herd does not.
- `score_danger` returns 0 for a tile near a `MemoryKind::Prey` cluster (regression test for the removed misuse).
- `score_danger` returns negative for a tile near a `HostileFactionSighting` cluster (forward-compatibility test; skip if the kind isn't added in this pass).
- Surveying with no candidate ≥ 15.0 returns to `Idle`, leaves `pending_migration` unset, schedules retry in 2 days.
- `migration_target_ready` failure clears `pending_migration` and applies retry delay.
- Ocean / `Wall` tile candidate is dropped pre-scoring (does not appear in `pending_migration`).

### Collapse

- Food deficit alone for 2 seasons: no collapse.
- Pop crash alone for 2 seasons: no collapse.
- Shelter loss alone for 2 seasons: no collapse.
- Food deficit + shelter loss for 1 season: no collapse.
- Food deficit + shelter loss for 2 seasons: emits `SwitchArchetype`.
- Food deficit + pop crash for 2 seasons: emits `SwitchArchetype`.
- Bed counting uses settlement-owned beds when settlement exists, ignores beds outside footprint.
- Post-collapse faction: `camp_state == Pitched`, `migration_phase == Idle`, `pending_migration.is_none()`, `last_migration_tick == now`.
- Player faction post-collapse: `nomad_autopilot == false`; AI faction: unchanged from prior value.

### Regression

```
cargo test --bin civgame raid
cargo test --bin civgame nomad
cargo test --bin civgame sedentary_collapse
cargo test --bin civgame
```

## Verification (end-to-end)

1. `cargo run` (Mixed economy, Settled, 30 pop) and observe `ActivityLog` for raid lifecycle entries. Confirm that on a single bad harvest, the faction does **not** raid; a sustained shortfall produces a `RaidPhase::Preparing` log → craft jobs → equip → march → steal → cooldown.
2. `cargo run` with two adjacent factions on a low-food seed (or manually edit one storage to 0). Confirm raid prep posts craft jobs visible in the Jobs inspector tab; raiders equip before departure; on return the cooldown gate prevents an immediate second raid even if the food crisis persists.
3. `cargo run` with a Nomadic player faction and a Nomadic AI faction. Move the player to a clearly food-rich area; AI nomad with full storage should not migrate. Drain the AI's storage; observe `Surveying` only when both food < 3 days AND local knowledge weak; confirm no migration to an ocean tile.
4. `cargo run`, raze a settled AI faction's shelters and storage. Confirm collapse fires only after ~10 in-game days of combined failure, and the resulting nomadic band starts `Pitched` with `Idle` migration phase (does not immediately re-migrate).

## Open Questions

- **Herd knowledge tier**: the cleanest fix is a dedicated `MemoryKind::HerdSighting` cluster populated by hunter/scout sightings. If `SharedKnowledge`'s cluster machinery is generic enough to accept a new kind without a sweeping refactor, do it in this pass. Otherwise, tag entries in `MemoryKind::AnyEdible` with a `source` discriminator and filter at read. The plan assumes the former; verify before implementation.
- **`BedSlot.faction_id`**: if beds don't currently carry owning-faction, the settlement-aware bed count needs to walk back through the building owner. This is one extra hop; minor, but call out.
- **Drafted players in a raid party**: the new `RaidPhase` machine never auto-drafts; the user said "prioritize drafting the most capable," but in this codebase `Drafted` = player-controlled. We interpret "draft" colloquially as "select into the raid party." Player-marker `Drafted` agents remain excluded from autonomous raid parties; the player can still issue raid commands manually.

## Out of Scope (intentionally deferred)

- Wholesale rewrite of `SharedKnowledge` cluster kinds beyond what the herd gating needs.
- New raider profession or persistent warrior class (uses existing job/craft infrastructure instead).
- Combat tuning, weapon balance, armor recipes beyond what's already in `crafting.rs`.
- Inter-faction diplomacy beyond the existing parent/subordinate filter.
