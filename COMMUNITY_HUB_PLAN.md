# Community Hub (Decoupled Economy) Architecture Plan

## Background & Motivation
Currently, CivGame ties economic systems (Markets, Treasuries, Job Boards, Trading) strictly to the physical `Settlement` entity. Nomadic factions are defined by their lack of a `Settlement`, which inadvertently excludes them from the pluralist economy and breaks HTN AI tasks that assume physical storage tiles. To allow seamless transitions between nomadic and settled lifestyles and support varying economic modes (Market, Command, Mixed), the economic engine must be decoupled from physical map geography.

## Scope & Impact
- **Affected Domains:** Faction logic, Settlement logic, Economy (Transactions, Markets, Modes, Policies), HTN AI Planning, Trading primitives.
- **Impact:** Nomads will have functional local economies and job boards. Factions can freely switch lifestyles without losing their economic state or crashing AI task planners.

## Proposed Solution
Introduce a `CommunityHub` abstraction (either as a new Entity or attached directly to the `Faction` entity).
1. **Move Economic Components:** Move `Market`, `Treasury`, and `JobBoard` components from `Settlement` to `CommunityHub`.
2. **Unified Storage Interface:** Abstract storage access so HTN planners can query the `CommunityHub` for resources, which delegates to either physical `FactionStorageTile` (for settled) or pooled inventory (for nomads).
3. **Location Abstraction:** `CommunityHub` will maintain a `Transform` or coordinate that updates dynamically (fixed for settlements, follows the centroid of the tribe for nomads) so traders can navigate to them.

## Alternatives Considered
- **Virtual Mobile Settlement:** Granting nomads a "mobile" settlement entity. Rejected because it overloads the definition of a settlement and introduces edge cases in terrain systems that assume settlements are static.
- **Trait-based Economic Zones:** Keeping settlements but adding a `Caravan` entity for nomads. Rejected as it requires duplicating logic across two different entity types rather than unifying them.

## Implementation Plan

### Phase 1: The Community Hub Abstraction
1. Define a `CommunityHub` component.
2. Refactor `src/simulation/faction.rs` and `src/simulation/settlement.rs` to spawn a `CommunityHub` for every faction at creation, regardless of `Lifestyle`.
3. Move `Market`, `Treasury`, and `JobBoard` instantiation to the `CommunityHub` entity.

### Phase 2: Updating the Economy
1. Refactor `src/economy/transactions.rs` and `src/economy/market.rs` to target `CommunityHub` entities instead of `Settlement` entities.
2. Ensure autonomous traders route to `CommunityHub` locations.
3. Hook up the `EconomicMode` and `ResourceControlPolicy` checks to the `CommunityHub`.

### Phase 3: Unifying Storage & HTN Tasks
1. Introduce a storage trait or proxy system in `src/simulation/faction.rs` that unifies settled (tile-based) and nomadic (pooled) inventory.
2. Refactor `src/simulation/htn.rs` (`WithdrawFromStorageMethod`) to request resources from the `CommunityHub`, which handles the internal routing based on the faction's `Lifestyle`.

### Phase 4: Migration & UI
1. Update UI panels (`src/ui/economy_panel.rs`, `src/ui/job_board.rs`) to read from the `CommunityHub`.
2. Ensure switching a faction from Nomadic to Settled simply instantiates the physical `Settlement` structures while keeping the `CommunityHub` intact.

## Verification
- **Unit Tests:** Verify that a `CommunityHub` processes market transactions identically for both Nomadic and Settled factions.
- **Simulation Tests:** Run the world simulation and observe that nomadic factions successfully post jobs and accumulate wealth via trade.
- **HTN Validation:** Ensure nomadic agents successfully plan and execute tasks requiring material withdrawals.

## Migration & Rollback
- This is a significant structural change. A separate feature branch is recommended.
- If issues arise, the rollback strategy is to revert to the rigid `Settlement` requirements and isolate the `CommunityHub` changes behind a compilation flag until stable.