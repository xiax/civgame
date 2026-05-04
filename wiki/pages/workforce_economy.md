# Capitalist/Contract Economy & Utility AI Plan

## Objective
Transform the centralized, fraction-based workforce allocation system into a decentralized, peer-to-peer (P2P) capitalist economy. Agents will evaluate tasks using a strict Utility AI equation ($U = E(R) - C$), and the market will use historical forecasting (EWMA) to determine the true value of goods and labor. Individual agents will earn wages, hold personal wealth, and post contracts for others to fulfill.

## Background & Motivation
Currently, Faction Chiefs allocate labor using rigid percentages (`workforce_budget`), and agents fulfill jobs communally. By moving to a P2P contract economy, we unlock emergent gameplay: wealthy agents paying for personal services, specialized artisans demanding high wages, and dynamic labor shifting based on actual supply and demand.

## Scope & Impact
*   **`src/economy/market.rs`**: Shift from instantaneous Walrasian pricing to an Exponentially Weighted Moving Average (EWMA) for price, supply, and demand.
*   **`src/simulation/jobs.rs`**: 
    *   Add `reward` (wage) and `employer` to `JobPosting`.
    *   Rewrite `job_claim_system` to use the Utility AI equation.
*   **`src/economy/agent.rs`**: Enhance `EconomicAgent` to support escrow (holding funds while a contract is active) and direct agent-to-agent transfers.
*   **`src/simulation/goals.rs` & `person.rs`**: Allow individual agents to evaluate their personal needs and post contracts to the `JobBoard` if they have excess wealth but lack time/skill.

## Proposed Solution & Implementation Steps

### Phase 1: Advanced Market Tracking (The Ledger)
1.  **Market History Resource**: Create a `MarketHistory` resource that records daily production volume, consumption volume, and average clearing prices for all `Good` types.
2.  **EWMA Forecasting**: Update the `SimulationSet::Economy` to calculate a moving average of these metrics. This prevents hyper-volatility (e.g., a massive price crash just because one tree was chopped) and gives agents reliable data to calculate expected future value.
3.  **Cost of Goods Sold (COGS)**: Base item valuations on the historical cost of the raw materials plus the expected labor time.

### Phase 2: The Contract Job Board
1.  **Job Posting Overhaul**:
    *   Expand `JobSource` to include `Agent(Entity)`.
    *   Add a `reward: f32` field to `JobPosting`.
2.  **Financial Escrow**: When a `JobBoardCommand::Post` is issued by an agent or faction, the `reward` amount is immediately deducted from the employer's `EconomicAgent` component and held in escrow on the `JobBoard`.
3.  **Payout**: Modify `JobCompletedEvent` handling so the escrowed funds are directly transferred to the claimant(s) upon successful completion. If cancelled, funds return to the employer.

### Phase 3: Utility-Driven Workforce Allocation (Worker AI)
1.  **The Utility Equation**: Rewrite `job_claim_system`. Remove the rigid `bucket_share` limits. Workers will calculate:
    $$U_{bid} = E(R) - (C_{action} + C_{opportunity})$$
    *   **$E(R)$ (Expected Reward)**: The `reward` value of the posting, modified by the agent's personal value of money (a starving agent values $1 more than a rich agent).
    *   **$C_{action}$ (Action Cost)**: Time spent walking to the job + stamina/tool degradation costs.
    *   **$C_{opportunity}$ (Opportunity Cost)**: The value of the agent's *best alternative*. For a master crafter, the opportunity cost of hauling wood is massive, so they will only accept hauling if the wage is exorbitant.
2.  **Claiming**: The agent simply iterates through available jobs, scores them using the $U$ equation, and claims the job with the highest $U > 0$.

### Phase 4: Faction Treasury & Wealthy Agents (Employer AI)
1.  **Faction Behavior**: The Chief (representing the Faction Treasury) posts public works (e.g., walls, shared farms, emergency food stockpiles) offering wages from the Faction's tax pool.
2.  **Agent Behavior**: High-wealth agents who need a house built or raw materials gathered will post jobs to the board, pricing the `reward` based on the current market moving average for labor.

## Verification & Testing
*   **Escrow Integrity**: Verify through unit tests that total currency in the system remains constant when jobs are posted, completed, or cancelled (no money created or destroyed).
*   **Death Spiral Prevention**: Ensure the Utility equation properly handles extreme edge cases. If the town is starving, the subjective value of food ($E(R)$ for food gathering) must skyrocket asymptotically so agents abandon all other tasks to survive.
*   **Performance**: Verify that calculating the Utility equation for all idle workers against all postings does not cause a CPU bottleneck in the 20Hz update loop.
