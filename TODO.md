# CivGame TODO List

## Z-Level Expansions
- [ ] **Ramps and Stairs Construction**: Implement UI orders and AI tasks for building ramps and stairs to facilitate vertical navigation beyond ±1 Z-level steps.
- [ ] **True Underground Mining (Tunneling)**: Support tunneling into terrain with "ceilings," requiring updates to pathfinding, line-of-sight, and rendering systems to handle subterranean movement.
- [ ] **Multi-Story Buildings**: Allow construction of floors and structures in `Air` tiles, enabling the creation of multi-level towers and bridges.
- [ ] Add item weights and a carrying system. The inventory starts small, only like 5kg but can be
  expanded with equipment and clothing. Humans can also carry stuff with their hands, depending on      
  item size and weight, but they can only carry one or two things this way and if they use up both    
  hands carrying something then they can't do anything else like build or craft. They should deposit   
  any resources they aren't using to the faction tile if they need more space to pick up stuff, but     
  make this logic smart and anticipate edge cases
- [x] I want to improve the building system. Currently the way buildings are placed follows a rigid        
  pattern and doesn't give each settlement a distinct personality. I would like a more fleshed out      
  building AI that can build expansive interesting and functional settlements, towns, and cities as     
  the game progresses. one that builds intelligently and can replace old buildings with new ones as     
  new technologies and materials become available
- terraforming should use mine and fill up task
- Click dragging to select multiple units and use flow fields for units to reach target

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