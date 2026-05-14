# Detailed Farm Planner Plan

## Corrections From Review
- Agricultural parcels/zones already exist in `organic_settlement.rs`; the missing part is that `Field` pressure never becomes actual farm work, because `pressure_to_intent(Field)` returns `None`.
- Existing land acquisition blocks households that already own any plot, so farmer households can fail to buy/lease agricultural plots after getting housing.
- Existing chief farm jobs use `home_tile ±5`, ignoring agricultural zones and plot tenure.
- Plant ownership has a bug for communal farming: `production.rs` currently prefers `HouseholdMember` before `JobClaim`, so a household member doing a chief-assigned communal farm job can accidentally create household-owned crops.
- Private farmers also need storage routing fixes: planting must withdraw seeds from household storage, and harvesting must deposit crops back to household storage instead of village storage.

## Summary
Add farms as proper agricultural plots produced by the settlement planner, then connect those plots to economy-specific labor behavior.

Communal economies will keep agricultural plots state-owned. The chief assigns farm plots to farmers and posts plot-scoped `Farm` jobs. Capitalist economies will list agricultural plots for sale/lease/sharecrop; farmer households acquire them and autonomously work their own plots.

Mixed economies follow the existing preset: staple grain remains chief-allocated unless the grain policy is made capitalist elsewhere.

## Interfaces And Types
- Extend `JobProgress::Planting` with plot scope:
  - `plot_id: Option<PlotId>`
  - `assigned_farmer: Option<Entity>`
  - Existing non-plot callers use `None`.
- Add `FarmPlotAssignments` resource:
  - maps communal farmer entity to assigned agricultural `PlotId`
  - maps plot to assigned farmer
  - cleans stale assignments when plots, factions, or professions change
- Extend `Task::DepositToFactionStorage`:
  - `target_faction_id: Option<u32>`
  - `None` preserves current village-storage behavior
  - private farm harvests set this to the household faction id
- Add farm-work helpers in `tasks.rs` or a small `farm.rs` module:
  - resolve eligible farm plots for a worker
  - find nearest unplanted tile inside eligible plots
  - find nearest mature plant inside eligible plots
  - count unplanted plantable tiles for job sizing

## Implementation Changes
- Settlement planner:
  - Keep using `DistrictKind::Agricultural` parcels and compatibility `SettlementPlan` zones.
  - Align agricultural plot sizing so organic farm parcels do not produce awkward tenure plots; use the existing 8x8 organic parcel size or explicitly document the 10x10 compatibility subdivision if retained.
  - Exclude roads, doormats, buildings, water, walls, and non-plantable terrain from farmable tile selection.

- Land listing and acquisition:
  - Change household acquisition from “household has any plot” to separate checks for residential plots and agricultural plots.
  - Preserve current residential acquisition behavior, including nearby child agricultural plot claiming.
  - Add farmer-specific agricultural acquisition for private grain policy:
    - candidate must be `Profession::Farmer` with `HouseholdMember`
    - household must not already hold an agricultural plot
    - prefer Sale, then Lease, then Sharecrop
    - buy plots in Market when affordable; fall back to lease/sharecrop only when policy allows it
  - Split listing caps so agricultural plots are not starved by residential/civic listings.

- Communal farm assignment:
  - Add an Economy-set system after plot carving/acquisition and before chief farm posting.
  - For factions where `policy_for(grain).chief_allocates_labor == true`, assign state-owned agricultural plots to current farmers.
  - Score assignments by distance to home/work location and available farmable tile count.
  - Chief farm postings become plot-scoped:
    - `Planting.area = plot.rect`
    - `plot_id = Some(plot_id)`
    - `assigned_farmer = Some(entity)`
    - target count is bounded by seeds available and unplanted farmable tiles
  - Remove or limit the old `home_tile ±5` posting to bootstrap cases where no agricultural plots exist yet.

- Job claiming:
  - For `Farm` jobs with `assigned_farmer`, let only that farmer claim the job while the assignment is valid.
  - If the farmer is gone, demoted, or no longer eligible, cleanup releases the assignment/posting for reassignment.
  - Update duplicate-posting checks and wage/target logic for the new `Planting` fields.

- Private farmer goal behavior:
  - Add a `FarmWorkScorer` for capitalist/private grain policy.
  - It fires only for `Profession::Farmer` workers with household-held agricultural plots.
  - It scores `AgentGoal::Farm` when either:
    - household storage has seeds and the plot has unplanted farmable tiles, or
    - the plot has mature crops accessible to that household.
  - Register it ahead of generic stockpile/gather-food behavior so private farmers work farms before wandering off to broad gathering unless food crisis logic dominates.

- HTN farming:
  - Replace generic farm planting search with plot-scoped planting search.
  - Replace generic `AnyEdible` farm harvest search with plot-scoped mature crop search.
  - Communal farm context:
    - eligible plot is the assigned state-owned plot
    - seed source is village storage
    - harvest destination is village storage
    - planted crops are faction-owned
  - Private farm context:
    - eligible plots are household-held agricultural plots
    - seed source is household storage
    - harvest destination is household storage
    - planted crops are household-owned
  - Keep existing wild-food gathering separate from `Farm`; farm goals should not harvest random berry bushes outside owned/assigned farm plots.

- Storage and ownership:
  - Update seed withdrawal validation so a worker may withdraw from village storage or their own household storage when the task route selected that storage.
  - Update harvest deposit routing to honor `target_faction_id`.
  - Fix planter `LandClaim` ownership:
    - state-owned assigned farm plot plus `JobClaim::Farm` => `ResourceOwner::Faction`
    - household-held farm plot worked by that household => `ResourceOwner::Household`
    - non-farm/play planting keeps existing fallback behavior
  - Preserve sharecropping split in `gather.rs`; landlord share still routes to landlord storage, tenant remainder routes to the worker household for private plots.

- Documentation:
  - Update root `AGENTS.md` and `src/simulation/CLAUDE.md`.
  - Document that farms are agricultural plots, communal farms are chief-assigned, and capitalist farms are acquired and worked by farmer households.

## Tests
- Add/update land tests:
  - agricultural plots are listed independently of residential listing caps
  - farmer household can acquire an agricultural plot even after already holding housing
  - Market farmer buys a plot when affordable
  - lease/sharecrop fallback works when sale is unavailable or unaffordable
- Add communal farming tests:
  - chief assigns state-owned agricultural plots to farmers
  - chief farm posting uses plot bounds, not `home_tile ±5`
  - assigned farmer is preferred/required for assigned farm job
  - communal household member planting via chief job creates faction-owned crops
- Add private farming tests:
  - Market chief still does not post farm jobs for capitalist grain policy
  - private farmer goal scorer selects `Farm` when household has plot plus seeds or mature crops
  - private planting withdraws seeds from household storage
  - private harvest deposits to household storage
  - private planted crops receive household `LandClaim`
- Add regression tests:
  - farm HTN does not harvest mature plants outside eligible farm plots
  - sharecrop harvest still splits landlord share correctly
  - old non-farm gather/deposit task paths still default to village storage
- Run:
  - `cargo test --bin civgame`
  - `cargo check`

## Assumptions
- “Communist” means the existing communal labor policy: `policy_for(grain).chief_allocates_labor == true`.
- “Capitalist” means grain policy is private/capitalist and the chief does not allocate farm labor.
- Mixed preset remains as currently defined: staple food/grain labor is communal unless the existing economy preset changes.
- Farms use existing plant/seed mechanics; no new crop types or crates are needed for this pass.
- This plan does not add rent UI or new farm-specific visual sprites; it wires settlement planning, tenure, labor, ownership, and storage behavior first.
