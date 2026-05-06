//! Pre-resolved `ResourceId`s for the 22 legacy `Good` variants.
//!
//! Hot paths shouldn't pay for `catalog.id_of("wood")` on every comparison.
//! `init_core_ids(catalog)` is called once at startup and writes the
//! `ResourceId` for each legacy good into a `OnceLock`. Migration call
//! sites read `core_ids::WOOD.get().copied().unwrap()` instead of
//! re-querying.
//!
//! When Phase 2d removes the `Good` enum, this module disappears with it.

use std::sync::OnceLock;

use super::goods::Good;
use super::resource_catalog::{load_resource_catalog, ResourceCatalog, ResourceId};

/// Bridge `Good → ResourceId` so callers don't sprinkle
/// `core_ids::good_to_resource_id(...)` everywhere. Disappears alongside
/// `Good` once Phase 2 residual #7 finishes.
impl From<Good> for ResourceId {
    fn from(g: Good) -> Self {
        good_to_resource_id(g)
    }
}

/// Process-global cache of the loaded `ResourceCatalog`. Populated either
/// by an explicit `install_catalog()` call from `WorldPlugin::build` (the
/// production path) or lazily on first read via `catalog()` (the test
/// path, which doesn't go through the plugin). Both paths converge on the
/// same single load — `OnceLock::get_or_init` ensures no double-parsing.
///
/// Read-only after init: every consumer takes an `&'static ResourceCatalog`,
/// no locks needed in hot paths.
static GLOBAL_CATALOG: OnceLock<ResourceCatalog> = OnceLock::new();

/// Install a freshly-loaded catalog into the process-global slot and
/// populate the per-Good `OnceLock`s. Called by `WorldPlugin::build` so
/// the live game shares a single catalog instance with the legacy `Good`
/// methods. Subsequent calls are no-ops (the static is already populated).
pub fn install_catalog(catalog: ResourceCatalog) {
    init_core_ids(&catalog);
    let _ = GLOBAL_CATALOG.set(catalog);
}

/// Borrow the global catalog. If neither `install_catalog` nor a prior
/// `catalog()` call has run, lazy-loads from `assets/data/resources/`
/// and runs `init_core_ids` as a side effect. Tests that bypass the
/// fixture / plugin (e.g. `simulation/carry.rs::tests`) rely on this
/// path so they don't have to thread setup through every test.
pub fn catalog() -> &'static ResourceCatalog {
    GLOBAL_CATALOG.get_or_init(|| {
        let cat = load_resource_catalog();
        init_core_ids(&cat);
        cat
    })
}

/// Reverse table: index = `ResourceId.0`, value = `Some(Good)` if the id
/// corresponds to one of the 22 legacy goods, `None` otherwise. Built by
/// `init_core_ids`. Capped at 256 — well above the current ~22 entries
/// but small enough to live as a fixed array (no allocation, fits in L1).
const REVERSE_TABLE_LEN: usize = 256;
static REVERSE_TABLE: OnceLock<[Option<Good>; REVERSE_TABLE_LEN]> = OnceLock::new();

/// Catalog-backed display name for a resource, returned with `'static`
/// lifetime so UI call sites can pass it directly to `format!` without
/// owning a `String`. The borrow is sound: `catalog()` returns
/// `&'static ResourceCatalog`, so the inner `String`'s slice lives as
/// long as the global. Used by UI panels (inspector, economy, debug)
/// that today still call `Good::name()`. Phase 2-residual #5.
pub fn display_name(id: ResourceId) -> &'static str {
    catalog()
        .get(id)
        .map(|d| d.display_name.as_str())
        .unwrap_or("?")
}

/// Reverse lookup: `ResourceId` → `Good`, returning `None` for resources
/// that aren't in the legacy enum. Used by migration scaffolding that
/// needs to construct an `Item` (which still carries a `Good` field) from
/// a `ResourceId`-based call site.
pub fn resource_id_to_good(id: ResourceId) -> Option<Good> {
    let idx = id.0 as usize;
    REVERSE_TABLE
        .get()
        .expect("core_ids: resource_id_to_good() called before init_core_ids()")
        .get(idx)
        .copied()
        .flatten()
    // `flatten` collapses `Option<Option<Good>>` from `.get(idx)` + the
    // table's `Option<Good>` slot.
}

