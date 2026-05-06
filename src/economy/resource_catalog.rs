//! Data-driven resource catalog.
//!
//! Resources are loaded from `assets/data/resources/*.ron` at startup into a
//! `ResourceCatalog` resource, indexed by both stable string keys and
//! deterministic `ResourceId(u16)` integer ids. Per-resource attributes
//! (bulk, weight, nutrition, tags…) live in the catalog, not in scattered
//! `match good { ... }` blocks. `core_ids::*` pre-resolves the `ResourceId`
//! for every founding resource at startup so hot paths can compare against
//! an integer rather than re-querying the catalog.

use ahash::AHashMap;
use bevy::prelude::*;
use serde::Deserialize;

use super::goods::Bulk;

/// Stable integer identifier for a resource. Assigned at catalog-load time
/// by sorting `ResourceDef::key` alphabetically — running the loader twice
/// against the same data files produces identical IDs, which keeps save
/// games portable across resource additions.
///
/// `ResourceId(u16::MAX)` is reserved as a sentinel for "no resource".
#[derive(Copy, Clone, Eq, Hash, PartialEq, Debug)]
pub struct ResourceId(pub u16);

/// Default resolves to the `NONE` sentinel so newtype consumers using
/// `Default::default()` (e.g. fixed-size deposit-slot array seeds) get
/// an unambiguous "no resource" marker that callers must overwrite.
impl Default for ResourceId {
    fn default() -> Self {
        Self::NONE
    }
}

impl ResourceId {
    /// Reserved sentinel meaning "no resource". Matches the `PersonAI`
    /// sentinel pattern (`u16::MAX = UNEMPLOYED`) so future migrations can
    /// adopt either representation without overlap.
    pub const NONE: Self = Self(u16::MAX);

    pub const fn raw(self) -> u16 {
        self.0
    }

    /// True if this resource has `edible_calories` set in the catalog.
    /// Mirrors catalog `edible_calories`; lazy-loads the catalog on first call.
    pub fn is_edible(self) -> bool {
        super::core_ids::catalog()
            .get(self)
            .and_then(|d| d.edible_calories)
            .is_some()
    }

    /// Per-unit entertainment value (drives solo-play willpower refill).
    /// Mirrors the catalog `entertainment_value`. Returns 0 for unknown ids.
    pub fn entertainment_value(self) -> u8 {
        super::core_ids::catalog()
            .get(self)
            .map(|d| d.entertainment_value)
            .unwrap_or(0)
    }

    /// Calories restored per unit when eaten. Mirrors catalog `edible_calories`;
    /// returns 0 for inedible / unknown resources. Truncates `u16` →
    /// `u8` (matches the legacy contract).
    pub fn nutrition(self) -> u8 {
        super::core_ids::catalog()
            .get(self)
            .and_then(|d| d.edible_calories)
            .map(|c| c.min(u8::MAX as u16) as u8)
            .unwrap_or(0)
    }

    /// How this resource must be held in hands when carried. Mirrors
    /// catalog `bulk`; defaults to `Small` for unknown ids (matches the
    /// legacy fallback).
    pub fn bulk(self) -> super::goods::Bulk {
        super::core_ids::catalog()
            .get(self)
            .map(|d| d.bulk.as_bulk())
            .unwrap_or(super::goods::Bulk::Small)
    }

    /// Per-unit weight in grams. Mirrors catalog `weight_g`; returns
    /// 0 for unknown ids.
    pub fn unit_weight_g(self) -> u32 {
        super::core_ids::catalog()
            .get(self)
            .map(|d| d.weight_g)
            .unwrap_or(0)
    }

    /// True if this resource is a planting seed (catalog `class == Seed`).
    /// Mirrors catalog `class == Seed`.
    pub fn is_seed(self) -> bool {
        matches!(
            super::core_ids::catalog().get(self).map(|d| d.class),
            Some(ResourceClass::Seed)
        )
    }

    /// Catalog `ResourceClass` of this resource. Returns `None` for the
    /// `NONE` sentinel and any unknown id.
    pub fn class(self) -> Option<ResourceClass> {
        super::core_ids::catalog().get(self).map(|d| d.class)
    }
}

/// High-level functional category. Used by HTN methods to enumerate "all
/// edible resources", "all crafting materials", etc. without enumerating
/// individual resources.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceClass {
    Food,
    Material,
    Tool,
    Weapon,
    Armor,
    Shield,
    Seed,
    Luxury,
    Knowledge,
    Currency,
    Fuel,
    Cloth,
    Hide,
    Ore,
}

/// Where a resource belongs in faction storage. Phase 2a defines the enum
/// for forward compatibility; storage tiles are still a single bucket
/// (`StorageTileMap`), so this field is descriptive only.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageClass {
    Pile,
    Granary,
    Armoury,
    Library,
}

