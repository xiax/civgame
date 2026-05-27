//! Per-species plant sprite loading + lookup (Phase 6 of biome-native plants
//! follow-ups). Mirrors `pixel_art::AnimalTextures`: walk the catalog at
//! startup, probe `assets/textures/plants/<folder>/<stage>_<variant>.png`,
//! cache `Handle<Image>` per `(species, stage, variant)`. Missing PNGs fall
//! through to a per-`PlantForm` shared fallback layer
//! (`assets/textures/plants/_form_<form>/...`), then to the legacy ASCII
//! sprites in `EntityTextures` at resolve time.
//!
//! The resolver returns `None` when no per-species or per-form PNG slot is
//! present; the caller (`entity_sprites::resolve_plant_sprite`) then routes
//! to legacy ASCII. This file does not own the legacy fallback handles.

use ahash::AHashMap;
use bevy::prelude::*;

use crate::simulation::plant_catalog::{PlantCatalog, PlantForm, PlantSpeciesId};
use crate::simulation::plants::GrowthStage;

/// Stage axis size — matches `GrowthStage as u8` count.
const STAGE_COUNT: usize = 5;

/// One stage's variant slots. `variant_count == 0` means "no PNGs authored
/// for this (species, stage)"; resolver should fall through to form
/// fallback or legacy ASCII. Variants beyond `variant_count` reuse slot 0.
#[derive(Default, Clone)]
pub struct PlantStageSlot {
    pub variants: [Option<Handle<Image>>; 3],
    pub variant_count: u8,
}

impl PlantStageSlot {
    pub fn handle_for(&self, variant: u8) -> Option<Handle<Image>> {
        if self.variant_count == 0 {
            return None;
        }
        let idx = (variant as usize) % (self.variant_count as usize);
        self.variants[idx].clone()
    }
}

#[derive(Resource, Default)]
pub struct PlantSpriteSet {
    by_species: AHashMap<PlantSpeciesId, [PlantStageSlot; STAGE_COUNT]>,
    by_form: AHashMap<PlantForm, [PlantStageSlot; STAGE_COUNT]>,
}

impl PlantSpriteSet {
    pub fn lookup_species(
        &self,
        species: PlantSpeciesId,
        stage: GrowthStage,
    ) -> Option<&PlantStageSlot> {
        let slots = self.by_species.get(&species)?;
        let slot = &slots[stage as usize];
        if slot.variant_count == 0 {
            None
        } else {
            Some(slot)
        }
    }

    pub fn lookup_form(&self, form: PlantForm, stage: GrowthStage) -> Option<&PlantStageSlot> {
        let slots = self.by_form.get(&form)?;
        let slot = &slots[stage as usize];
        if slot.variant_count == 0 {
            None
        } else {
            Some(slot)
        }
    }
}

/// Stage axis: how many variants we probe per stage and the filename stem.
/// Seed/Harvested are single-variant by design (sprout/stubble doesn't
/// warrant authoring 3 distinct silhouettes).
const STAGE_LAYOUT: [(GrowthStage, &str, u8); STAGE_COUNT] = [
    (GrowthStage::Seed, "seed", 1),
    (GrowthStage::Seedling, "seedling", 3),
    (GrowthStage::Harvested, "harvested", 1),
    (GrowthStage::Mature, "mature", 3),
    (GrowthStage::Overripe, "overripe", 3),
];

const ASSET_ROOT_REL: &str = "textures/plants";
const ASSET_ROOT_FS: &str = "assets/textures/plants";

