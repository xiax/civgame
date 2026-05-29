# Fix Bedless Sleep When Valid Beds Exist

**STATUS: shipped.** All 4 changes landed + 11 pure-fn unit tests
(`bed_owner_is_stale` / `home_bed_claim_is_stale` / `should_reroute_bedless_sleeper`).
Full suite 1443 pass. `construction.rs` (reconciliation pre-pass, upgraded staleness in
both passes, generalized reroute), `htn.rs` (`htn_sleep_dispatch_system` owner==actor
gate), `src/simulation/CLAUDE.md` updated.

## Context
Workers sometimes finish a whole night on `Task::Sleep { bed: None }` (1× recovery, no
bed bonus) even though a same-faction bed is, or could be, theirs. Round 1 already
shipped (`53fece7`): rooted `bed_eligible_for_faction` (replacing the 30-tile Manhattan
box), root-faction bucketing, and cancellation of in-flight bedless sleep for workers
*freshly* bedded (`construction.rs:6655-6668`). `sleep`/`bed` tests pass.

Remaining problem = **bed-claim consistency**. `assign_beds_system` and the sleep
dispatcher each trust one side of `Person.HomeBed ↔ Bed.owner` without verifying the
other. Four verified holes:

- **A. Leaked `Bed.owner` locks beds.** Both passes skip beds with `bed.owner.is_some()`
  (6522, 6604); nothing clears an owner whose entity died or whose `HomeBed` no longer
  points back. Pass A/A.5 spouse-relocations set a new bed's owner without clearing the
  old; `death_system` never clears `bed.owner`.
- **B. Homeless detection treats any live bed as "housed."** `stale = bed_query.get(bed)
  .is_err()` (6474-6477 faction, 6583-6586 solo). A person whose `HomeBed` points to a
  bed owned by another (or `owner == None`) is never reassigned.
- **C. Sleep dispatch ignores ownership.** `htn_sleep_dispatch_system` (htn.rs:3671-3690)
  reads the bed Transform regardless of `Bed.owner`, so an agent can route to a bed that
  isn't theirs.
- **D. Previously-bedded → transient route fail → bedless all night.** Freshly-bedded
  list doesn't cover a worker who already owned a reachable bed but hit a dusk routing
  failure (htn.rs:3776/3817 → `Sleep { bed: None }` + `begin_sleeping`).

Goal: make `HomeBed == bed ⇔ Bed.owner == person` a reconciled, same-root-eligible
invariant, and re-route any bedless sleeper with a valid bed — no per-tick churn.

## Changes

### 1. Bidirectional reconciliation — `construction.rs::assign_beds_system`
Pre-pass after the `tick % 30` gate (~5985), before Pass 0. Hoist the `root_of` (6504) +
`plot_faction_at` (6505) closures above it. Walk every `Bed`; clear `bed.owner = None`
when ANY: owner Entity gone (`person_query.get(owner)` err — covers death leak); owner's
`HomeBed.0 != Some(this_bed)` (non-reciprocal — covers relocation leak); bed no longer
`bed_eligible_for_faction` for owner's `root_faction`.

Then upgrade the homeless-`stale` check in both passes from "entity missing" to the full
invariant: `Ok(bed) => bed.owner != Some(person) || !bed_eligible_for_faction(...)`,
`Err(_) => true`. Pre-pass runs first so freed beds re-enter the `available` pool (6514,
6597) the same tick. Fixes **A** + **B**.

### 2. Generalized bedless reroute — replace `cancel_bedless_sleep` tail (6655-6668)
One end-of-system scan: cancel any worker who is mid `Sleep { bed: None }`
(`aq.current.as_sleep() == Some(None)`), has a valid reconciled `HomeBed` (live + `owner
== person`), is not already on the bed tile, and has **no** recent SLEEP failure
(`history.recently_failed_count(MethodId::SLEEP, now) == 0`). `cancel_chain` → next
dispatch routes to `Sleep { bed: Some(_) }`. Subsumes the freshly-bedded case. Fixes
**D**. Reuses existing `recently_failed_count` (htn.rs:716) as the cooldown — no new
helper, no churn (30-tick cadence + 600-tick TTL guard). Add `Option<&MethodHistory>` to
`person_query`; if over Bevy's limit, split via `ParamSet` / read-only query (verify with
`cargo check`).

### 3. Dispatcher ownership gate — `htn.rs::htn_sleep_dispatch_system` (~3671-3690)
Read `&Bed` in `bed_query`; populate `ctx.home_bed`/`home_bed_tile` only when `bed.owner
== Some(actor)`, else leave `None` so `SleepMethod::expand` (1506) yields `Sleep { bed:
None }`. Defense-in-depth for the ≤30-tick pre-reconciliation window. Fixes **C**.

### 4. Docs
Update `src/simulation/CLAUDE.md` "Bed eligibility" / "Couple bed pairing" block:
bidirectional `HomeBed ↔ Bed.owner` reconciliation, mismatched-claim-as-homeless,
valid-bed bedless reroute guarded by `recently_failed_count(SLEEP)`, dispatcher
`owner == actor` gate. `sleep.rs` unchanged (executor already self-heals orphans).

## Tests
Beside `bed_eligible_for_faction` suite (9016-9189) + `test_fixture::TestSim`:
1. Dead / non-reciprocal / ineligible `Bed.owner` cleared → bed claimable same pass.
2. Mismatched `HomeBed` (`owner == Some(other)` and `owner == None`) → reassigned.
3. Bedless sleeper + valid `HomeBed` + no recent SLEEP fail → `cancel_chain` + reroute.
4. Worker with in-TTL SLEEP `FailedRouting` NOT re-cancelled; re-cancelled after TTL.
5. Dispatcher: `HomeBed` → bed with `owner != actor` ⇒ `Sleep { bed: None }`.

Run: `cargo test --bin civgame sleep`, `... bed`, `... construction`, `cargo check`.

## Assumptions
- "Available bed" = live, same-root-eligible, unowned **after** reconciliation.
- Truly unreachable bed → sleep in place (1×), retry after `MethodHistory` TTL, not every
  tick. Self-correcting.