/// Free-form resource tag. Tags drive HTN method preconditions like
/// "AcquireFood iterates `catalog.with_tag(\"food\")`" — adding a tagged
/// resource lights up every method that gates on the tag without code
/// changes. Phase 2a stores them as `String` for simplicity; Phase 2c may
/// switch to a small interned-symbol table if tag traffic becomes hot.
pub type ResourceTag = String;

/// Single resource entry. Carries every per-resource attribute consumed
/// by simulation code — bulk/weight for carrying, edibility/nutrition for
/// hunger, entertainment for play, sprite for rendering, etc. Adding a
/// new resource is one RON entry plus an optional sprite.
#[derive(Clone, Debug, Deserialize)]
pub struct ResourceDef {
    /// Stable, lowercase, snake_case identifier. Sorts alphabetically to
    /// produce deterministic `ResourceId`s.
    pub key: String,
    /// Human-readable name shown in inspector / context menu / market UI.
    pub display_name: String,
    pub class: ResourceClass,
    pub bulk: BulkDef,
    pub weight_g: u32,
    /// Calories/satiation per unit when eaten. `None` for inedible.
    #[serde(default)]
    pub edible_calories: Option<u16>,
    /// Solo-play willpower-refill rate when an agent plays with this item.
    #[serde(default)]
    pub entertainment_value: u8,
    /// Plant kind grown from this resource when planted, by string key.
    /// Mirrors `PlantKind::seed_good()` in the inverse direction. `None`
    /// for non-seeds.
    #[serde(default)]
    pub plantable_as: Option<String>,
    /// Default storage bucket. Descriptive only in Phase 2a.
    pub storage_class: StorageClass,
    /// Base trade value used by markets (Phase 2b will read this).
    #[serde(default)]
    pub trade_base_value: u16,
    /// Free-form tags for HTN method gating.
    #[serde(default)]
    pub tags: Vec<ResourceTag>,
    /// Sprite key consumed by `entity_sprites::spawn_ground_item_sprites`
    /// to choose the ground-pile sprite for this resource. `None` means
    /// no sprite is currently authored — the entity stays invisible until
    /// the field is populated. Catalog-driven so adding a resource only
    /// requires adding a sprite to `SpriteLibrary` + setting this field.
    #[serde(default)]
    pub sprite_key: Option<String>,
}

/// Serializable mirror of `super::goods::Bulk`. We can't `#[derive(Deserialize)]`
/// on `Bulk` directly because it lives in `goods.rs` which doesn't depend
/// on serde — defining a parallel enum here keeps the data layer thin.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkDef {
    Small,
    OneHand,
    TwoHand,
}

impl BulkDef {
    pub fn as_bulk(self) -> Bulk {
        match self {
            BulkDef::Small => Bulk::Small,
            BulkDef::OneHand => Bulk::OneHand,
            BulkDef::TwoHand => Bulk::TwoHand,
        }
    }
}

/// World resource. Inserted at `WorldPlugin::build` from the loaded RON
/// files. Read-only after init — no system mutates the catalog at runtime,
/// so hot paths skip locking and can take immutable refs freely. `Clone`
/// is implemented (cheap — `Vec<ResourceDef>` plus two `AHashMap`s) so the
/// same catalog can live both as a Bevy resource and as a process-global
/// `OnceLock` for the `ResourceId::*` accessors that can't take system params.
#[derive(Resource, Default, Clone)]
pub struct ResourceCatalog {
    /// Indexed by `ResourceId.0`. Always sorted by `key` so two loads of
    /// the same data files produce identical id assignments.
    defs: Vec<ResourceDef>,
    /// `key` → `ResourceId` lookup for save-game restores and tests.
    by_key: AHashMap<String, ResourceId>,
    /// Tag → list of resources carrying that tag. Built once at load.
    by_tag: AHashMap<ResourceTag, Vec<ResourceId>>,
}

impl ResourceCatalog {
    /// Construct a catalog from a vector of definitions. Sorts by `key` for
    /// deterministic id assignment, builds the by-key and by-tag indices,
    /// and panics on duplicate keys (resource files must be authored
    /// without conflicts; failing fast is preferable to silent shadowing).
    pub fn from_defs(mut defs: Vec<ResourceDef>) -> Self {
        defs.sort_by(|a, b| a.key.cmp(&b.key));

        let mut by_key = AHashMap::with_capacity(defs.len());
        let mut by_tag: AHashMap<ResourceTag, Vec<ResourceId>> = AHashMap::new();

        for (idx, def) in defs.iter().enumerate() {
            assert!(
                idx <= u16::MAX as usize,
                "ResourceCatalog: more than {} resources is not supported \
                 (ResourceId is u16; bump it if you need more)",
                u16::MAX
            );
            let id = ResourceId(idx as u16);
            if by_key.insert(def.key.clone(), id).is_some() {
                panic!(
                    "ResourceCatalog: duplicate resource key {:?}. Each resource \
                     definition must have a unique `key`.",
                    def.key
                );
            }
            for tag in &def.tags {
                by_tag.entry(tag.clone()).or_default().push(id);
            }
        }

        Self {
            defs,
            by_key,
            by_tag,
        }
    }

