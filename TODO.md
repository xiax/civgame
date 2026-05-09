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


Can you come up with a plan to overhaul the construction system please? The way it is currently implemented doesn't make sense  historically and doesn't look good and doesn't take into account farming plots and room for various buildings and the starting buildings infrastructure for various ages that matches the number of people in the faction and is able to expand organically.

I want to Add the concept of land ownership to the game. All land around a settlement belongs to the state by default but In mixed and capitalist economies the state can rent or sell land to households. Everything on a plot of land belongs to the household and the household can build or farm on the land. Land value should follow market dynamics, people desire housing next to where they work, land further away from settlement centre or other desirable resources are lower value and can be sold in larger quantities for private farmers or rented out for a tenant farmer model.
