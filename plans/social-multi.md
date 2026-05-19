# Ambient Work-Social Multitasking

## Summary
- Implement v1 as **primary work + secondary ambient social**, not arbitrary parallel tasks.
- Difficulty: **moderate and contained**. The current code already gives nearby agents passive social-need relief, but deeper effects like relationships, tech awareness gossip, wage gossip, and knowledge-tier promotion are still gated on the exclusive `AgentGoal::Socialize`.
- Reliability comes from keeping `ActionQueue`, `JobClaim`, routing, inventory, and work progress single-owner. Social overlap becomes a lightweight side channel.

## Key Changes
- Add a simulation-level secondary social component, e.g. `SecondarySocial { partner, expires_tick, strength }`, plus helpers for:
  - compatible primary tasks/goals
  - reciprocal/valid social contact
  - explicit `Socialize` vs ambient work-social contact
- Add an `ambient_social_pairing_system` before goal selection that pairs nearby compatible workers within radius 3, preferring same job/target, same root faction, then nearest deterministic partner.
- Keep incompatible states excluded: drafted/combat/raid/defend/rescue, sleep, eat, drink, seek-care/heal, dormant LOD, SOLO, hostile factions, and player-forced commands unless explicitly safe.
- Refactor social consumers to use the shared social-contact helper:
  - social need fill
  - relationship memory
  - awareness/settlement gossip
  - shared-knowledge tier promotion
  - wage gossip
- Keep deliberate teaching/mastery transfer limited to explicit `AgentGoal::Socialize` or teaching tasks, so casual work chatter does not become accidental instruction.
- Suppress unnecessary Socialize detours while a valid ambient social contact is active, so workers do not abandon jobs just because their social need rose.

## Test Plan
- Add focused tests in `src/simulation/test_fixture.rs`:
  - two nearby workers keep their primary `ActionQueue`/`JobClaim` while gaining `SecondarySocial`
  - social need and relationship memory improve during work-social contact
  - awareness and wage gossip propagate through ambient contact
  - explicit tech teaching does not trigger from ambient contact
  - incompatible states never receive `SecondarySocial`
  - despawned/out-of-range partners are cleaned up
- Add unit tests for the task/goal compatibility predicate.
- Run `cargo test --bin civgame` and `cargo check`.

## Assumptions
- Scope is **Work + Social** first.
- Style is **Ambient Nearby** with conservative cooldowns.
- No new crates.
- Update simulation docs after behavior changes, especially `src/simulation/CLAUDE.md` and root `AGENTS.md` if the new scheduling rule is documented there.
