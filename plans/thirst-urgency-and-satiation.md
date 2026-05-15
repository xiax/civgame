# Drinking-water urgency + satiation fix

## Context

The user reports two symptoms about the thirst pipeline:

1. **Drinking water doesn't feel like an urgent need** — even moderately thirsty agents pick other goals over Drink.
2. **Agents don't drink until quenched** — they take a single sip and walk away with thirst still high, then need to be re-dispatched repeatedly.

Both bugs trace to specific divergences from how the hunger/eat pipeline is built. The thirst code path was originally written to "mirror hunger" (per comments in `drink.rs` and `goal_scorers.rs:683-685`), but two key asymmetries slipped in.

### Root cause 1 — urgency curve is too flat (Problem 1)

`ThirstScorer::score` (`src/simulation/goal_scorers.rs:686-710`) uses a hand-rolled linear ramp with a **0.30 floor**:

```rust
let urgency = ((t - THIRST_TRIGGER) / 75.0).clamp(0.30, 1.0);
```

At `THIRST_TRIGGER = 180` this evaluates to exactly **0.30**.

Meanwhile `SurvivalHungerScorer` (`goal_scorers.rs:643-672`) uses the shared `hunger_utility` two-stage smoothstep (`utility_curves.rs:37-41`):
- `hunger == 180` → **~0.55**
- `hunger == 200` → ~0.85
- `hunger == 230` → ~1.0

Both scorers live in `GoalClass::Survival` (same tier), so the tier-breaker doesn't help — selection is by raw urgency. A parched agent (thirst=180) with mild hunger (hunger=140, urgency ≈ 0.2) correctly picks Drink, but the much more common case — moderate thirst (180–200) competing against any kind of moderate hunger — always loses because thirst's curve starts much lower and ramps slower (full slope reaches ~0.67 at SEVERE=230 vs hunger ~1.0 at 230).

### Root cause 2 — single-sip-per-action (Problem 2)

`drink_task_system` (`src/simulation/drink.rs:148-239`) accumulates `TICKS_DRINK=4` work ticks, calls `perform_drink` **exactly once** (reduces thirst by `DRINK_THIRST_REDUCTION=80`), then `aq.finish_task` and exits.

`eat_task_system` (`src/simulation/production.rs:1106-1308`) uses the same `TICKS_EAT=8` accumulator but then **loops** (lines 1153–1292): repeatedly picks the next best edible from inventory/hands and consumes it until either:
- `hunger <= 0` (fully sated), or
- the next bite would waste >50% of the smallest food's nutrition (`hunger * 2.0 < min_nut`), or
- no edibles remain.

Drink has no such loop. From thirst=255 a single drink takes the agent to 175 — still above `THIRST_TRIGGER=180`-ish, definitely above the `GoalCommitment::UntilNeedBelow { threshold: 120.0 }` exit condition. The commitment is *satisfied* by one sip (since 175 < 180? actually no, 255−80=175 < 180 yes, and we need <120 → not satisfied). What actually happens: from thirst=180 → one sip → 100 → commitment satisfied → goal flips off → thirst climbs back to 180 over ~20s → re-dispatch. This is the user's "sip and leave" symptom.

It's worse for tile drinks: the agent already walked to the river and is standing on a fresh-water tile. There is *zero* reason to limit them to one sip there — they're literally next to unlimited water.

## Approach

Two surgical edits, each mirroring the proven hunger/eat pattern:

### Fix 1: Replace the linear thirst urgency with a smoothstep

Add a `thirst_utility(thirst)` curve in `src/simulation/utility_curves.rs` anchored on `THIRST_TRIGGER=180` / `THIRST_SEVERE=230`, shaped to **at or above** hunger's curve so a parched agent outranks an equally hungry one (thirst kills faster IRL — ~3 days vs ~3 weeks).

