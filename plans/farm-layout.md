**Farm Layout And Labor Fix**

**Summary**
Fix this as a mixed model: communal fields stay outside the built-up settlement, but are biased to the nearest viable edge instead of wandering to the best faraway fertility patch. Household kitchen plots become real Agricultural child plots, and any household adult can tend them.

**Key Changes**
- In `organic_settlement.rs`, keep `build_ag_belt` outside the built footprint, but add a preferred near-edge candidate band: use candidates within `fp_extent + 2 * parcel_size + ACCESS_SOFT` when any exist, fall back to the full scan only when necessary. Add a distance penalty so “good enough and close” beats “slightly better and very far.”
- In `land.rs`, when a household acquires a Residential plot, attach an Agricultural child plot. First reuse an existing nearby Agricultural plot within 12 tiles; if none exists, create a small kitchen-garden child plot behind the residence, opposite its frontage edge, after checking passability, no roads/walls/water, no existing plot overlap, and reachability.
- Ensure household storage is created whenever a household gains an Agricultural plot, including child plots acquired through the Residential path. The current storage creation only runs on direct Agricultural acquisition.
- In `htn.rs` / `goal_scorers.rs`, allow any `HouseholdMember` with a household-held Agricultural plot to use private `FarmScope`; remove the `Profession::Farmer` requirement for private household work. Farmers still get a slightly higher score, but non-farmer adults can work their own plot.
- For private planting, use household seed storage first, then fall back to parent village seed storage; harvest still deposits to household storage.
- In `faction.rs`, promote/retain enough Farmers for communal fields when state-owned Agricultural plots have plant/harvest work, not only when food per head is already low.
- In `jobs.rs` / `projects.rs`, raise farm workforce share when plantable farm work exists and post multiple open plot-scoped Farm jobs when assignments have not caught up, instead of only one nearest fallback job.
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` to document the new near-belt + household-kitchen-plot behavior.

**Interface Changes**
- Extend `FarmScope::Private` to distinguish seed source candidates from harvest deposit faction.
- Extend `GoalScoringContext` with household/private-farm availability so `FarmWorkScorer` can score household plot work for non-farmers.
- Add small land/farm helpers for “household has farm plot,” “private farm has work,” and “create/claim child kitchen plot.”

**Test Plan**
- Add tests that the ag belt prefers a reachable near-edge fertile plot over a far richer plot.
- Add tests that Residential acquisition creates or claims a child Agricultural plot and spawns household storage.
- Add tests that non-Farmer household adults can resolve private `FarmScope` and pick `AgentGoal::Farm`.
- Add tests for parent-village seed fallback with household harvest deposit.
- Add tests that chief farm posting can open multiple unassigned plot jobs and that farmer recruitment rises when communal plot work exists.
- Run `cargo test --bin civgame`.

**Assumptions**
- Keep large communal fields outside town; do not restore the old home-centered megablock fallback.
- Household kitchen plots are small, attached, and rent/listing-free because rent flows through the parent Residential plot.
- Any household adult may tend household plots, but survival, sleep, raids, and claimed jobs still preempt farm work.
