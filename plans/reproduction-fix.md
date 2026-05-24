**Reproduction Bootstrap Fix**

**Summary**
- Diagnosis: reproduction is only partly working as intended. `reproduction.rs` correctly requires opposite-sex co-sleeping before conception, but seeded “spouse” relationships do not currently guarantee opposite-sex pairs or adjacent sleeping.
- Current timing is also slow by design: pregnancy lasts `54,000` ticks, i.e. 15 game days / 3 seasons, after conception.
- Existing test passed, but it only verifies household/affinity seeding, not sex compatibility or bed proximity.

**Key Changes**
- In founder spawning, generate settled founder sexes in deterministic male/female pairs when population allows, with varied pair order by faction/home seed so chiefs are not always the same sex.
- In [settlement_bootstrap.rs](/Users/xiao1/civgame/src/simulation/settlement_bootstrap.rs:72), make spouse seeding sex-aware: only assign spouse-grade affinity to opposite-sex pairs; leftover founders become kin/solo.
- In [construction.rs](/Users/xiao1/civgame/src/simulation/construction.rs:5703), add a couple-first bed assignment phase before individual bed claiming so seeded spouses get beds within co-sleep radius.
- Allow seeded spouses already assigned to separated beds to move to an existing nearby partner bed, but keep the stricter blueprint fallback threshold to avoid random extra bed spam.
- Leave `Pregnancy` duration unchanged for this pass; the goal is to fix missing/rare conception, not retune gestation yet.
- Update `AGENTS.md` game-start seeding notes to document sex-balanced founder couples and spouse bed clustering.

**Tests**
- Add a 2-founder Subsistence start test asserting one male, one female, reciprocal spouse affinity, same household, and bed tiles within co-sleep radius.
- Add a 4/6-founder start test asserting spouse-grade pairs are opposite-sex and no same-sex pair receives spouse affinity.
- Add a bed assignment regression test where both spouses begin homeless and still receive nearby beds in the first assignment pass.
- Add/extend co-sleep tests to confirm same-sex sleepers are ignored and opposite-sex nearby sleepers are tracked.
- Run `cargo test --bin civgame` focused on bootstrap/reproduction tests, plus the existing targeted test that currently passes.

**Assumptions**
- This applies to settled Subsistence/Mixed bootstrap relationships. Market starts keep their current one-person household model unless separately changed.
- Expected post-fix behavior: founder couples should usually attempt conception after the first valid shared sleep cycle, with births appearing 15 game days after successful conception.
