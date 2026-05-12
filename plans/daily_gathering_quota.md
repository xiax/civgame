# Daily Gathering Quota Plan

## Objective
Implement a "daily quota" system for chief-assigned basic resource gathering (Food, Wood, Stone) as requested by the user. 
This will prevent workers from constantly topping off minor storage deficits throughout the day, allowing them to naturally idle or focus on other tasks once the day's gathering quota is met. 
Reactive jobs like Building construction, Hauling, and Crafting will continue to be evaluated and posted dynamically so the settlement remains responsive to player actions.

## Key Files & Context
- `src/simulation/jobs.rs`: Contains `chief_job_posting_system` which currently evaluates all deficits and posts jobs every 60 ticks. It drops unclaimed jobs unconditionally, which allows it to statelessly recalculate targets.

## Implementation Steps
1. **Preserve Gathering Jobs on the Job Board:**
   - In `chief_job_posting_system`, modify the `retain` closure that cleans up unclaimed chief postings. 
   - Change the logic so that `JobProgress::Calories` and `JobProgress::Stockpile` are **not** dropped when unclaimed. They will instead persist until fully completed (handled automatically by `record_progress` and `JobCompletedEvent`).
   - `JobProgress::Planting` and `JobProgress::Crafting` will continue to be dropped and statelessly recalculated.

2. **Daily Quota for Food (Calories):**
   - Introduce a daily tick check: `let is_daily_tick = clock.tick % 3600 == 0;`.
   - Wrap the Food Stockpile posting logic (Step 3) in `if is_daily_tick { ... }`.
   - The chief will only assess the food deficit and post a new food gathering job once a day. If the workers finish it early, they will not be assigned more food gathering until the next day.

3. **Hybrid Anticipatory/Reactive Quota for Materials (Wood/Stone):**
   - The material stockpile postings (Step 3b) fulfill both *anticipatory* needs (routine gathering) and *reactive* needs (Blueprint demands).
   - Only include the `anticipatory` storage target if `is_daily_tick` is true. Otherwise, `anticipatory` is 0.
   - `bp_demand` (Blueprint demand) will continue to be evaluated every 60 ticks.
   - If a `Stockpile` job for the needed resource already exists on the board, instead of skipping or dropping it, we will **update its target** if the current total deficit (`deficit = target_total - stored`) is greater than the job's remaining work (`target - deposited`).

4. **Update CraftOrder Stockpile Demands (Step 3b-ii):**
   - Similar to Blueprint demands, CraftOrder material deficits are reactive.
   - If a `Stockpile` job already exists for the demanded resource, update its target if the deficit exceeds the remaining work on the job, rather than skipping.

## Verification & Testing
- **Food Gathering:** Run the game and verify that the food gathering job is posted once. Once completed, no new food jobs should appear until the next day (3600 ticks later), leaving workers idle or free for other tasks.
- **Blueprint Construction:** Place a building blueprint. The chief should immediately evaluate the `bp_demand` and either post a new Wood/Stone `Stockpile` job or increase the target of an existing one.
- **Job Completion:** Ensure that `Calories` and `Stockpile` jobs correctly disappear from the job board once their target is met.