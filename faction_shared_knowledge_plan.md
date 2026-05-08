# Faction-Shared Knowledge Implementation Plan

## Objective
Improve the reliability of resource gathering by implementing a **Faction-Shared Knowledge** system. This ensures agents do not "forget" crucial resources (like trees or stone) when their personal 32-slot memory fills up, allows newly spawned agents to instantly benefit from the faction's exploration, and ensures agents always route to the nearest known resource.

## Key Files & Context
- `src/simulation/memory.rs`: Contains the current `AgentMemory` and `vision_system`.
- `src/simulation/htn.rs`: HTN dispatchers that query memory for gathering targets.
- `src/simulation/gather.rs`: Executes gather tasks and currently calls `memory.forget()`.
- `src/simulation/faction.rs`: Defines factions and `FactionMember`.

## Proposed Solution

### 1. Introduce `FactionResourceMemory`
We will create a new Bevy `Resource` to store known static resource locations globally per faction.
- **Structure:** `AHashMap<u32, AHashMap<MemoryKind, AHashSet<(i32, i32)>>>` (where `u32` is `FactionId`).
- This shared memory will exclusively handle static, tile-based resources like `MemoryKind::wood()`, `MemoryKind::stone()`, and `MemoryKind::AnyEdible`.
- **Note:** Moving targets like `MemoryKind::Prey` and entity-based tracking will remain in the individual agent's `AgentMemory` as tactical, short-term memory subject to freshness decay.

### 2. Update `vision_system` (`memory.rs`)
- Modify `vision_system` to access `ResMut<FactionResourceMemory>` and `Query<&FactionMember>`.
- When an agent sees a static resource (plant, ground item, or stone tile), it will insert the coordinates into its faction's shared memory.
- When an agent sees an empty tile that used to have a resource, it will remove it from the faction's memory.

### 3. Update HTN Dispatchers (`htn.rs`)
- Update dispatchers such as `htn_acquire_food_dispatch_system`, `htn_acquire_good_dispatch_system`, `htn_harvest_plant_dispatch_system`, and `htn_stockpile_food_dispatch_system`.
- Replace calls to `agent_memory.best_for(kind)` with a query to `FactionResourceMemory`.
- The new lookup will iterate over the faction's known `AHashSet<(i32, i32)>` for the requested `MemoryKind` and compute the **closest** tile to the agent (e.g., using Chebyshev distance), rather than the "freshest" memory.

### 4. Update Gathering Execution (`gather.rs` & `items.rs`)
- When an agent successfully harvests a resource or discovers a tile is empty upon arrival (e.g., another agent harvested it first), `gather_system` currently calls `agent_memory.forget()`.
- This will be changed to remove the tile from `FactionResourceMemory`, ensuring the shared map is self-correcting and stays up-to-date.

## Alternatives Considered
- **Categorized Agent Memory:** Expands the agent's internal array into separate buckets (e.g., Food, Materials). Rejected because it does not enable knowledge sharing between agents.
- **Chunk-Based Memory:** Agents remember chunks containing resources instead of exact tiles. Rejected for now as it adds pathing complexity (agents would need to sweep the chunk upon arrival).

## Verification & Testing
1. **Unit/Integration Tests:** Check `src/simulation/test_fixture.rs` to ensure tests expecting agents to gather from memory still pass. Modify test assertions that check `AgentMemory` to check `FactionResourceMemory`.
2. **Behavioral Verification:** Run the simulation and observe agents. They should reliably path to the nearest tree/stone without forgetting its location, and multiple agents should benefit from one agent's exploration.

## Migration & Rollback
The change is isolated to the simulation systems. `AgentMemory` remains intact for non-resource memory (prey, relationships, settlements). If performance issues arise from large HashSets, we can cap the HashSet size or fall back to the old `AgentMemory` behavior.