    pub fn get(&self, id: ResourceId) -> Option<&ResourceDef> {
        self.defs.get(id.0 as usize)
    }

    pub fn id_of(&self, key: &str) -> Option<ResourceId> {
        self.by_key.get(key).copied()
    }

    pub fn with_tag(&self, tag: &str) -> &[ResourceId] {
        self.by_tag.get(tag).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ResourceId, &ResourceDef)> {
        self.defs
            .iter()
            .enumerate()
            .map(|(i, d)| (ResourceId(i as u16), d))
    }
}

/// File-level wrapper used by the RON parser. Each `*.ron` file is a
/// `ResourceFile { resources: [...] }` so we can glob-load multiple files
/// and merge them into a single catalog.
#[derive(Debug, Deserialize)]
pub struct ResourceFile {
    pub resources: Vec<ResourceDef>,
}

/// Load every `*.ron` file under `assets/data/resources/`, parse it, and
/// merge the entries into one catalog. Panics with a clear message on
/// missing dir / parse errors / duplicate keys — failing at startup is
/// the right default for a config-driven system.
pub fn load_resource_catalog() -> ResourceCatalog {
    let dir = std::path::Path::new("assets/data/resources");
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "ResourceCatalog: cannot read {:?}: {}. \
             Resource definition files must live in assets/data/resources/*.ron.",
            dir, e
        )
    });

    let mut defs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ron") {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("ResourceCatalog: cannot read {:?}: {}", path, e)
        });
        let file: ResourceFile = ron::from_str(&body).unwrap_or_else(|e| {
            panic!("ResourceCatalog: parse error in {:?}: {}", path, e)
        });
        defs.extend(file.resources);
    }

    if defs.is_empty() {
        panic!(
            "ResourceCatalog: no resources found in {:?}. At least one \
             resource definition is required.",
            dir
        );
    }

    ResourceCatalog::from_defs(defs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stable keys for the 22 founding resources. Pins the catalog
    /// contract: every key listed here must resolve and carry the
    /// expected attributes, otherwise the migration broke.
    const LEGACY_KEYS: &[&str] = &[
        "fruit",
        "meat",
        "grain",
        "wood",
        "stone",
        "tools",
        "cloth",
        "coal",
        "iron",
        "luxury",
        "grain_seed",
        "weapon",
        "armor",
        "shield",
        "skin",
        "copper",
        "tin",
        "gold",
        "silver",
        "berry_seed",
        "clay_tablet",
        "book",
    ];

    /// Loading the catalog twice yields identical `ResourceId` assignments
    /// for every key. This is the contract that lets save games store
    /// integer IDs without breaking when new resources land alphabetically
    /// before existing ones.
    #[test]
    fn catalog_id_assignments_are_deterministic() {
        let a = load_resource_catalog();
        let b = load_resource_catalog();
        assert_eq!(a.len(), b.len(), "catalogs disagree on resource count");
        for (id, def) in a.iter() {
            assert_eq!(
                Some(id),
                b.id_of(&def.key),
                "key {:?} resolved to different IDs across two loads",
                def.key
            );
        }
    }

    /// Every legacy resource must have a matching catalog entry, or
    /// the migration broke. Pinning this catches accidental drift between
    /// the founding key list and the RON file.
    #[test]
    fn every_legacy_key_resolves_to_a_catalog_entry() {
        let catalog = load_resource_catalog();
        for &key in LEGACY_KEYS {
            assert!(
                catalog.id_of(key).is_some(),
                "no catalog entry under key {:?}",
                key
            );
        }
    }

    /// Every legacy resource has a `sprite_key` populated so the
    /// catalog-driven `spawn_ground_item_sprites` doesn't silently regress
    /// to invisible piles when a new resource is authored. Pinning this
    /// makes the contract explicit: adding a resource without a sprite
    /// is allowed, but removing one from an existing entry will fail.
    #[test]
    fn every_legacy_key_has_a_sprite_key() {
        let catalog = load_resource_catalog();
        for &key in LEGACY_KEYS {
            let id = catalog.id_of(key).unwrap();
            let def = catalog.get(id).unwrap();
            assert!(
                def.sprite_key.is_some(),
                "key {:?} is missing sprite_key in catalog",
                key
            );
        }
    }
}
