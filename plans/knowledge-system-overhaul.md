# Knowledge System Overhaul Plan

## Summary
Rebuild technology into a true knowledge system: personal awareness, mastery, accepted beliefs, rejected beliefs, written transmission, teaching, and faction-level practice all become parts of one knowledge ontology.

The first content target is the ancient core, Paleolithic through Bronze Age. The overhaul keeps a staged compatibility bridge so existing construction, crafting, HTN, vehicles, tablets, seeding, and UI continue working while the catalog expands.

A major addition is a richer building-knowledge model: each era gets multiple construction traditions, and settlements choose techniques based on local materials such as timber, clay, silt, sandy soil, reeds/straw, hide, and stone lithology.

## Core Model
Add a richer catalog while preserving transitional aliases:

```rust
pub type KnowledgeId = u16;
pub type TechId = KnowledgeId; // compatibility alias during migration

pub struct KnowledgeDef {
    pub id: KnowledgeId,
    pub key: &'static str,
    pub name: &'static str,
    pub era: Era,
    pub domain: KnowledgeDomain,
    pub kind: KnowledgeKind,
    pub truth: TruthStatus,
    pub scale: AdoptionScale,
    pub complexity: u8,
    pub prerequisites: &'static [KnowledgeReq],
    pub triggers: &'static [KnowledgeTrigger],
    pub belief_group: Option<&'static str>,
    pub contradicts: &'static [KnowledgeId],
    pub effects: &'static [KnowledgeEffect],
    pub legacy_aliases: &'static [&'static str],
}
```

Replace the `u64` ceiling with multiword `KnowledgeBits`, keeping `has`, `unlock`, `union`, `iter`, and `count` APIs so existing gates can migrate gradually.

`PersonKnowledge` becomes aware/mastered/accepted/rejected/confidence-based. Practical gates check mastered knowledge; belief-model effects check accepted knowledge. False knowledge can be mastered, taught, written, institutionalized, and later rejected without being forgotten.

## Building Knowledge
Split “building tech” into two layers:

- **Technique knowledge:** what people know how to build, such as wattle, dry-stone, mudbrick, timber framing, reed thatch, or cut stone.
- **Local material suitability:** what the settlement can actually support from nearby terrain, storage, trade, and known resource clusters.

Add a construction-specific knowledge domain:

```rust
KnowledgeDomain::Construction
KnowledgeKind::PracticalTechnique
KnowledgeEffect::UnlockBuildingTechnique(BuildingTechnique)
KnowledgeEffect::ImproveTechniqueSelection
```

Add a new `BuildingTechnique`/`WallTechnique` layer that can map onto current `WallMaterial` during migration. Existing `WallMaterial::{Palisade, WattleDaub, Stone, Mudbrick, CutStone}` remains as the render/combat compatibility surface, but selection chooses from richer techniques first.

Initial building techniques:

- **Paleolithic / mobile shelter**
  Brush Windbreak, Hide Lean-To, Stake-and-Hide Tent, Hearth Shelter Siting, Stone Ring Hearth.
- **Mesolithic / lightweight local structures**
  Wattle Screens, Reed Matting, Bark Roofing, Pit House, Timber Post Setting, Fish-Weir Carpentry.
- **Neolithic / permanent village techniques**
  Wattle and Daub, Cob Walling, Adobe Brick, Mudbrick Moulding, Timber Longhouse Framing, Thatch Roofing, Dry-Stone Walling, Raised Granary Floors.
- **Chalcolithic / specialized and defensive construction**
  Stone Footings, Fired Brick, Lime Plaster, Drainage Ditches, Palisade Engineering, Gatehouse Framing, Corbelled Stone, Planned Street Frontage.
- **Bronze Age / civic and monumental construction**
  Cut Stone Masonry, Ashlar Dressing, Mudbrick Mass Architecture, Timber Truss Roofing, Vault/Arch Precursor, Monumental Labor Coordination, Hydraulic Masonry, City Wall Engineering.

Local material rules:
- Forest/wood-heavy settlements prefer Timber Post, Palisade, Timber Longhouse, Bark Roofing.
- Wetland/tropical/clay areas prefer Wattle and Daub, Cob, Mudbrick, Reed Matting, Thatch.
- River silt areas prefer Mudbrick, Raised Granary Floors, Hydraulic Masonry.
- Desert/badlands/sandy soil prefer Adobe, Mudbrick, Stone Footings, Courtyard forms.
- Limestone areas favor lime plaster, dressed stone, and later cut-stone masonry.
- Granite/basalt areas make strong stone walls possible but slower and more labor/tool intensive.
- Sandstone areas favor easier block quarrying and desert masonry.
- Nomadic groups favor hide, reed, felt/lattice, and packable framing.

