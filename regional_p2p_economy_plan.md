# Regional P2P Economy & Utility AI Plan

## Objective
Transform the simulation into a decentralized, peer-to-peer (P2P) capitalist economy driven by **Regional Ledgers** and a Utility AI equation ($U = E(R) - C_{action} - C_{opportunity}$). Instead of tracking individual prices and jobs, agents will remember `VisitedRegions` and query local boards, allowing for emergent migration, realistic trade, and massive performance scaling. Furthermore, we will model actual government officials and bureaucrats who manage these regional municipalities.

## Background & Motivation
Currently, Faction Chiefs allocate labor communally, and agents rely on a generic 32-slot memory array that easily "thrashes." Furthermore, the global Job Board and Market lack geographic nuance. Moving to a Regional model simplifies agent memory, improves CPU performance, and replaces Faction-based communism with a localized, municipality-driven P2P economy where real agents act as bureaucrats to manage public works and taxation.

## Scope & Impact
*   **`src/simulation/memory.rs`**: Simplify agent memory to track `VisitedRegions` (pointers to regions) rather than specific economic data.
*   **`src/simulation/region.rs`**: Establish the `Region` as the core economic and administrative unit, holding the Local Market, Treasury, and Job Board.
*   **`src/economy/market.rs`**: Move the Market from a global resource to a per-Region component, utilizing Exponentially Weighted Moving Averages (EWMA) for forecasting.
*   **`src/simulation/jobs.rs`**: Migrate the Job Board to be Region-specific. Add `reward` (wage) and `employer` to `JobPosting`. Shift `job_claim_system` to use the Utility AI equation.
*   **`src/economy/agent.rs`**: Enhance `EconomicAgent` with escrow features, P2P currency transfers, and tax liability tracking.

## Proposed Solution & Implementation Steps

### Phase 1: Regional Abstraction & Simplified Memory
1.  **The Region Unit**: Define geographic Regions (or settlements). Each Region acts as a municipality, possessing its own `RegionalMarket`, `RegionalJobBoard`, and `RegionalTreasury`.
2.  **Visited Regions Memory**: Update agent memory to include a small, fixed array: `VisitedRegions: [Option<(RegionId, Freshness)>; 8]`.
3.  **Gossip System Update**: When agents socialize, they gossip about `RegionId`s (e.g., "I visited Region B recently"). This costs almost zero CPU while still allowing agents to discover new economic hubs.

### Phase 2: Regional Market Tracking (The Ledger)
1.  **Localized Markets**: Each Region tracks its own daily production volume, consumption volume, and EWMA clearing prices for all `Good` types.
2.  **Cost of Goods Sold (COGS)**: Base item valuations on the historical cost of raw materials within that specific Region plus expected labor time.
3.  **Price Disparity & Arbitrage**: Markets being regional naturally incentivizes trade and hauling jobs across borders.

### Phase 3: The Contract Job Board & Escrow
1.  **Regional Job Boards**: Jobs are no longer global. They are posted to the `RegionalJobBoard` where the work is located.
2.  **P2P Postings & Escrow**: Individual agents or the Region can post jobs. When posted, the `reward` is deducted from the employer and held in escrow on the board.
3.  **Payout**: Transfer escrowed funds directly to the claimant upon `JobCompletedEvent`.

### Phase 4: Utility-Driven Workforce Allocation (Worker AI)
1.  **The Utility Equation**: Rewrite `job_claim_system`. Workers calculate:
    $$U_{bid} = E(R) - (C_{action} + C_{opportunity})$$
    *   **$E(R)$ (Expected Reward)**: The `reward` value, modified by the agent's subjective wealth (poor agents value $1 more).
    *   **$C_{action}$ (Action Cost)**: Travel time (estimated via Manhattan distance) + stamina degradation.
    *   **$C_{opportunity}$ (Opportunity Cost)**: The value of the agent's best alternative action.
2.  **Job Discovery**: When an agent's `BucketSlot` fires, they query the `RegionalJobBoard` of their *current* Region, and optionally other known regions if local utility is low.

### Phase 5: Bureaucracy and Taxation (Government Agents)
1.  **The Bureaucrat Profession**: Bureaucrat is a physical job role. The Region posts ongoing salary-based contracts for bureaucrats. Agents evaluating this job use the standard Utility AI (steady high wage vs. action cost of working at the town hall).
2.  **Administrative Action**: Public works (building walls, emergency stockpiles) are NOT magically posted. A physical bureaucrat agent must be active and working at their desk to evaluate the Region's needs and physically post the public contracts to the `RegionalJobBoard`. If a Region has no bureaucrats, infrastructure decays.
3.  **Tax Collection**: Bureaucrats periodically generate "Tax Assessment" tasks. Agents pay a percentage of their wealth or income to the `RegionalTreasury`. If the treasury dries up, bureaucrats stop getting paid, they quit to find other work, and the regional government collapses.

### Phase 6: Wealthy Agents & Private Employers
1.  **Agent Behavior**: High-wealth agents lacking time/skills will post private contracts to their local board (e.g., "Build me a house"), pricing the `reward` based on the local EWMA for labor.

## Verification & Testing
*   **Government Collapse Test**: Remove all bureaucrats from a region and verify that public jobs cease being posted and the treasury stops receiving taxes.
*   **Migration Test**: Verify that agents migrate when local wages drop compared to a neighboring region.
*   **Escrow Integrity**: Verify total system currency remains constant during posting, completion, and cancellation.
*   **Death Spiral Prevention**: Ensure the Utility equation handles starvation correctly.