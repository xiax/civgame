# CivGame TODO List

## Z-Level Expansions
- [ ] **Ramps and Stairs Construction**: Implement UI orders and AI tasks for building ramps and stairs to facilitate vertical navigation beyond ±1 Z-level steps.
- [ ] **True Underground Mining (Tunneling)**: Support tunneling into terrain with "ceilings," requiring updates to pathfinding, line-of-sight, and rendering systems to handle subterranean movement.
- [ ] **Multi-Story Buildings**: Allow construction of floors and structures in `Air` tiles, enabling the creation of multi-level towers and bridges.
- [x] Add item weights and a carrying system. The inventory starts small, only like 5kg but can be
  expanded with equipment and clothing. Humans can also carry stuff with their hands, depending on      
  item size and weight, but they can only carry one or two things this way and if they use up both    
  hands carrying something then they can't do anything else like build or craft. Inventory is personal inventory, when hauling stuff for a task or gathering, they should use their hands.    
  make this logic smart and anticipate edge cases
- [x] I want to improve the building system. Currently the way buildings are placed follows a rigid        
  pattern and doesn't give each settlement a distinct personality. I would like a more fleshed out      
  building AI that can build expansive interesting and functional settlements, towns, and cities as     
  the game progresses. one that builds intelligently and can replace old buildings with new ones as     
  new technologies and materials become available
- terraforming should use mine and fill up task
X- Click dragging to select multiple units, there should be an option to draft them (rim world style) that switches them to military mode. In military mode it's right click to move, no harvesting or gathering, and right clicking on hostile entities auto attacks them and right clicking on neutral entities opens a menu for attack or move to. and use flow fields for units to reach target
X- add parallelization
- faction camp should have 20 tiles view range
- rimworld-like bottom menu for mass orders for gathering wood or farming or gathering bushes or construction
- Additional orders for mass crafting from a central control menu
- world generation to be more expansive with wider plains, bigger valleys and mountains, larger bodies of water, etc.
X implement willpower system. Work decreases willpower, fun stuff increases willpower, add play task, low willpower workers can look for entertainment items or people to play with. Any item like wood or stone can be played with, though they have low entertainment value. Add entertainment values to every item. Playing with other people should have higher value for everyone except loners. Playing with people counts as socialization as well and builds relationships
- allow workers to get more than one thing if their hands aren't full
-  I want to refactor the resouce collection system. Rather than      
- I want to add a military system to the game. There should be a military button that opens a military menu where players can define their military, add people to the military, either individually recruiting from a list of all workers or just increase the number cap and the game will automatically pick the most suitable, making sure not to leave crucial industries like food production without workers.

- I want to overhaul the world generation system. I want to first generate a realistic earth-like planet with various biomes and geographical features, mountains, rivers, valleys, plains, lakes, and oceans. The globe is divided into grids, like mega-chunks that make up the world. Then the player should be allowed to choose where they want to settle, and then the game generates the game map from that location's information using a deterministic seed that stays the same for that game, and when the player explores past their mega-chunk, we generate the map for that mega-chunk and allow the player to switch between the two using the world map. Make sure the geographical features between adjacent mega-chunks connect properly and that the biomes transition from on to the next in a way that makes realistic sense.

-I want to overhaul the resource gathering system. Picking stuff off the ground should be prioritized over      
  doing actual gathering work like cutting down trees or mining stone if the worker can see the resource just     
  lying on the groun  

 The "Hive Mind" (Global Spatial Registry)
  How it works: Individual workers do not have vision or memory. Instead, the
  simulation maintains a global spatial index (e.g., a grid or quadtree in
  src/world/spatial.rs) of all items, enemies, and resources. When a worker needs
  food, it queries the global index for the nearest food source, regardless of
  whether it "saw" it.



2. The "Unique Target" Problem
  The weakness of Flow Fields is when every unit wants to go to a different spot (e.g., 100k units each picking a
  specific individual enemy to fight).
  The Solution: Goal Grouping & Influence Maps.
   * Instead of each soldier targeting a specific enemy, they target an "Enemy Influence Map." 
   * You generate a heat map where "high heat" is where enemies are densest. 
   * The Flow Field guides the 100k units toward the "heat." Once they are within a few tiles of an enemy, they
     switch to simple local steering (look at the nearest 8 tiles and move toward the enemy).

  3. Implementation Requirements for 100k Units
  To actually hit that number in your CivGame (which uses Bevy/ECS), you must implement these three things:

   * GPU or SIMD Integration: At 100k units, even looking up a byte in a Flow Field can be slow if done
     sequentially. You should process movement in parallel using Bevy's ParallelSystem or even a Compute Shader.
   * Staggered Updates: As mentioned in your MEMORY.md, don't update all 100k units every tick. Update 10,000 units
     per tick over 10 ticks. At 20Hz, an agent only "decides" where to move 2 times per second, which is plenty for
     a large-scale battle.
   * Spatial Hashing: You need a lightning-fast way for units to "see" what is in front of them so they don't
     overlap. Your src/world/spatial.rs will be more important for performance than the pathfinding itself.


 - terraform_dispatch_system and goal_dispatch_system both run in
 ParallelB (simulation/mod.rs ordering uses .after(...), which is fine
 — Bevy honours it). No actual race.
 - Plan-fail → Explore fallback (plan.rs:1628-1650) can loop when
 preconditions stay unmet. This is a wandering-while-idle problem, not the
 "walks past target without engaging" bug the user is reporting. File
 separately if it becomes the dominant symptom after the Z fix.
 - Idle wander timer (movement.rs:327-420) doesn't promote back to Seeking.
 Pre-existing; out of scope.



