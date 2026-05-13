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
/// migrates, the commit pass consumes the entity and either:
/// - adds `packed_form` to a target inventory (when `Some`) — full carry,
///   no material loss; OR
/// - drops `floor(refund_qty * refund_pct)` of `refund_resource` as a
///   `GroundItem` at the entity's tile and despawns (when `packed_form` is
///   `None`) — sticks-and-leaves teardown.
///
/// Held by Bedrolls (always packable), Tents (refund-only, drops half
/// their wood on teardown), Yurts (full carry via `PackedYurt` good).
///
/// Phase 4: `packed_bundles` lets a structure pack into *multiple*
/// liftable bundles (e.g. Yurt → 2× yurt_frame_bundle + 2×
/// yurt_cover_bundle) rather than a single 80 kg good no member can
/// carry. `packed_form` remains the legacy single-good path; new
/// structures (Yurt v2) prefer `packed_bundles`.
#[derive(Component, Clone, Debug)]
pub struct Deployable {
    /// `Some(rid)` = packs into this resource good when the camp moves.
    /// `None` = not packable; teardown drops a refund and despawns.
    pub packed_form: Option<ResourceId>,
    /// Phase 4: bundled pack form — pack into N copies of M different
    /// resources rather than a single mega-good. Empty = legacy
    /// `packed_form` path. Yurts use this so frame bundles and cover
    /// bundles can be distributed across multiple carriers.
    pub packed_bundles: Vec<(ResourceId, u32)>,
    /// Fraction of `refund_qty` returned as `GroundItem`s on teardown
    /// when `packed_form == None`. 0.0 = clean despawn, 1.0 = full refund.
    /// Ignored when `packed_form` is set.
    pub refund_pct: f32,
    /// Resource id of the refund drop. `None` = no refund (clean despawn).
    pub refund_resource: Option<ResourceId>,
    /// Base refund quantity (typically the matching recipe input). Multiplied
    /// by `refund_pct` at teardown to get the actual ground-item qty.
    pub refund_qty: u8,
}

impl Deployable {
    /// Bedroll / Yurt-style: full carry, no material refund needed (the
    /// packed-form good itself is the carried representation).
    pub fn fully_packable(packed: ResourceId) -> Self {
        Self {
            packed_form: Some(packed),
            packed_bundles: Vec::new(),
            refund_pct: 0.0,
            refund_resource: None,
            refund_qty: 0,
        }
    }

    /// Phase 4: yurt-style bundled pack — splits into N pieces so
    /// individual members / pack animals can carry one each.
    pub fn bundled(bundles: Vec<(ResourceId, u32)>) -> Self {
        Self {
            packed_form: None,
            packed_bundles: bundles,
            refund_pct: 0.0,
            refund_resource: None,
            refund_qty: 0,
        }
    }

    /// Tent-style: deployed-only; teardown drops `refund_pct` of
    /// `refund_qty` units of `refund_resource` at the entity's tile.
    pub fn refund_only(refund_pct: f32, refund_resource: ResourceId, refund_qty: u8) -> Self {
        Self {
            packed_form: None,
            packed_bundles: Vec::new(),
            refund_pct: refund_pct.clamp(0.0, 1.0),
            refund_resource: Some(refund_resource),
            refund_qty,
        }
    }

    /// Compute the actual `(resource, qty)` to drop on teardown. Returns
    /// `None` for fully-packable forms (no drop) or zero-qty refunds.
    pub fn compute_refund_drop(&self) -> Option<(ResourceId, u32)> {
        let res = self.refund_resource?;
        let qty = (self.refund_qty as f32 * self.refund_pct).floor() as u32;
        if qty == 0 {
            return None;
        }
        Some((res, qty))
    }
}
