**Animal Biome Expansion Plan**

**Summary**
Add a data-driven animal catalog so the game can support many biome-native species without adding one-off ECS markers and duplicated systems for every animal. The first pass will cover full terrestrial/coastal ecology and husbandry, using an alt-history useful-animal roster. Ocean value will be represented by coastal/shore stocks, not true swimming entities.

**Key Interfaces**
- Add `AnimalSpecies(pub AnimalSpeciesId)` and `AnimalCatalog` loaded from `assets/data/animals/*.ron`, mirroring the existing plant catalog pattern.
- Add profile data for spawn rules, diet/ecology, social pattern, health/reproduction, butcher yields, tameability, domestic uses, housing, pack/draft capacity, and sprite keys.
- Keep legacy marker components only where current systems still need compatibility, but move shared behavior to `AnimalSpecies`.
- Add resource catalog entries for `milk`, `eggs`, `wool`, `feathers`, `ivory`, `shellfish`, and `dung`.
- Replace `CorpseSpecies`-limited butchery with corpse records keyed by `AnimalSpeciesId`.

**Roster And Behavior**
- Tundra: reindeer, musk ox, mammoth, arctic hare.
- Taiga: moose, brown bear, beaver, sable.
- Temperate: aurochs, red deer, wild boar, sheep, chicken.
- Grassland: horse, bison, antelope, wild ass.
- Tropical: water buffalo, elephant, junglefowl, peafowl.
- Desert: camel, gazelle, ostrich, donkey, desert goat.
- Mountain: ibex, yak, llama, alpaca, eagle.
- Wetland: water buffalo, duck, goose, capybara, crocodile.
- Steppe: horse, sheep, goat, saiga, bactrian camel.
- Badlands: bighorn sheep, wild ass, goat, lizard.
- Ocean/coast: seal, sea turtle, shorebird, shellfish beds.
- Generalize spawning to biome/coastal weighted pools with a global animal budget, plus generic large-herd LOD for herd species.
- Generalize ecology: predators hunt smaller prey, prey flee threat profiles, herbivores graze/browse plants, omnivores forage opportunistically, dangerous animals can attack humans when hungry or threatened.
- Generalize husbandry: tameable species map to existing tech families, with domestic products and uses such as milk, eggs, wool, dung fuel/fertilizer stock, pack animals, mounts, draft animals, guards, and companions.

**Implementation Changes**
- Create `src/simulation/animal_catalog.rs` and load it from `WorldPlugin`.
- Refactor `spawn_animals`, `animal_needs_tick_system`, water seeking/drinking, reproduction, death/corpse, hunting directives, taming, and wild-herd bloom/collapse to use catalog profiles.
- Rename/generalize `DeerGrazer` into a reusable herbivore grazer/browser component.
- Replace per-species sprite spawn/animate duplication with a generic `spawn_animal_sprites` / `animate_animal_sprites` path using catalog sprite keys and existing procedural creature templates.
- Update `SpatialIndex` with a generic `IndexedKind::Animal` for new species.
- Update `AGENTS.md` and `src/simulation/CLAUDE.md` with the new catalog, spawn, ecology, and husbandry conventions.

**Test Plan**
- `cargo check`
- `cargo test --bin civgame`
- Add catalog tests: loads all animal RON, no duplicate keys, every land biome has at least 3 primary species, Ocean has at least 3 coastal species.
- Add spawn tests: biome-native pools choose expected animals, coastal-only species require saltwater adjacency, global budget is respected.
- Add behavior tests: generic reproduction preserves species, butchery yields profile products, predators choose valid prey, herbivores reduce hunger by grazing.
- Add husbandry tests: taming gates by tech, domestic products accrue on cadence, pack/draft capacities attach from profile, housing assignment respects domestic class.

**Assumptions**
- No new crates.
- Use procedural/generated sprite-library art for new animals; PNG directional art remains optional for later polish.
- Existing techs are reused for v1: `DOG_DOMESTICATION`, `ANIMAL_HUSBANDRY`, `HORSE_TAMING`, `LOOM_WEAVING`, and `PORTABLE_DWELLINGS`.
- Aquatic movement is deferred; Ocean exploration rewards come from coastal animal stocks.