ForageFood: 1.15= base 1.40 (+1.40 weights, +0.00 bias) -0.25 dist

FarmFood: 1.21= base 1.26 (+1.26 weights, +0.00 bias) +0.2 persist -0.25 dist

GatherWood: Rejected (Wrong Goal)
GatherStone: Rejected (Wrong Goal)
PlantAndFarm: Rejected (Not Learned)
HuntFood: Rejected (Preconditions Unmet)
ScavengeFood: 0.45= base 0.70 (+0.70 weights, +0.00 bias) -0.25 dist

BuildBlueprint: Rejected (Wrong Goal)
BuildBed: Rejected (Wrong Goal)
WithdrawAndEat: 1.50= base 2.00 (+2.00 weights, +0.00 bias) -0.5 no target
ReturnSurplusFood: Rejected (Wrong Goal)
EatFromInventory: 1.50= base 2.00 (+2.00 weights, +0.00 bias) -0.5 no target

PlaySocial: Rejected (Wrong Goal)
PlaySolo: Rejected (Wrong Goal)
Explore: -0.50= base 0.00 (+0.00 weights, +0.00 bias) -0.5 no target



Simulating Society and Economy
Govern the interaction between individual agents using a global Blackboard system acting as a centralized market or registry.

Resource Ledgers: Agents post localized buy and sell orders for goods to the Blackboard. The system matches these orders, adjusting local price gradients based on supply and demand. Agents read these gradients to update their Utility AI, driving them to migrate or switch professions based on profit incentive.

Skill Specialization: Restrict access to specific HTN branches based on an agent's memory and societal role. A tribal simulation restricts agents to baseline survival branches. Progressing to a complex economy unlocks specialized, interdependent branches. A blacksmith agent executes the "Smelt Iron" branch, outputting goods to the Blackboard that a soldier agent requires to execute the "Equip Weapon" branch.

Contractual Tasks: Allow agents to generate tasks for other agents. A wealthy agent writes a "Build House" contract to the Blackboard. Laborer agents evaluate the contract's utility against their own financial needs, accepting the task and executing the corresponding HTN.

High-level agents delegate tasks using a combination of Hierarchical Multi-Agent Reinforcement Learning (MARL) and structured negotiation protocols. The architecture separates strategic planning from mechanical execution, allowing complex societies to function autonomously without centralized micro-management.

Hierarchical Agent Architecture
Agents capable of delegation possess a split-level cognitive model.

The Meta-Controller (Manager): This layer evaluates high-level utility and societal drives. When the Meta-Controller determines a goal is necessary but recognizes the agent lacks the time, physical capability, or optimal HTN (Hierarchical Task Network) branch to execute it, it formulates a delegable subtask.

The Execution Controller (Worker): This layer handles the actual physical steps within the world. Worker agents constantly scan their environment for subtasks generated by Meta-Controllers that align with their specific skill sets.

Task Delegation Logic: The Contract Net Protocol
Agents negotiate task assignments through a decentralized execution mechanism known as the Contract Net Protocol (CNP). This protocol prevents network overload by structuring communication through the central Blackboard.

Call for Proposals (CFP): The Manager agent writes a formalized task schema to the regional Blackboard. This schema includes the required skill (e.g., "Masonry"), the objective coordinates, the deadline, and the proposed reward.

Bidding Phase: Worker agents within the region read the CFP. Eligible workers calculate their estimated completion cost and submit a bounded proposal (a "bid") back to the Blackboard.

Evaluation and Award: The Manager agent reviews the submitted bids. It evaluates the proposals based on predetermined criteria, such as the worker's proximity or historical success rate. The Manager selects the optimal candidate and flags the contract as "Awarded."

Execution and Verification: The winning Worker agent triggers its local HTN to physically complete the task. Upon finishing, the worker updates the contract state on the Blackboard. The Manager verifies the completion and executes the reward transfer, adjusting the global resource ledgers.

Utility Mathematics for Task Acceptance
Worker agents evaluate contracts mathematically. They do not accept tasks purely based on availability. The logic engine calculates the expected utility of a proposed contract using a strict equation:

$$U_{bid} = E(R) - (C_{action} + C_{opportunity})$$
​
$E(R)$ represents the expected reward value offered by the Manager.
$C_{action}$ represents the predicted physical and resource costs the Worker will incur by executing the necessary HTN branch.
$C_{opportunity}$ represents the mathematical value of the Worker's next best available action.

A Worker agent only submits a bid if U 
bid
​
  yields a positive integer greater than its current baseline utility. This logic ensures the simulated economy naturally allocates labor to the most profitable and efficient sectors at any given moment.

How detailed do you want the failure states to be when an agent abandons an awarded contract midway through execution?
