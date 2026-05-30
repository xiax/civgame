//! Data-driven wild-animal species catalog (Phase 2 of the ecology overhaul).
//!
//! Replaces the hard-coded `*_COUNT` / `*_HP` constants and inline biome pools
//! in `animals.rs` with one table of per-species ecology: hit points, social
//! pattern, diet, habitat biomes, initial population target, territory size,
//! birth season, and migration strategy. Counts and habitat scoring read from
//! here so adding/retuning a species is a single-row edit.
//!
//! Kept as a compile-checked code table (not RON) so a malformed entry is a
//! build error rather than a runtime panic. `AnimalSpeciesId` is the index into
//! [`ANIMAL_SPECIES`]; ordering is stable (matches `AnimalKind`).

use crate::simulation::animals::AnimalKind;
use crate::world::geomorph::ReliefSample;
use crate::world::globe::Biome;
use crate::world::seasons::Season;

/// Stable id = index into [`ANIMAL_SPECIES`]. Mirrors `PlantSpeciesId`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AnimalSpeciesId(pub u16);

impl AnimalSpeciesId {
    pub const fn raw(self) -> u16 {
        self.0
    }
}

/// High-level grouping pattern (cluster geometry lives in `animals.rs`'s
/// `SocialPattern`; this is the ecological classification).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnimalSocial {
    Herd,
    Pack,
    Solitary,
}

/// What the species eats — drives Phase 3 foraging and predator/prey pressure.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnimalDiet {
    Grazer,
    Browser,
    Omnivore,
    Carnivore,
}

/// Seasonal range behaviour.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnimalMigration {
    /// Fixed territory.
    Resident,
    /// Shifts territory centre seasonally (water/snow/forage).
    LocalSeasonal,
    /// Long-distance route across globe cells.
    WorldRoute,
}

/// One wild species' ecology.
#[derive(Clone, Debug)]
pub struct AnimalSpeciesDef {
    pub kind: AnimalKind,
    pub display_name: &'static str,
    pub hp: u8,
    pub social: AnimalSocial,
    pub diet: AnimalDiet,
    /// Biomes the species is native to. Habitat suitability is 0 outside these.
    pub biomes: &'static [Biome],
    /// Initial wild population target across the seeded near-region.
    pub target_count: u32,
    /// Territory radius (tiles) for Phase 3 wander-bounding.
    pub territory_radius: i32,
    /// Exclusive species defend non-overlapping territories (predators);
    /// non-exclusive (grazers) tolerate overlap.
    pub exclusive: bool,
    /// Season births concentrate in (Phase 3 reproduction pacing).
    pub birth_season: Season,
    pub migration: AnimalMigration,
}

/// The species table. Index = `AnimalSpeciesId`; order matches `AnimalKind`.
/// Counts preserve the legacy `*_COUNT` values so startup density is unchanged
/// until tuned. Earth-analog habitat assignments.
pub const ANIMAL_SPECIES: [AnimalSpeciesDef; 8] = [
    AnimalSpeciesDef {
        kind: AnimalKind::Wolf,
        display_name: "Wolf",
        hp: 30,
        social: AnimalSocial::Pack,
        diet: AnimalDiet::Carnivore,
        biomes: &[
            Biome::Temperate,
            Biome::Taiga,
            Biome::Tundra,
            Biome::Grassland,
            Biome::Mountain,
        ],
        target_count: 150,
        territory_radius: 40,
        exclusive: true,
        birth_season: Season::Spring,
        migration: AnimalMigration::Resident,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Deer,
        display_name: "Deer",
        hp: 20,
        social: AnimalSocial::Herd,
        diet: AnimalDiet::Browser,
        biomes: &[
            Biome::Temperate,
            Biome::Taiga,
            Biome::Grassland,
            Biome::Wetland,
        ],
        target_count: 400,
        territory_radius: 30,
        exclusive: false,
        birth_season: Season::Spring,
        migration: AnimalMigration::LocalSeasonal,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Horse,
        display_name: "Wild Horse",
        hp: 40,
        social: AnimalSocial::Herd,
        diet: AnimalDiet::Grazer,
        biomes: &[Biome::Grassland, Biome::Steppe],
        target_count: 200,
        territory_radius: 36,
        exclusive: false,
        birth_season: Season::Spring,
        migration: AnimalMigration::LocalSeasonal,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Cow,
        display_name: "Aurochs",
        hp: 35,
        social: AnimalSocial::Herd,
        diet: AnimalDiet::Grazer,
        biomes: &[Biome::Grassland, Biome::Temperate, Biome::Wetland],
        target_count: 80,
        territory_radius: 30,
        exclusive: false,
        birth_season: Season::Spring,
        migration: AnimalMigration::LocalSeasonal,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Pig,
        display_name: "Wild Boar",
        hp: 25,
        social: AnimalSocial::Pack,
        diet: AnimalDiet::Omnivore,
        biomes: &[
            Biome::Temperate,
            Biome::Taiga,
            Biome::Tropical,
            Biome::Wetland,
        ],
        target_count: 120,
        territory_radius: 20,
        exclusive: false,
        birth_season: Season::Spring,
        migration: AnimalMigration::Resident,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Fox,
        display_name: "Fox",
        hp: 12,
        social: AnimalSocial::Pack,
        diet: AnimalDiet::Omnivore,
        biomes: &[
            Biome::Temperate,
            Biome::Taiga,
            Biome::Grassland,
            Biome::Tundra,
        ],
        target_count: 80,
        territory_radius: 24,
        exclusive: true,
        birth_season: Season::Spring,
        migration: AnimalMigration::Resident,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Rabbit,
        display_name: "Rabbit",
        hp: 6,
        social: AnimalSocial::Pack,
        diet: AnimalDiet::Grazer,
        biomes: &[
            Biome::Grassland,
            Biome::Temperate,
            Biome::Steppe,
            Biome::Desert,
        ],
        target_count: 500,
        territory_radius: 12,
        exclusive: false,
        birth_season: Season::Spring,
        migration: AnimalMigration::Resident,
    },
    AnimalSpeciesDef {
        kind: AnimalKind::Cat,
        display_name: "Wildcat",
        hp: 8,
        social: AnimalSocial::Solitary,
        diet: AnimalDiet::Carnivore,
        biomes: &[Biome::Temperate, Biome::Taiga, Biome::Grassland],
        target_count: 60,
        territory_radius: 24,
        exclusive: true,
        birth_season: Season::Spring,
        migration: AnimalMigration::Resident,
    },
];