macro_rules! core_ids {
    ($( $name:ident => $key:literal => $snake:ident ),+ $(,)?) => {
        $(
            #[allow(non_upper_case_globals)]
            pub static $name: OnceLock<ResourceId> = OnceLock::new();

            /// Lazy snake-case accessor. First call lazy-loads the catalog
            /// and primes every core_ids `OnceLock`; subsequent reads are a
            /// single relaxed atomic load. Use this anywhere code wants
            /// "the ResourceId for <name>" — replaces the legacy
            /// `Good::<Name>.into()` pattern.
            pub fn $snake() -> ResourceId {
                let _ = catalog();
                *$name.get().expect(concat!(
                    "core_ids::", stringify!($snake),
                    "() read before init_core_ids() ran"
                ))
            }
        )+

        /// Resolve every legacy `Good`'s `ResourceId` from the catalog and
        /// install it in the matching `OnceLock`. Also populates the
        /// `ResourceId → Good` reverse table consumed by
        /// `resource_id_to_good`. Idempotent (re-init is a silent no-op
        /// via `OnceLock::set`'s `Result`). Called from `WorldPlugin::build`
        /// after the catalog is inserted.
        pub fn init_core_ids(catalog: &ResourceCatalog) {
            let mut reverse: [Option<Good>; REVERSE_TABLE_LEN] = [None; REVERSE_TABLE_LEN];
            $(
                let id = catalog.id_of($key).unwrap_or_else(|| {
                    panic!(
                        "core_ids: catalog is missing required core resource {:?}. \
                         The RON catalog must define every legacy `Good` variant.",
                        $key
                    )
                });
                let _ = $name.set(id);
                let idx = id.0 as usize;
                assert!(
                    idx < REVERSE_TABLE_LEN,
                    "core_ids: resource id {} for {:?} is past the reverse-table size {}; \
                     bump REVERSE_TABLE_LEN if the catalog grows that large.",
                    idx, $key, REVERSE_TABLE_LEN
                );
                reverse[idx] = Some(Good::$name);
            )+
            // OnceLock::set Err means the table is already populated (e.g.
            // re-init in tests) — we accept the existing assignments since
            // ResourceIds are stable across loads.
            let _ = REVERSE_TABLE.set(reverse);
        }

        /// Lookup table for legacy `Good` → `ResourceId`. Used by
        /// migration scaffolding that still passes `Good` around.
        /// Triggers lazy catalog load on first call so unit tests that
        /// don't go through `WorldPlugin::build` / `TestSim::new` still
        /// work — without this, `Good::*` methods (which call into
        /// here) panic with "read before init_core_ids ran" when used
        /// from non-fixture tests like `simulation::carry::tests`.
        pub fn good_to_resource_id(good: Good) -> ResourceId {
            let _ = catalog();
            match good {
                $( Good::$name => *$name.get().expect(concat!(
                    "core_ids::", stringify!($name),
                    " was read before init_core_ids() ran"
                )), )+
            }
        }
    }
}

core_ids! {
    Fruit => "fruit" => fruit,
    Meat => "meat" => meat,
    Grain => "grain" => grain,
    Wood => "wood" => wood,
    Stone => "stone" => stone,
    Tools => "tools" => tools,
    Cloth => "cloth" => cloth,
    Coal => "coal" => coal,
    Iron => "iron" => iron,
    Luxury => "luxury" => luxury,
    GrainSeed => "grain_seed" => grain_seed,
    Weapon => "weapon" => weapon,
    Armor => "armor" => armor,
    Shield => "shield" => shield,
    Skin => "skin" => skin,
    Copper => "copper" => copper,
    Tin => "tin" => tin,
    Gold => "gold" => gold,
    Silver => "silver" => silver,
    BerrySeed => "berry_seed" => berry_seed,
    ClayTablet => "clay_tablet" => clay_tablet,
    Book => "book" => book,
}