/// Probe `assets/textures/plants/<folder>/<stem>[_<variant>].png` for each
/// `(species, stage, variant)` and `_form_<form>/...` for the form fallback
/// layer. Files that don't exist on disk are skipped so the resolver can
/// route to a higher fallback tier — Bevy's `AssetServer::load` would
/// happily return a handle for a missing path and log warnings on every
/// frame, which is exactly what we don't want here.
pub fn load_plant_sprites(asset_server: &AssetServer, catalog: &PlantCatalog) -> PlantSpriteSet {
    let mut set = PlantSpriteSet::default();

    for def in catalog.iter() {
        let folder = def.sprite_folder();
        let species_variant_cap = def.sprite_variants();
        let mut stage_slots: [PlantStageSlot; STAGE_COUNT] = Default::default();
        let mut any_loaded = false;

        for (idx, (_stage, stem, max_variants)) in STAGE_LAYOUT.iter().enumerate() {
            let effective_max = (*max_variants).min(species_variant_cap).max(1);
            let mut slot = PlantStageSlot::default();
            for v in 0..effective_max {
                let asset_path = sprite_asset_path(folder, stem, *max_variants, v);
                let fs_path = format!("{ASSET_ROOT_FS}/{folder}/{}", file_name(stem, *max_variants, v));
                if std::path::Path::new(&fs_path).exists() {
                    slot.variants[v as usize] = Some(asset_server.load(&asset_path));
                    slot.variant_count = slot.variant_count.max(v + 1);
                    any_loaded = true;
                }
            }
            // If only some variants were authored, fill gaps with slot 0
            // so `handle_for(variant)` mod-indexing always lands somewhere
            // sensible.
            if slot.variant_count > 0 && slot.variant_count < 3 {
                let first = slot.variants[0].clone();
                for v in (slot.variant_count as usize)..3 {
                    if slot.variants[v].is_none() {
                        slot.variants[v] = first.clone();
                    }
                }
            }
            stage_slots[idx] = slot;
        }
        if any_loaded {
            set.by_species.insert(def.id, stage_slots);
        }
    }

    for form in [
        PlantForm::Grass,
        PlantForm::Forb,
        PlantForm::Shrub,
        PlantForm::Vine,
        PlantForm::Tree,
        PlantForm::Aquatic,
        PlantForm::Cactus,
        PlantForm::Tuber,
    ] {
        let folder = format!("_form_{}", form_slug(form));
        let mut stage_slots: [PlantStageSlot; STAGE_COUNT] = Default::default();
        let mut any_loaded = false;
        for (idx, (_stage, stem, max_variants)) in STAGE_LAYOUT.iter().enumerate() {
            let mut slot = PlantStageSlot::default();
            for v in 0..*max_variants {
                let asset_path = sprite_asset_path(&folder, stem, *max_variants, v);
                let fs_path = format!("{ASSET_ROOT_FS}/{folder}/{}", file_name(stem, *max_variants, v));
                if std::path::Path::new(&fs_path).exists() {
                    slot.variants[v as usize] = Some(asset_server.load(&asset_path));
                    slot.variant_count = slot.variant_count.max(v + 1);
                    any_loaded = true;
                }
            }
            if slot.variant_count > 0 && slot.variant_count < 3 {
                let first = slot.variants[0].clone();
                for v in (slot.variant_count as usize)..3 {
                    if slot.variants[v].is_none() {
                        slot.variants[v] = first.clone();
                    }
                }
            }
            stage_slots[idx] = slot;
        }
        if any_loaded {
            set.by_form.insert(form, stage_slots);
        }
    }

    set
}

fn form_slug(form: PlantForm) -> &'static str {
    match form {
        PlantForm::Grass => "grass",
        PlantForm::Forb => "forb",
        PlantForm::Shrub => "shrub",
        PlantForm::Vine => "vine",
        PlantForm::Tree => "tree",
        PlantForm::Aquatic => "aquatic",
        PlantForm::Cactus => "cactus",
        PlantForm::Tuber => "tuber",
    }
}

fn file_name(stem: &str, max_variants: u8, variant: u8) -> String {
    if max_variants == 1 {
        format!("{stem}.png")
    } else {
        format!("{stem}_{}.png", variant)
    }
}

fn sprite_asset_path(folder: &str, stem: &str, max_variants: u8, variant: u8) -> String {
    format!("{ASSET_ROOT_REL}/{folder}/{}", file_name(stem, max_variants, variant))
}

pub fn setup_plant_sprites(asset_server: Res<AssetServer>, mut set: ResMut<PlantSpriteSet>) {
    let catalog = crate::simulation::plant_catalog::catalog();
    *set = load_plant_sprites(&asset_server, catalog);
}
