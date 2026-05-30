//! Deterministic simulation RNG.
//!
//! Every sim-critical random draw in `src/simulation/` routes through
//! [`SimRng`] so the whole simulation is reproducible from its `WorldSeed` —
//! the prerequisite for replays / lockstep multiplayer, and the fix for the
//! residual nondeterminism that `crate::collections` (hashing) did not cover.
//!
//! ## Why derivation, not a shared stream
//!
//! [`SimRng`] holds **only the immutable master seed** and is read as
//! `Res<SimRng>` (never `ResMut`). Each call site builds a *local*
//! [`fastrand::Rng`] seeded from a splitmix64 [`mix`] of stable inputs —
//! `(master, stable_key, tick, site_salt)`. Because every draw is a pure
//! function of those inputs, the outcome is independent of **how many draws
//! preceded it** and of **which thread ran the system**. That is what makes it
//! correct under Bevy's multi-threaded executor and under archetype-shift query
//! reordering — a single shared mutable stream would be order-fragile and would
//! serialise every RNG-using system.
//!
//! ## Using it
//!
//! ```ignore
//! let mut r = sim_rng.for_entity(entity, clock.tick, RngSite::CombatHitRoll);
//! if r.f32() < hit_chance { /* … */ }
//! ```
//!
//! `for_*` returns a [`fastrand::Rng`], so existing call shapes
//! (`.f32()`, `.u8(0..100)`, `.i32(-r..=r)`, `.bool()`, `.shuffle(&mut v)`, …)
//! port one-for-one. Multiple draws at one site reuse the same local `r`: its
//! stream advances locally and is deterministic because the *seed* is stable
//! and no other system shares it.
//!
//! ## Stable-key convention
//!
//! Key on something that does not depend on execution order:
//! - per-agent decisions → the agent [`Entity`] (`for_entity`)
//! - per-tile rolls (plants) → the tile coords (`for_tile`)
//! - spawn-time scatter → a monotonic spawn index / cluster id / faction id
//!   (`for_key`)
//!
//! **Never** key on a running per-tick counter ("draws so far"): that
//! reintroduces the order-fragility this module exists to remove. Init-time
//! sites with no tick pass `tick = 0`.

use bevy::prelude::{Entity, Resource};

// Canonical splitmix64 constants. Shared with `region::home_pick_seed` via
// [`splitmix_finalize`] so there is exactly one mixing primitive in the crate.
const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;
const M1: u64 = 0xBF58_476D_1CE4_E5B9;
const M2: u64 = 0x94D0_49BB_1331_11EB;

/// The splitmix64 finalising avalanche. Pure function — the shared primitive
/// behind both [`mix`] and `region::home_pick_seed`. Extracting it keeps the
/// two byte-for-byte identical without changing either output.
#[inline]
pub fn splitmix_finalize(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(M1);
    x ^= x >> 27;
    x = x.wrapping_mul(M2);
    x ^= x >> 31;
    x
}

/// The one 2-input splitmix64 mixer for the crate. Deterministic, no process
/// entropy. Use this anywhere two stable `u64`s must fold into one seed; never
/// `ahash::AHasher::default()` (per-process keyed).
#[inline]
pub fn mix(a: u64, b: u64) -> u64 {
    splitmix_finalize(a.wrapping_add(GOLDEN).wrapping_mul(M1) ^ b)
}

/// Per-call-site salt. One variant per randomness site so salts are centrally
/// registered, greppable, and collision-free (the project's registry
/// preference over scattered magic consts).
///
/// **Never renumber an existing variant** — the numeric value feeds saved-seed
/// reproducibility, so changing it changes the world. Append new variants with
/// fresh numbers.
#[derive(Clone, Copy, Debug)]
#[repr(u64)]
pub enum RngSite {
    // animals (1..=9)
    AnimalSpawnNeeds = 1,
    AnimalSpawnGrazeTimer = 2,
    AnimalClusterGroupSize = 3,
    AnimalClusterMemberOffset = 4,
    AnimalReproductionBirth = 5,
    AnimalClusterCenterShuffle = 6,
    AnimalMisc = 7,
    // plants (10..=19)
    PlantSproutRoll = 10,
    PlantScatterChance = 11,
    PlantScatterOffset = 12,
    PlantFruitRoll = 13,
    PlantWildKindPick = 14,
    PlantMisc = 15,
    // reproduction (20..=29)
    ReproConception = 20,
    ReproSex = 21,
    // combat (30..=39)
    CombatHitRoll = 30,
    CombatArmorCoverage = 31,
    CombatMiscRoll = 32,
    // knowledge (40..=49)
    KnowledgeDiscovery = 40,
    KnowledgeTeaching = 41,
    // goals / htn / gather (50..=59)
    GoalSocialPick = 50,
    GoalPlayPick = 51,
    HtnExploreOffsetA = 52,
    HtnExploreOffsetB = 53,
    GatherExploreOffset = 54,
    // person / stats (60..=69)
    PersonDisposition = 60,
    PersonNamePick = 61,
    PersonSpawnHelperA = 62,
    PersonSpawnHelperB = 63,
    StatsRoll = 64,
    StatsVariance = 65,
    PersonPersonality = 66,
    PersonSkinTone = 67,
    PersonHairColor = 68,
    // movement thread_rng replacements (70..=79)
    MovementNudgeA = 70,
    MovementNudgeB = 71,
    // wild_herd / nomad / vehicle (80..=89)
    WildHerdNeeds = 80,
    NomadJitter = 81,
    VehicleHitCellPick = 82,
}

