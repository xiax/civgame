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
X- world generation to be more expansive with wider plains, bigger valleys and mountains, larger bodies of water, etc.
X implement willpower system. Work decreases willpower, fun stuff increases willpower, add play task, low willpower workers can look for entertainment items or people to play with. Any item like wood or stone can be played with, though they have low entertainment value. Add entertainment values to every item. Playing with other people should have higher value for everyone except loners. Playing with people counts as socialization as well and builds relationships
- allow workers to get more than one thing if their hands aren't full
X- I want to add a military system to the game. There should be a military button that opens a military menu where players can define their military, add people to the military, either individually recruiting from a list of all workers or just increase the number cap and the game will automatically pick the most suitable, making sure not to leave crucial industries like food production without workers.

X - I want to overhaul the world generation system. I want to first generate a realistic earth-like planet with various biomes and geographical features, mountains, rivers, valleys, plains, lakes, and oceans. The globe is divided into grids, like mega-chunks that make up the world. Then the player should be allowed to choose where they want to settle, and then the game generates the game map from that location's information using a deterministic seed that stays the same for that game, and when the player explores past their mega-chunk, we generate the map for that mega-chunk and allow the player to switch between the two using the world map. Make sure the geographical features between adjacent mega-chunks connect properly and that the biomes transition from on to the next in a way that makes realistic sense.

X -I want to overhaul the resource gathering system. Picking stuff off the ground should be prioritized over      
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


X - Can you come up with a plan to overhaul the construction system please? The way it is currently implemented doesn't make sense  historically and doesn't look good and doesn't take into account farming plots and room for various buildings and the starting buildings infrastructure for various ages that matches the number of people in the faction and is able to expand organically.

X - I want to Add the concept of land ownership to the game. All land around a settlement belongs to the state by default but In mixed and capitalist economies the state can rent or sell land to households. Everything on a plot of land belongs to the household and the household can build or farm on the land. Land value should follow market dynamics, people desire housing next to where they work, land further away from settlement centre or other desirable resources are lower value and can be sold in larger quantities for private farmers or rented out for a tenant farmer model.


X - I feel like there are a lot of places where the nomadic and settlement system are clashing as well as places where the different economic models are clashing, can you propose a design that allows us to seamlessly switch between the different models we want to capture in the game? In a way that minimizes bugs and allows us to expand to more systems easily in the future?

X - Overhaul settlement and nomadic system to make them work better together and work better in the future, throw in overhaul for making the different economic models mesh together as well.



Market-mode coherence track (M4-M5 deferred per memory)

Free-agent gathering. In Market mode, when should_craft declines (cooldown or no materials), the household's natural alternative is to
  gather raw materials themselves and sell to market. Today there's no AgentGoal::SelfStockpile — autonomous gathering routes through
  Stockpile JobClaims (chief-driven) or Survive (food only).


X - Fix memory leaks in late game.

I need logging to track performance metrics and identify bottlenecks in the code that is slowing down each tick, give me a plan to do this comprehensively please.



X - Settlements shouldn't be split by a river when placing initial settlements and camps



X - Add a fishing system to the game that works well with existing systems and is robust and extensible and true to history. 

X - Add swimming to the game.

X - Nomadic tribes should migrate way farther, they shouldn't be migrating to just within the gathering distance of their current settlement and actually look for good spots where there is a lot of food. They should send scouts far further away to look for good locations. Migration is also extremely buggy. I see people left behind in migrations, people not migrating properly, people still gathered around the old faction base and people no gain to the new faction base properly. Can you plan a comprehensive robust, and extensible migration system? I would also like it if it is possible to play a faction on the move rather than migration just being an atomic task.




X- Overhaul technology system to be entirely knowledge based, please propose a system for technological adoption that is true to history that uses the knowledge based system.

X- settlement creation gates the leader knowing about farming and deciding to farm, technological adoption is on a per agent level baed on their knowledge, technological adoption by governments depends on the knowledge of government officials and bureaucrats.

X - Add lookout system where an agent standing on high ground can see up to 50 grind vision rather than the standard 15, broken when they move. Add this as a player command and as something agents can naturally do, might be useful when exploring. Can be extended further with telescopes in the future.

Chief should have a daily quota. He can only post so many jobs a day. Immediate needs first, like getting enough food, then 

Implement the ε-greedy system.

X - Overhaul faction construction planning and fix doors being blocked by walls or other buildings, all doors should have an adjacent road on the opening side, not diagonal. The construction system should generate layouts randomly but with rules to make it logical and looks good and organic, rather than have a fixed plan.

X - Hunting system is currently buggy. Hunters lose target and just stand there doing nothing. They don't appear to chase. And they seem to hunt without weapons when they really shouldn't.

X - Update walls

