# Trade Arbitrage

**Status:** Skeleton — awaiting planning session.
**Parent plan:** `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md` (Goal+HTN Behavioural Richness), Follow-up 3c.
**Depends on:** Phases A–E of parent plan. Strongly benefits from Follow-up 2 (opportunity-producers) for `MarketTradeOpportunityProducer`.

## Trigger

Pick up after at least one other modern-age domain (Heal or Teach) ships, so the per-domain pattern is well-rehearsed. Also requires multi-settlement gameplay to be in routine use — single-settlement campaigns don't exercise arbitrage.

## Scope

Add inter-settlement trade: `Trader` profession, caravans, price-gap detection between markets. Entrepreneurial Disposition drives `TradeArbitrageScorer`.

## Current state

- `SettlementMarket` per settlement (`src/economy/market.rs`).
- `Settlement.market_tile` linkage.
- **Verify:** does `Profession::Trader` exist? Search `src/simulation/profession.rs`. If yes, what's its primary skill — `Bargaining`? `Trade`?
- No inter-settlement caravan mechanism. Goods don't currently flow between markets.

## Files to touch

- `src/simulation/profession.rs` — confirm or add `Profession::Trader`; primary skill `Bargaining`/`Trade`.
- New `src/economy/caravan.rs` — `Caravan { origin_settlement, destination_settlement, manifest: Vec<(ResourceId, u32)>, escort: Vec<Entity> }`; `caravan_move_system`; `caravan_arrival_system` (executes trades at destination market).
- `src/economy/market.rs` — `price_gap_index_system` (Economy, daily) computes per-resource price gaps between every market pair within travel range.
- `src/simulation/goal_scorers.rs`:
  - `TradeArbitrageScorer` (Enterprise class): for Traders, score from `max_price_gap × buy_quantity × (1 + entrepreneurial/255 × 1.5) × travel_risk_factor`.
- `src/simulation/htn.rs` — new methods `LoadCaravan`, `TravelCaravan`, `SellAtMarket`. New `AbstractTask::ExecuteTrade`.
- `src/simulation/faction.rs` — `chief_trader_assignment_system` mirroring bureaucrat pattern.

## Open questions a real plan must resolve

- **Caravan unit.** Single entity carrying inventory, or group of agents + pack animals? Recommend: pack-animal-led, agent escorts (reuses existing `PackAnimalInventory`).
- **Travel risk.** Hostile factions intercepting caravans? Wildlife (wolves) attacking? Start with simple distance-based time cost; add hostility later.
- **Currency vs barter.** Does the existing `treasury` API support inter-faction transfers? If subsistence factions have empty `economic_policy`, do they accept caravans?
- **Price discovery interaction.** Caravans bringing in stock should affect buyer-determined prices per existing [[feedback-market-pricing]] — verify the `SettlementMarket` accepts external sellers without crashing prices.
- **Route persistence.** Once a profitable route is discovered, do Traders stick with it? Add `KnownRoute` memory per agent or per faction.
- **Bandit / piracy mechanics.** Out of scope for first pass; flag for later.

## Acceptance criteria

- Entrepreneurial Trader detects a price gap between two markets and runs a caravan.
- Both markets show the trade in their price history; treasury transfers correctly.
- Non-entrepreneurial Trader runs trades less often (richness check).
- Calibration test: spawn 2 settlements with diverging prices on one resource → caravan flow appears within N game-days.
- Inspector surfaces Trader's last route + profit.