Implementation should extend the existing `select_wall_material` idea into `select_building_technique`, considering:
- mastered construction knowledge from chief/architect poster pool,
- available stored resources,
- raw gatherable resources,
- market procurement,
- biome/topsoil/lithology around the settlement,
- structure purpose: home, storage, civic, defensive, hydraulic, nomadic shelter.

Add resources if needed: `clay`, `reeds`, `thatch`, `lime`, and possibly `brick`. Keep generic `wood` and `stone` as fallback compatibility inputs.

## Ancient Core Catalog
Port every current tech into `KNOWLEDGE_CATALOG` with legacy aliases, then add foundational knowledge that old techs should depend on.

Foundations:
Fire Use, Ember Carrying, Toolstone Recognition, Edge Geometry, Hafting, Cordage, Hide Working, Animal Tracking, Seasonal Memory, Oral Tradition, Route Memory, Water Source Memory.

Math and records:
One-Many Counting, Tally Marks, Clay Tokens, Number Words, Measures and Units, Ration Arithmetic, Fractions, Practical Geometry, Area Measurement, Ledger Accounting, Seal Marks, Cuneiform Writing, Scribe Training, Legal Formulae.

Subsistence and craft:
Foraging Lore, Spear Hunting, Food Smoking, Food Drying, Fishing Weirs, Bow Craft, Dog Domestication, Seed Selection, Crop Cultivation, Grain Processing, Animal Husbandry, Irrigation, Fermentation, Flint Knapping, Bone Tools, Microlithic Blades, Pottery, Loom Weaving, Copper Working, Tin Prospecting, Bronze Casting.

Institutions:
Sacred Ritual, Shrine Custodianship, Chiefly Authority, Tribute Accounting, Bureaucratic Office, City-State Organization, Professional Army, Monumental Labor Coordination.

Cosmology and false knowledge:
Sky Dome Cosmology, Lunar Phase Observation, Solar Year Approximation, Eclipse Omens, Agricultural Calendar, Geocentric Cosmos, Divine Kingship Cosmology, Mathematical Astronomy.

Medicine and sanitation:
Wound Binding, Bone Setting, Herbal Remedies, Midwifery Lore, Latrine Practice, Clean Water Practice, Spirit Illness, Sympathetic Magic, Miasma Theory.

## False Belief Mechanics
Use competing belief groups with soft consequences:
- `cosmology`: Sky Dome, Geocentric Cosmos, future Heliocentric Model.
- `disease_causation`: Spirit Illness, Miasma Theory, future Contagion Theory.
- `omens`: Eclipse Omens, Weather Omens, Empirical Forecasting.

False beliefs can spread, satisfy cultural or ritual needs, and bias decisions. They should not hard-lock survival. Some can be accidentally useful, such as Miasma Theory increasing sanitation interest.

Normal UI labels these as “Belief”, “Model”, or “Contested”; debug UI can reveal truth status.

## Migration Steps
1. Add `KnowledgeBits`, `KnowledgeId`, `KnowledgeDef`, and `KNOWLEDGE_CATALOG`; keep old `TechId` and constants as aliases.
2. Convert `FactionTechs`, `PersonKnowledge`, adoption arrays, and study progress to multiword knowledge storage.
3. Port the current 50 techs, then add foundational math/writing/building/false-belief knowledge.
4. Add construction technique selection and local-material scoring; map chosen techniques to current wall/build outputs during transition.
5. Add new construction resources only where the simulation can use them immediately.
6. Update discovery, reading, teaching, lectures, tablets/books, activity log, tech panel, and inspector to use knowledge language.
7. Migrate construction, crafting, HTN, vehicles, animals, tools, vision, territory, and settlement planning gates to knowledge helpers.
8. Update `AGENTS.md` and `src/simulation/CLAUDE.md`.

## Test Plan
Required tests:
- Catalog is dense and acyclic.
- Knowledge ids above 63 work.
- Legacy tech constants and vehicle RON aliases resolve correctly.
- Existing gates still work through compatibility aliases.
- Building technique selection prefers clay/silt techniques near clay/silt, timber techniques near forest/wood, and stone techniques near suitable lithology.
- Settlements step down to locally available techniques instead of always choosing the highest era wall.
- False beliefs can be accepted, taught, written, contradicted, and rejected.
- Era derivation ignores false-only and experimental knowledge.
- Tablets/books can encode any knowledge id.

Verification:
```bash
cargo check
cargo test --bin civgame
```

## Assumptions
Use staged migration, not a hard replacement. Ancient-core knowledge is the first target. Building knowledge should be materially local rather than a single linear upgrade ladder. False beliefs use competing-model mechanics with real but soft simulation effects.