X - Add water usage and drinking for both humans and animals

X - The way pack camp and pitch camp actually works isn't very realistic. The workers don't actually physically pack the camp. They don't carry the goods to the new camp, the camp itself just magically moves.


X - Stockpile food job posting not tracking collected calories properly


X - And can we add farms to the settlement planner? Farming should be portioned and zoned properly and should be for both communist and capitalist economies. Chief assigns plots for farmers to farm in communist economies and farmers buy plots in capitalist economies.

X - Add bridges to settlement planner

X - Workers should be able to do more than one task at a time, like work and socialize at the same time.

X - Make water actually flow, calculate z levels properly for water level and banks of river and make it more realistic with varying river sizes and stuff. Model water sources and reservoirs properly.

X - Make edges between different biomes more organic, right now the edges are these jagged square blocks.

X- Change how construction fallbacks work to be more unique per era rather than just fallback to previous era's construction policies. For example in neolithic if there is not material to build walls the planner shouldn't fall back to paleolithic patterns of building beds and hearths in crescent shapes, they should be building beds in sort of slums on the outskirts of the faction where there is free space, or longhouses with more beds in one house to save materials.

X - Animals walk on water and walk through walls

X - Create a detailed plan to add pathfinding checks to the settlement planner to make sure each placed furniture is reachable. For seeding and placing houses make sure the interior of the house is reachable by pathfinding after fully built by simulating the build. Same should apply for farms.

Player faction and factions with permanent settlements shouldn't auto-migrate anymore.

X - Farming doesn't need the technology to be adopted by the faction, if either the chief or architect or any bureaucrat knows it, it can be planned and posted, only in free market does the individual farmer need to have the knowledge, though farmers with the knowledge work faster.

X - Fix gap in survive goal for hunger and drink when no tasks under the goal meet requirements

X - Not making a tile cropland when seeding because its fertility doesn't meet some arbitrary requirement doesn't make sense from a realism perspective. Why would people just leave some land in the middle of their farm empty? I'm also seeing cropland being seeded that isn't actually in an agriculture zone.

X - Import new animal sprites

X - Faction queues way too many crafting jobs for stuff the faction doesn't even need.

X - Workers spend too much time preparing fields that they miss the chance to actually plant seeds.

X - Mining and digging should be incremental work. Each tile can be dug to up to 7 levels, completing the 7th removes the tile. At each level you get incremental resources. Without tools you can't dig past level 3. Digging down has effects even at incremental levels since it can provide cover and slow down people passing through the tile.

X - The amount of game map z level variability should depend on the biome and status in the world map. Currently we can't even properly simulate plains. Our game map generation needs to be more realistic.

Add initial food and resources and stone tools to spawn appropriate for each era.

Adjust work time to be more realistic and show progress bars. Cutting down trees should take way longer with initial stone tools

Everything that is set to run every certain number of ticks need to spread the work onto every tick or run in the background. Make sure not to design systems that only run every certain number of ticks in the future.

Need UI overhaul to add commands for all actions players can take, in a way that is robust and extensible in the future

Need debug menu overhaul that works with the current version of the game and contains all important tools a developer needs to debug and test the game like entity spawners and ways to give resources to factions, etc.

X - Fix workers standing too far away from their work targets when interacting with them.

X - I want to add diplomacy and territory to the game. Each faction should exert influence over a certain amount of territory based on each of their settlements and era, and they should try and protect their territory. Factions can also form agreements with other factions, whether that is trade, alliance, or war. By default factions should not like strangers trespassing on their land, maybe they should even send a message to them. Diplomacy with players should be handled through some form of diplomacy screen.

X - I want to add diplomacy AI that allows AI to initiate and make deals with other AI and players they are aware of, and judge the value of fair deals, including warning players for intruding in their territory and stuff like that.

Barter economies

X - Add volume to items and carrying capacity, remove hard number limit

Debug toggle for how quickly workers learn new knowledge

I want to add a lot more plant varieties to the game, plants that would be native to different biomes and were historically useful for human development. I want every biome to have at least a few unique native plants, so there is more incentive for exploration.

I want to add a lot more animal varieties to the game, animals that would be native to different biomes and were historically useful for human development. I want every biome to have at least a few unique native animals, so there is more incentive for exploration.

We don't have enough building wall options for each era. It doesn't make sense to only have wattle and daub for paleolithic when a lot of places don't even have reeds.

I want the plant and animal seeding to be more natural and realistic to real life.

Tiles with stuff on it sometimes flash the underlying tile, especially in fog of war

Some plants don't have sprites and are essentially invisible.


/Users/xiao1/civgame/plans/sleep-arrival-panic.md
task-outcome-feedback-contract.mddiplomacy-marriage.mddiplomacy-espionage.mddiplomacy-federations.md