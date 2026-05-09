//! Pack/deploy abstraction for nomadic structures.
//!
//! Every nomadic structure exists in two forms:
//! - **Deployed**: a tile entity (`Bed`/`TentShelter`/`PackBundle`/...) carrying
//!   a [`Deployable`] component.
//! - **Packed**: a `ResourceId` good in a member's `Equipment` or on a
//!   pack-animal — the entity has been despawned and its packed-form good
//!   added to inventory.
//!
//! Phase 8 will add `Task::PackCamp` / `Task::DeployItem` and the helper fns
//! that drive the conversion. Phase 2 just lays the type down so finalize sites
//! can stamp it on Bedrolls (and later Tents/Yurts/PackBundles).

use bevy::prelude::*;

use crate::economy::resource_catalog::ResourceId;

/// Marks a tile entity as a packable nomadic structure. When the band
/// migrates, [`pack_deployable`] (Phase 8) consumes the entity and either:
/// - adds `packed_form` to a target inventory (when `Some`) — full carry,
///   no material loss; OR
/// - drops `refund_pct` of the recipe inputs as `GroundItem`s and despawns
///   (when `packed_form` is `None`) — sticks-and-leaves teardown.
///
/// Held by Bedrolls (always packable), Tents (50% refund only, no carry),
/// Yurts (full carry via `PackedYurt` good), and PackBundles.
#[derive(Component, Clone, Copy, Debug)]
pub struct Deployable {
    /// `Some(rid)` = packs into this resource good when the camp moves.
    /// `None` = not packable; teardown drops `refund_pct` of inputs and
    /// despawns the entity.
    pub packed_form: Option<ResourceId>,
    /// Fraction of recipe inputs returned as `GroundItem`s on teardown
    /// when `packed_form == None`. 0.0 = clean despawn, 1.0 = full refund.
    /// Ignored when `packed_form` is set.
    pub refund_pct: f32,
}

impl Deployable {
    /// Bedroll-style: full carry, no material refund needed (the bedroll
    /// good itself is the packed form).
    pub fn fully_packable(packed: ResourceId) -> Self {
        Self {
            packed_form: Some(packed),
            refund_pct: 0.0,
        }
    }

    /// Tent-style: deployed-only; teardown drops half the materials at the
    /// old camp tile.
    pub fn refund_only(refund_pct: f32) -> Self {
        Self {
            packed_form: None,
            refund_pct: refund_pct.clamp(0.0, 1.0),
        }
    }
}