Anchor points:
- `thirst ≤ 100` → ~0.0
- `thirst == 150` → ~0.30 (rising; trigger soon)
- `thirst == 180` (TRIGGER) → **~0.60** (slightly above hunger's 0.55 at same value)
- `thirst == 230` (SEVERE) → ~0.95
- `thirst ≥ 250` → 1.0

Implementation: two-stage smoothstep mirroring `hunger_utility` shape, weights `0.60` / `0.40`:

```rust
pub fn thirst_utility(thirst: f32) -> f32 {
    let low = smoothstep(thirst, 100.0, 180.0) * 0.60;
    let high = smoothstep(thirst, 180.0, 230.0) * 0.40;
    (low + high).clamp(0.0, 1.0)
}
```

Then in `ThirstScorer::score` (`goal_scorers.rs:687`), replace the inline clamp with `thirst_utility(t)` and drop scoring below `urgency < 0.10` (mirrors `SurvivalHungerScorer`'s gate at line 646). Keep `THIRST_TRIGGER` as the **task-side** gate in `htn_drink_dispatch_system` so the dispatcher still refuses to fire Drink for non-thirsty agents — the scorer can score lower thirst values but won't actually dispatch a task until 180.

Actually simpler: keep the explicit `if t < THIRST_TRIGGER { return None; }` early-out in the scorer too — that preserves current behaviour of *not selecting Drink* until trigger, while giving the new curve a sharper ramp once we cross it.

### Fix 2: Convert `drink_task_system` to a multi-sip loop

In `src/simulation/drink.rs:204-237`, replace the single `perform_drink` call with a loop modelled on `eat_task_system`:

```rust
// Drink until quenched, source exhausted, or contaminated-sip cap reached.
// One sip = DRINK_THIRST_REDUCTION (80). Stop when thirst would go negative
// or when the next sip would waste >50% of it (thirst * 2.0 < 80 → thirst < 40).
let mut sips: u32 = 0;
let mut last_outcome = DrinkOutcome::SourceGone;
while needs.thirst > 40.0 && sips < MAX_SIPS_PER_ACTION {
    let outcome = perform_drink(source, &mut agent, &mut carrier, &mut needs, agent_tile, &chunk_map);
    match outcome {
        DrinkOutcome::SourceGone => break,
        DrinkOutcome::Drank { raw: _ } => {
            sips += 1;
            last_outcome = outcome;
        }
    }
}
// Sickness roll: apply once per action with severity scaled to sips taken if raw/contaminated.
if let DrinkOutcome::Drank { raw } = last_outcome { /* existing severity logic */ }
```

Key details:

- **`MAX_SIPS_PER_ACTION = 4`** safety cap. Even a fully-thirsty agent (thirst 255) drops to ~0 in 4 sips of 80. Prevents accidental infinite loops if the math regresses.

- **Inventory drink** consumes one `clean_water` per sip — natural cap when the supply runs out.

- **Tile drink** doesn't consume a resource, only verifies adjacency + tile kind on each call. Since the agent's tile and the source tile haven't moved between sips, all sips succeed. This gives the user-asked behaviour: an agent that walked to a river drinks fully before leaving.

- **Sickness severity** scales with sips for raw/contaminated water — taking 3 sips of raw river water is worse than one. Implementation: `severity_per_sip * sips` capped at `255` so the existing `SICKNESS_RAW_DRINK_SEVERITY=60` / `SICKNESS_CONTAMINATED_DRINK_SEVERITY=140` constants stay meaningful. (User can wave off — the simpler choice is "apply severity once regardless of sip count"; pick whichever feels right at implementation. Default: scale.)

- **Stop condition `thirst > 40.0`** mirrors eat's "majority waste" logic — drinking when thirst < 40 would waste >50% of the next 80-unit sip.

### Optional polish — bring commitment threshold in line

`ThirstScorer` sets `GoalCommitment::UntilNeedBelow { threshold: 120.0 }` (`goal_scorers.rs:706`). After Fix 2, an agent will drink down to ~0 in one action anyway, so the commitment threshold doesn't matter much. Leave it at 120 for now — it only kicks in if the action is interrupted mid-sip-loop by something higher-class.

## Critical files

- `src/simulation/utility_curves.rs` — add `thirst_utility` (new pub fn ~10 lines + tests).
- `src/simulation/goal_scorers.rs:686-710` — `ThirstScorer::score` swaps inline clamp for `thirst_utility` call, adds `urgency < 0.10` gate.
- `src/simulation/drink.rs:204-237` — `drink_task_system` consumes `perform_drink` in a `while needs.thirst > 40.0 && sips < MAX_SIPS_PER_ACTION` loop; sickness applied once at end.

No changes to: dispatcher (`htn_drink_dispatch_system`), `perform_drink` itself, needs.rs constants, the typed-task pipeline, or `CLAUDE.md` thirst-pipeline section beyond a one-line note.

## CLAUDE.md update

In `src/simulation/CLAUDE.md` "Thirst pipeline" section, append one sentence:

> `drink_task_system` loops `perform_drink` up to `MAX_SIPS_PER_ACTION = 4` until `thirst ≤ 40` so a single dispatch fully quenches the agent.

## Verification

1. `cargo check` — confirms the curve + scorer + executor changes type-check.
2. `cargo test --bin civgame thirst` and `cargo test --bin civgame utility_curves` — exercises the new curve and any existing thirst tests.
3. Manual run via `cargo run`:
   - Open inspector on a worker. Wait for thirst to cross 180. Confirm the agent picks `AgentGoal::Drink` even when also moderately hungry.
   - Watch a worker drink at a river tile: confirm a single action takes thirst from ~180 to near 0, not 180→100.
   - Watch a worker with `clean_water` in inventory: same — multiple units consumed in one Drink action.
4. Quick regression on `cargo test --bin civgame` overall — no related-but-distant assertions should flip.