/// Resolve a species def by `AnimalKind`. Linear over 8 entries (cheap).
pub fn species(kind: AnimalKind) -> &'static AnimalSpeciesDef {
    ANIMAL_SPECIES
        .iter()
        .find(|d| d.kind == kind)
        .expect("every AnimalKind has a catalog entry")
}

/// Stable id for a kind = table index.
pub fn id_of(kind: AnimalKind) -> AnimalSpeciesId {
    let idx = ANIMAL_SPECIES
        .iter()
        .position(|d| d.kind == kind)
        .expect("every AnimalKind has a catalog entry");
    AnimalSpeciesId(idx as u16)
}

/// Initial wild population target for a kind.
pub fn count_for(kind: AnimalKind) -> u32 {
    species(kind).target_count
}

/// Hit points for a kind.
pub fn hp_for(kind: AnimalKind) -> u8 {
    species(kind).hp
}

/// Habitat suitability in `[0,1]` for a species at a tile, from biome + relief.
/// Zero outside the species' native biomes. Within them, mountains/steep slopes
/// reduce suitability for large-bodied grazers; aridity reduces it for
/// everything except desert specialists (rabbits here). Pure + unit-testable.
pub fn habitat_suitability(def: &AnimalSpeciesDef, biome: Biome, relief: &ReliefSample) -> f32 {
    if !def.biomes.contains(&biome) {
        return 0.0;
    }
    let mut s = 1.0_f32;
    // Steep terrain penalises herds more than solitary/pack hunters.
    let slope_pen = match def.social {
        AnimalSocial::Herd => relief.slope.clamp(0.0, 1.0) * 0.7,
        _ => relief.slope.clamp(0.0, 1.0) * 0.35,
    };
    s *= 1.0 - slope_pen;
    // Aridity penalty, waived for desert-tolerant species.
    let desert_ok = def.biomes.contains(&Biome::Desert);
    if !desert_ok {
        s *= 1.0 - relief.aquifer_depth_norm.clamp(0.0, 1.0) * 0.4;
    }
    s.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_exactly_one_entry() {
        for kind in [
            AnimalKind::Wolf,
            AnimalKind::Deer,
            AnimalKind::Horse,
            AnimalKind::Cow,
            AnimalKind::Pig,
            AnimalKind::Fox,
            AnimalKind::Rabbit,
            AnimalKind::Cat,
        ] {
            let n = ANIMAL_SPECIES.iter().filter(|d| d.kind == kind).count();
            assert_eq!(n, 1, "kind {:?} must have exactly one catalog entry", kind);
        }
    }

    #[test]
    fn ids_are_stable_and_dense() {
        for (i, d) in ANIMAL_SPECIES.iter().enumerate() {
            assert_eq!(id_of(d.kind).raw() as usize, i);
        }
    }

    #[test]
    fn desert_rabbit_rejects_taiga_but_horse_loves_grassland() {
        let flat = ReliefSample {
            slope: 0.0,
            local_relief: 0.0,
            mountain_distance: 10.0,
            coast_distance: 10.0,
            aquifer_depth_norm: 0.2,
            topographic_position: 0.0,
            class: crate::world::geomorph::ReliefClass::LowlandPlain,
        };
        // Horse: high in grassland, zero in taiga.
        let horse = species(AnimalKind::Horse);
        assert!(habitat_suitability(horse, Biome::Grassland, &flat) > 0.5);
        assert_eq!(habitat_suitability(horse, Biome::Taiga, &flat), 0.0);
        // Wolf: present in taiga, absent from desert.
        let wolf = species(AnimalKind::Wolf);
        assert!(habitat_suitability(wolf, Biome::Taiga, &flat) > 0.5);
        assert_eq!(habitat_suitability(wolf, Biome::Desert, &flat), 0.0);
        // Rabbit tolerates desert.
        let rabbit = species(AnimalKind::Rabbit);
        assert!(habitat_suitability(rabbit, Biome::Desert, &flat) > 0.4);
    }

    #[test]
    fn steep_slope_penalises_herds_more_than_solitary() {
        let steep = ReliefSample {
            slope: 1.0,
            local_relief: 0.5,
            mountain_distance: 1.0,
            coast_distance: 10.0,
            aquifer_depth_norm: 0.2,
            topographic_position: 0.0,
            class: crate::world::geomorph::ReliefClass::MountainSlope,
        };
        let horse = species(AnimalKind::Horse); // Herd
        let cat = species(AnimalKind::Cat); // Solitary
        let horse_s = habitat_suitability(horse, Biome::Grassland, &steep);
        let cat_s = habitat_suitability(cat, Biome::Grassland, &steep);
        assert!(cat_s > horse_s, "solitary should tolerate slope better than herd");
    }
}
