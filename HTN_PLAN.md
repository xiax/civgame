# HTN Architecture Rewrite Plan

## 1. Background & Motivation
The current AI plan and task system uses a fixed-size state vector (42 dimensions) for linear Utility AI, combined with statically defined step sequences (`PLAN_STEPS_*`). This architecture presents several scaling bottlenecks:
- **Rigidity**: Adding a new resource requires adding corresponding slots to the state vector and hand-tuning weights across all plan definitions.
- **Brittleness**: Plans cannot adapt dynamically (e.g. going to fetch a tool if missing during a task) and must either fail completely or have every combinatorial scenario hardcoded.
- **Maintenance Overhead**: Highly similar actions (e.g. `GatherWood`, `GatherStone`, `DeliverHide`, `DeliverGrain`) are duplicated instead of parameterized, polluting the system with boilerplate.

## 2. Scope & Impact
The proposed solution replaces the linear Utility AI and fixed step arrays with a **Hierarchical Task Network (HTN)** and **Dynamic Action Queue**. This refactor affects:
- `src/simulation/plan/mod.rs` & `registry.rs`
- `src/simulation/tasks.rs`
- `src/simulation/jobs.rs` (Goal selection coupling)
- Execution logic inside `plan_execution_system` and `goal_dispatch_system`.

## 3. Proposed Solution: HTN Planner
The rewrite introduces a scalable task hierarchy:
1. **Parameterized Primitive Tasks**: The existing `TaskKind` enum will be refactored to hold its target data natively (e.g., `TaskKind::WithdrawGood(Good, u32)` instead of implicitly reading `PersonAI.target_entity`).
2. **Action Queue**: Agents will carry an `ActionQueue { tasks: VecDeque<TaskKind> }` component. Instead of stepping through static ID arrays, they pop from this queue and execute.
3. **HTN Domain**: A planner system will expand abstract tasks (e.g. `AbstractTask::ObtainItem(Good::Wood)`) into sequences of primitive tasks (e.g., `WalkTo(Storage) -> Withdraw(Good::Wood)` OR `WalkTo(Tree) -> Harvest(Tree)`).
4. **Knowledge & Gossip System (Known Tasks)**: The existing `KnownPlans` component will be refactored into `TaskKnowledge`. Instead of gossiping opaque `PlanId`s, agents will learn and forget specific `Method`s or `AbstractTask` capabilities (e.g., learning a specific `RecipeId` or how to perform a complex economic activity). The HTN planner will only expand paths that the agent currently "knows".
5. **Parameterized Goal Selection**: Replaces the 42-float array. The agent's active `JobClaim` or highest-priority `AgentGoal` will directly seed the HTN planner without arbitrary float multiplication.

## 4. Phased Implementation Plan

### Phase 1: Parameterize Primitive Tasks
- Modify `TaskKind` to include parameters (e.g., `Harvest(Entity)`, `Withdraw(Good, u32)`).
- Update `tasks::assign_task_with_routing` to accept and store these explicit parameters instead of loose `target_entity` / `dest_tile` variables.
- Update task executor systems to read parameters directly from `TaskKind` (eliminating `PersonAI.target_z` and similar loose tracking variables where possible).

### Phase 2: Action Queue Integration
- Introduce the `ActionQueue` component on the worker entities.
- Refactor `plan_execution_system` to consume tasks from the `ActionQueue`.
- Temporarily update `registry.rs` to populate the `ActionQueue` with the parameterized primitive tasks instead of pushing `StepId`s, preserving the old goal-scoring logic but proving out the execution pipeline.

### Phase 3: Implement the HTN Planner
- Create a new `htn.rs` module.
- Define `AbstractTask` enum (e.g., `SatisfyNeeds`, `ExecuteJob(JobClaim)`).
- Write the expansion logic (Methods) that converts an `AbstractTask` into a `VecDeque<TaskKind>` given the current world state (via queries).
- Hook the HTN planner into `plan_execution_system`: when the `ActionQueue` is empty, determine the highest priority `AbstractTask` and expand it into the queue.

### Phase 4: Clean Up Legacy Systems
- Delete `STATE_DIM`, `build_state_vec`, and `state.rs`.
- Delete `PlanRegistry`, `StepRegistry`, and `registry.rs`.
- Simplify `goal_dispatch_system` to fully offload logic to the HTN planner.

## 5. Alternatives Considered
- **GOAP (Goal Oriented Action Planning)**: Explored as a more dynamic alternative, but A* search over world states can be computationally heavy for dozens of agents at 20 ticks/sec, and is harder to debug. HTNs are more predictable and performant for colony sims.
- **Behavior Trees**: Easier to author but suffer from deep nesting and lack of simple forward planning (like fetching a tool *before* executing the job). HTN offers the perfect middle ground.

## 6. Verification
- Regression testing: Ensure agents can still perform basic survival loops (eating, sleeping), gather resources, construct buildings, and craft.
- Performance profiling: Confirm the HTN expansion (which only happens when an agent's queue is empty) does not introduce latency spikes compared to the every-tick linear float scoring.

## 7. Migration & Rollback Strategy
The phased approach keeps the simulation running at every step. If the parameterized tasks (Phase 1) or Action Queue (Phase 2) introduce bugs, they can be isolated and reverted using standard git workflows. If the HTN (Phase 3) struggles with edge cases, the legacy Utility AI can be temporarily restored since the execution layer (`ActionQueue`) remains compatible.