/// Immutable master RNG seed for the simulation, derived from `WorldSeed`.
///
/// See the module docs: read as `Res<SimRng>`, build a local
/// [`fastrand::Rng`] per draw via [`SimRng::for_entity`] / [`SimRng::for_tile`]
/// / [`SimRng::for_key`].
#[derive(Resource, Clone, Copy, Debug)]
pub struct SimRng {
    master: u64,
}

impl SimRng {
    /// Derive the master seed from the world seed. Salted distinct from
    /// `collections::FIXED_SEEDS` and the net client-id domain so the three
    /// determinism domains can never be confused.
    pub fn from_world_seed(world_seed: u64) -> Self {
        const SIM_RNG_DOMAIN: u64 = 0x5349_4D52_4E47_5F00; // b"SIMRNG\0\0"
        Self {
            master: mix(world_seed, SIM_RNG_DOMAIN),
        }
    }

    /// Local RNG for an entity-keyed draw at `tick`.
    #[inline]
    pub fn for_entity(&self, entity: Entity, tick: u64, site: RngSite) -> fastrand::Rng {
        self.local(mix(entity.to_bits(), mix(tick, site as u64)))
    }

    /// Local RNG for a tile-keyed draw at `tick`. Packs `(x, y)` into one key.
    #[inline]
    pub fn for_tile(&self, tile: (i32, i32), tick: u64, site: RngSite) -> fastrand::Rng {
        let key = ((tile.0 as u32 as u64) << 32) | (tile.1 as u32 as u64);
        self.local(mix(key, mix(tick, site as u64)))
    }

    /// Local RNG for an arbitrary stable key (spawn index, cluster id, faction
    /// id, …). Init/OnEnter sites with no tick pass `tick = 0`.
    #[inline]
    pub fn for_key(&self, key: u64, tick: u64, site: RngSite) -> fastrand::Rng {
        self.local(mix(key, mix(tick, site as u64)))
    }

    #[inline]
    fn local(&self, k: u64) -> fastrand::Rng {
        fastrand::Rng::with_seed(mix(self.master, k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_is_deterministic_and_input_sensitive() {
        assert_eq!(mix(1, 2), mix(1, 2));
        assert_ne!(mix(1, 2), mix(2, 1));
        assert_ne!(mix(1, 2), mix(1, 3));
    }

    #[test]
    fn same_seed_and_key_reproduce() {
        let a = SimRng::from_world_seed(42);
        let b = SimRng::from_world_seed(42);
        let e = Entity::from_raw(7);
        assert_eq!(
            a.for_entity(e, 100, RngSite::CombatHitRoll).f32(),
            b.for_entity(e, 100, RngSite::CombatHitRoll).f32(),
        );
    }

    #[test]
    fn different_seed_diverges() {
        let a = SimRng::from_world_seed(42);
        let b = SimRng::from_world_seed(43);
        let e = Entity::from_raw(7);
        assert_ne!(
            a.for_entity(e, 100, RngSite::CombatHitRoll).f32(),
            b.for_entity(e, 100, RngSite::CombatHitRoll).f32(),
        );
    }

    #[test]
    fn site_salt_decorrelates() {
        let r = SimRng::from_world_seed(42);
        let e = Entity::from_raw(7);
        // Same entity+tick, different site → independent draws.
        assert_ne!(
            r.for_entity(e, 5, RngSite::CombatHitRoll).f32(),
            r.for_entity(e, 5, RngSite::CombatArmorCoverage).f32(),
        );
    }

    /// Guardrail: no NEW global `fastrand::` draws or `thread_rng()` may appear
    /// in `src/simulation/`. While `ENFORCE` is false (migration in progress)
    /// this only reports the remaining sites; flip it to `true` once migration
    /// is complete (plan Phase 12) to fail the build on regressions.
    ///
    /// Cosmetic randomness in `src/rendering/` and `src/ui/` is intentionally
    /// out of scope and not scanned.
    #[test]
    fn no_global_fastrand_in_simulation() {
        use std::fmt::Write as _;
        use std::path::Path;

        const ENFORCE: bool = true;
        // Banned global free-function draws. `fastrand::Rng::` (local
        // instances) and `fastrand::seed` (the about-to-be-removed test crutch)
        // are not flagged here.
        const BANNED: &[&str] = &[
            "fastrand::f32(",
            "fastrand::f64(",
            "fastrand::u8(",
            "fastrand::u16(",
            "fastrand::u32(",
            "fastrand::u64(",
            "fastrand::i32(",
            "fastrand::usize(",
            "fastrand::bool(",
            "fastrand::shuffle(",
            "fastrand::choice(",
            "thread_rng(",
        ];

        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/simulation");
        let mut report = String::new();
        let mut total = 0usize;
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("read src/simulation")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();
        for path in entries {
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Skip this module (defines the allowed primitives) and the test
            // fixture (test scaffolding — randomized setup is out of the
            // production-sim determinism scope; it also owns the `fastrand::seed`
            // crutch until Phase 12).
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "sim_rng.rs" || name == "test_fixture.rs" {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap_or_default();
            let mut file_hits = 0usize;
            for (lineno, line) in src.lines().enumerate() {
                if BANNED.iter().any(|b| line.contains(b)) {
                    file_hits += 1;
                    let _ = writeln!(report, "  {}:{}: {}", name, lineno + 1, line.trim());
                }
            }
            total += file_hits;
        }

        if total > 0 {
            let msg = format!(
                "{} global fastrand/thread_rng site(s) remain in src/simulation:\n{}",
                total, report
            );
            if ENFORCE {
                panic!("{msg}");
            } else {
                eprintln!("[sim_rng guardrail / WARN] {msg}");
            }
        }
    }
}
