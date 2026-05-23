//! Hand-drawn pixel sprites for every vehicle part kind, visually distinct
//! variants, and weapon-module composites.
//!
//! Same `SpriteLibrary` + `ascii_to_image` pipeline as the rest of
//! `sprite_library.rs`; kept in its own file so the existing 5.5k-line
//! library doesn't grow another 1k lines of templates.
//!
//! Naming:
//! - Per-cell parts: `vehicle_<kind>_<variant_or_base>_<view>`
//!   (e.g. `vehicle_wheel_spoked_side`, `vehicle_frame_base_front`).
//! - Multi-cell weapon modules: `vehicle_module_<module_label>_<view>`.
//!
//! Heading→view: side sprite drawn facing east (W flips it horizontally),
//! front sprite drawn facing south; N and S share the front view.

use crate::rendering::pixel_art::{ascii_to_image, WARM_PALETTE};
use crate::rendering::sprite_library::SpriteLibrary;
use crate::simulation::vehicle::VehiclePartKind;
use bevy::math::IVec3;
use bevy::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VehicleSpriteView {
    Side,
    Front,
}

impl VehicleSpriteView {
    fn token(self) -> &'static str {
        match self {
            VehicleSpriteView::Side => "side",
            VehicleSpriteView::Front => "front",
        }
    }
}

/// Map a 0..4 heading to `(view, flip_x)`. Heading convention from
/// `VehicleFootprint::offsets_by_heading`: 0=N, 1=W, 2=S, 3=E. With two
/// views (no back sprite), N and S both render the front; E/W share the
/// side sprite with W flipped horizontally.
pub fn view_for_heading(heading: u8) -> (VehicleSpriteView, bool) {
    match heading % 4 {
        0 => (VehicleSpriteView::Front, false),
        1 => (VehicleSpriteView::Side, true),
        2 => (VehicleSpriteView::Front, false),
        _ => (VehicleSpriteView::Side, false),
    }
}

fn kind_token(kind: VehiclePartKind) -> &'static str {
    match kind {
        VehiclePartKind::Frame => "frame",
        VehiclePartKind::Deck => "deck",
        VehiclePartKind::Wall => "wall",
        VehiclePartKind::Axle => "axle",
        VehiclePartKind::Wheel => "wheel",
        VehiclePartKind::Hitch => "hitch",
        VehiclePartKind::Yoke => "yoke",
        VehiclePartKind::CargoBay => "cargo_bay",
        VehiclePartKind::CrewSeat => "crew_seat",
        VehiclePartKind::WeaponMount => "weapon_mount",
        VehiclePartKind::Engine => "engine",
        VehiclePartKind::Track => "track",
        VehiclePartKind::ArmorPlate => "armor_plate",
        VehiclePartKind::Turret => "turret",
    }
}

/// Sprite key for one vehicle cell. `variant_label = None` returns the
/// per-kind base key (`..._base_<view>`). Callers should try the variant
/// key first and fall back to the base key if the variant has no distinct
/// art registered.
pub fn vehicle_part_sprite_key(
    kind: VehiclePartKind,
    variant_label: Option<&str>,
    view: VehicleSpriteView,
) -> String {
    format!(
        "vehicle_{}_{}_{}",
        kind_token(kind),
        variant_label.unwrap_or("base"),
        view.token()
    )
}

/// Sprite key for a multi-cell weapon module composite.
pub fn vehicle_module_sprite_key(module_label: &str, view: VehicleSpriteView) -> String {
    format!("vehicle_module_{}_{}", module_label, view.token())
}

/// Anchor cell of a module footprint — the cell with smallest `(z, y, x)`.
/// The renderer emits one composite sprite at this cell and skips the
/// other module cells.
pub fn module_anchor_cell(cells: &[IVec3]) -> Option<IVec3> {
    cells.iter().copied().min_by(|a, b| (a.z, a.y, a.x).cmp(&(b.z, b.y, b.x)))
}

/// XY extent (`width`, `depth`) in cells of a module footprint.
pub fn module_footprint_extent(cells: &[IVec3]) -> (u32, u32) {
    if cells.is_empty() {
        return (1, 1);
    }
    let xs = cells.iter().map(|c| c.x);
    let ys = cells.iter().map(|c| c.y);
    let (min_x, max_x) = (xs.clone().min().unwrap(), xs.max().unwrap());
    let (min_y, max_y) = (ys.clone().min().unwrap(), ys.max().unwrap());
    ((max_x - min_x + 1) as u32, (max_y - min_y + 1) as u32)
}

// ── 16×16 per-cell sprite templates ──────────────────────────────────────
// `WARM_PALETTE` chars:
// X near-black outline. d/D/b/B/t/T browns. s/S earth. k/K/l/P slates.
// o gold/flame. r/R red. y bright spark. M green. n/i water.

// ── Frame ────────────────────────────────────────────────────────────────

const FRAME_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "..DDDDDDDDDDDD..",
    "..DbBBBBBBBBbD..",
    "..DBtttttttttD..",
    "..DBtbtbtbtbtD..",
    "..DBtttttttttD..",
    "..DbBBBBBBBBbD..",
    "..DDDDDDDDDDDD..",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "......DDDDD.....",
    "......DbBBD.....",
    "......DBttD.....",
    "......DBBtD.....",
    "......DBBbD.....",
    "......DbbbD.....",
    "......DDDDD.....",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_LIGHT_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "..DDDDDDDDDDDD..",
    "..DBBBBBBBBBBD..",
    "..DBttttttttBD..",
    "..D.B.B.B.B.BD..",
    "..DBttttttttBD..",
    "..DBBBBBBBBBBD..",
    "..DDDDDDDDDDDD..",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_HEAVY_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    ".DDDDDDDDDDDDDD.",
    ".DbbbbbbbbbbbbD.",
    ".DbBBBBBBBBBBbD.",
    ".DbBtKtKtKtBbD..",
    ".DbBtKtKtKtBbD..",
    ".DbBtKtKtKtBbD..",
    ".DbBBBBBBBBBBbD.",
    ".DbbbbbbbbbbbbD.",
    ".DDDDDDDDDDDDDD.",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_TRUSS_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "..DDDDDDDDDDDD..",
    "..D.D.D.D.D.D...",
    "..DD.DD.DD.DD...",
    "..DbXbXbXbXbXD..",
    "..DBXBXBXBXBXD..",
    "..DbXbXbXbXbXD..",
    "..DD.DD.DD.DD...",
    "..D.D.D.D.D.D...",
    "..DDDDDDDDDDDD..",
    "................",
    "................",
    "................",
    "................",
];

// ── Deck ─────────────────────────────────────────────────────────────────

const DECK_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "..bbbbbbbbbbbb..",
    "..tTtTtTtTtTtT..",
    "..tBtBtBtBtBtB..",
    "..bDbDbDbDbDbD..",
    "..tBtBtBtBtBtB..",
    "..bbbbbbbbbbbb..",
    "..DDDDDDDDDDDD..",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const DECK_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "...bbbbbbbbbb...",
    "...tTtTtTtTtT...",
    "...tBtBtBtBtB...",
    "...bDbDbDbDbD...",
    "...tBtBtBtBtB...",
    "...bbbbbbbbbb...",
    "...DDDDDDDDDD...",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// ── Wall ─────────────────────────────────────────────────────────────────

const WALL_BASE_SIDE: &[&str] = &[
    "................",
    "..DDDDDDDDDDDD..",
    "..DbBtBtBtBtbD..",
    "..DbBtBtBtBtbD..",
    "..DDDDDDDDDDDD..",
    "..DBtBtBtBtBtD..",
    "..DBtBtBtBtBtD..",
    "..DDDDDDDDDDDD..",
    "..DbBtBtBtBtbD..",
    "..DbBtBtBtBtbD..",
    "..DDDDDDDDDDDD..",
    "..DBtBtBtBtBtD..",
    "..DBtBtBtBtBtD..",
    "..DDDDDDDDDDDD..",
    "................",
    "................",
];

const WALL_BASE_FRONT: &[&str] = &[
    "................",
    "....DDDDDDDD....",
    "....DbBtBtbD....",
    "....DbBtBtbD....",
    "....DDDDDDDD....",
    "....DBtBtBtD....",
    "....DBtBtBtD....",
    "....DDDDDDDD....",
    "....DbBtBtbD....",
    "....DbBtBtbD....",
    "....DDDDDDDD....",
    "....DBtBtBtD....",
    "....DBtBtBtD....",
    "....DDDDDDDD....",
    "................",
    "................",
];

// ── Axle ─────────────────────────────────────────────────────────────────

const AXLE_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "..XXXXXXXXXXXX..",
    "..XdDDDDDDDDdX..",
    "..XDDDDDDDDDDX..",
    "..XdDDDDDDDDdX..",
    "..XXXXXXXXXXXX..",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const AXLE_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "XXXXXXXXXXXXXXXX",
    "XdDDDDDDDDDDDDdX",
    "XDDDDDDDDDDDDDDX",
    "XdDDDDDDDDDDDDdX",
    "XXXXXXXXXXXXXXXX",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const AXLE_STEERING_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "..XXXXXX........",
    "..XdDDDdX.......",
    "..XDDDDDX.......",
    "...XdDDDXXXXXXX.",
    "....XXXXXdDDDdX.",
    "..........XDDDX.",
    "..........XXXXX.",
    "................",
    "................",
    "................",
];

const AXLE_REINFORCED_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "..XXXXXXXXXXXX..",
    "..XKKKKKKKKKKX..",
    "..XkKKkKKkKKkX..",
    "..XKKKKKKKKKKX..",
    "..XkKKkKKkKKkX..",
    "..XKKKKKKKKKKX..",
    "..XXXXXXXXXXXX..",
    "................",
    "................",
    "................",
    "................",
];

// ── Wheel ────────────────────────────────────────────────────────────────

const WHEEL_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    ".....XXXXX......",
    "....XdDDDdX.....",
    "...XDbBBBbDX....",
    "..XDbBtttBbDX...",
    "..XDBttBttBDX...",
    "..XdBtBBBtdX....",
    "..XDBttBttBDX...",
    "..XDbBtttBbDX...",
    "...XDbBBBbDX....",
    "....XdDDDdX.....",
    ".....XXXXX......",
    "................",
    "................",
    "................",
];

const WHEEL_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "......XXXX......",
    ".....XdDDdX.....",
    ".....XDDDDX.....",
    ".....XDbbDX.....",
    ".....XDBBDX.....",
    ".....XDBBDX.....",
    ".....XDBBDX.....",
    ".....XDbbDX.....",
    ".....XDDDDX.....",
    ".....XdDDdX.....",
    "......XXXX......",
    "................",
    "................",
    "................",
];

const WHEEL_SOLID_SIDE: &[&str] = &[
    "................",
    "................",
    ".....XXXXX......",
    "....XdDDDdX.....",
    "...XDbbbbbDX....",
    "..XDbBBBBBbDX...",
    "..XDBBBBBBBDX...",
    "..XdBBBXBBBdX...",
    "..XDBBBBBBBDX...",
    "..XDbBBBBBbDX...",
    "...XDbbbbbDX....",
    "....XdDDDdX.....",
    ".....XXXXX......",
    "................",
    "................",
    "................",
];

const WHEEL_SPOKED_SIDE: &[&str] = &[
    "................",
    "................",
    ".....XXXXX......",
    "....XdDDDdX.....",
    "...XD.D.DDX.....",
    "..XDb.Bt.bDX....",
    "..XD.tBt.DX.....",
    "..XdB.BX.BdX....",
    "..XD.tBt.DX.....",
    "..XDb.Bt.bDX....",
    "...XD.D.DDX.....",
    "....XdDDDdX.....",
    ".....XXXXX......",
    "................",
    "................",
    "................",
];

const WHEEL_IRON_RIM_SIDE: &[&str] = &[
    "................",
    "................",
    ".....KKKKK......",
    "....KkPPPkK.....",
    "...KPbBBBbPK....",
    "..KPbBttBbPK....",
    "..KPBttBttPK....",
    "..KkBtBPBtkK....",
    "..KPBttBttPK....",
    "..KPbBttBbPK....",
    "...KPbBBBbPK....",
    "....KkPPPkK.....",
    ".....KKKKK......",
    "................",
    "................",
    "................",
];

// Iron rim front — slate-toned narrow rectangle.
const WHEEL_IRON_RIM_FRONT: &[&str] = &[
    "................",
    "................",
    "......KKKK......",
    ".....KkPPkK.....",
    ".....KPPPPK.....",
    ".....KPbbPK.....",
    ".....KPBBPK.....",
    ".....KPBBPK.....",
    ".....KPBBPK.....",
    ".....KPbbPK.....",
    ".....KPPPPK.....",
    ".....KkPPkK.....",
    "......KKKK......",
    "................",
    "................",
    "................",
];

// ── Hitch / Yoke ─────────────────────────────────────────────────────────

const HITCH_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "......DDDD......",
    "......DbBD......",
    "......DbBD......",
    "..DDDDDbBD......",
    "..DBBBBBBD......",
    "..DBttttBD......",
    "..DbBBBBbD......",
    "..DDDDDDDD......",
    "................",
    "................",
    "................",
];

const HITCH_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "......XXXX......",
    "......XbbX......",
    "......XbBX......",
    "....DDDXbBDDD...",
    "....DbBbbBbBD...",
    "....DBtBBBtBD...",
    "....DDDDDDDDD...",
    "................",
    "................",
    "................",
];

const YOKE_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "....DD......DD..",
    "....DbD....DbD..",
    "....DbBDDDDbBD..",
    "....DBtBBBBBtD..",
    "....DbBttttBtD..",
    "....DDDbBBbDDD..",
    "......DDDDDD....",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const YOKE_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "....DD....DD....",
    "....DbD..DbD....",
    "....DbBDDbBD....",
    "....DBtBBtBD....",
    "....DbBttBbD....",
    "....DDDbbDDD....",
    "......DDDD......",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// ── CargoBay ─────────────────────────────────────────────────────────────

const CARGOBAY_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "...DDDDDDDDDD...",
    "...DbbbbbbbbD...",
    "...DbBBBBBBbD...",
    "...DbBTTTTBbD...",
    "...DbBToTTBbD...",
    "...DbBTTTTBbD...",
    "...DbBTTTTBbD...",
    "...DbBBBBBBbD...",
    "...DbbbbbbbbD...",
    "...DDDDDDDDDD...",
    "................",
    "................",
    "................",
    "................",
];

const CARGOBAY_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "...DDDDDDDDDD...",
    "...DbbbbbbbbD...",
    "...DbBBBBBBbD...",
    "...DbBTTTTBbD...",
    "...DbBToTTBbD...",
    "...DbBTTTTBbD...",
    "...DbBTTTTBbD...",
    "...DbBBBBBBbD...",
    "...DbbbbbbbbD...",
    "...DDDDDDDDDD...",
    "................",
    "................",
    "................",
    "................",
];

// ── CrewSeat ─────────────────────────────────────────────────────────────

const CREWSEAT_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "......DDDD......",
    "....DDDrrDD.....",
    "....DRRRrrD.....",
    "....DRRrrrD.....",
    "....DRRrrrD.....",
    "...DDRRrrrDD....",
    "...DRRRrrrrD....",
    "...DRRrrrrrD....",
    "...DDDDDDDDDD...",
    "...D........D...",
    "...d........d...",
    "................",
    "................",
];

const CREWSEAT_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "......XXXX......",
    "....XXDrrDXX....",
    "....XDRrrrDX....",
    "....DDRRrrDD....",
    "....DRRrrrrD....",
    "....DRRrrrrD....",
    "...DDRRrrrrDD...",
    "...DRRrrrrrrD...",
    "...DDDDDDDDDD...",
    "...D........D...",
    "...d........d...",
    "................",
    "................",
];

// ── WeaponMount ──────────────────────────────────────────────────────────

const WEAPONMOUNT_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    ".......X........",
    ".......kK.......",
    "......kKK.......",
    "......kKK.......",
    "......kKK.......",
    "....DDDKKDDD....",
    "....DRKKKKRD....",
    "....DRrrrrRD....",
    "....DRrXXrRD....",
    "....DDDDDDDD....",
    "................",
    "................",
    "................",
];

const WEAPONMOUNT_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    ".......X........",
    "......kKk.......",
    "......kKK.......",
    "......kKK.......",
    "....DDKKKKDD....",
    "....DRKKKKRD....",
    "....DRrrrrRD....",
    "....DRrXXrRD....",
    "....DDDDDDDD....",
    "................",
    "................",
    "................",
];

// ── Engine ───────────────────────────────────────────────────────────────

const ENGINE_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    ".......k........",
    "......KKK.......",
    "...XXKKKKKXX....",
    "...XlPllPlPlX...",
    "...XKKKKKKKKX...",
    "...XKlPlPllKX...",
    "...XKoooooKKX...",
    "...XKRRRRRoKX...",
    "...XKKKKKKKKX...",
    "...XlPllPllPX...",
    "...XXXXXXXXXX...",
    "................",
    "................",
    "................",
];

const ENGINE_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    ".......k........",
    "......KKK.......",
    "...XXKKKKKXX....",
    "...XllPlPllPX...",
    "...XKKKKKKKKX...",
    "...XKoooooKKX...",
    "...XKoXXXoKKX...",
    "...XKoXXXoKKX...",
    "...XKoooooKKX...",
    "...XKKKKKKKKX...",
    "...XXXXXXXXXX...",
    "................",
    "................",
    "................",
];

// ── Track ────────────────────────────────────────────────────────────────

const TRACK_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "...XXXXXXXXXX...",
    "..XdDDDDDDDDdX..",
    "..XDKKKKKKKKDX..",
    "..XdKkKkKkKkdX..",
    "..XDKKKKKKKKDX..",
    "..XdKkKkKkKkdX..",
    "..XDKKKKKKKKDX..",
    "..XdDDDDDDDDdX..",
    "...XXXXXXXXXX...",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const TRACK_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "....XXXXXXXX....",
    "....XKKKKKKX....",
    "....XKkKkKkX....",
    "....XKKKKKKX....",
    "....XKkKkKkX....",
    "....XKKKKKKX....",
    "....XKkKkKkX....",
    "....XKKKKKKX....",
    "....XXXXXXXX....",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const TRACK_METAL_SIDE: &[&str] = &[
    "................",
    "................",
    "...XXXXXXXXXX...",
    "..XkKKKKKKKKkX..",
    "..XKPlPlPlPlKX..",
    "..XKPXPXPXPXKX..",
    "..XKlPlPlPlPKX..",
    "..XKPXPXPXPXKX..",
    "..XKPlPlPlPlKX..",
    "..XkKKKKKKKKkX..",
    "...XXXXXXXXXX...",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// ── ArmorPlate ───────────────────────────────────────────────────────────

const ARMORPLATE_BASE_SIDE: &[&str] = &[
    "................",
    "..KKKKKKKKKKKK..",
    "..KPlPlPlPlPlK..",
    "..KlKKKKKKKKlK..",
    "..KPKsKsKsKsKK..",
    "..KlKKKKKKKKlK..",
    "..KPKsKsKsKsKK..",
    "..KlKKKKKKKKlK..",
    "..KPKsKsKsKsKK..",
    "..KlKKKKKKKKlK..",
    "..KPlPlPlPlPlK..",
    "..KKKKKKKKKKKK..",
    "................",
    "................",
    "................",
    "................",
];

const ARMORPLATE_BASE_FRONT: &[&str] = &[
    "................",
    "....KKKKKKKK....",
    "....KPlPlPlK....",
    "....KlKKKKlK....",
    "....KPKsKsKK....",
    "....KlKKKKlK....",
    "....KPKsKsKK....",
    "....KlKKKKlK....",
    "....KPKsKsKK....",
    "....KlKKKKlK....",
    "....KPlPlPlK....",
    "....KKKKKKKK....",
    "................",
    "................",
    "................",
    "................",
];

// ── Turret ───────────────────────────────────────────────────────────────

const TURRET_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "...XXKKKKK......",
    "..XkKKKKKKkX....",
    "..XKlPllPlKKXXXX",
    "..XKllPllPKKKKKK",
    "..XKlPllPlKKXXXX",
    "..XkKKKKKKkX....",
    "...XXKKKKK......",
    "....XXXXX.......",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const TURRET_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    ".....XXXXX......",
    "....XkKKKkX.....",
    "....XKllPKX.....",
    "....XKlllKX.....",
    "....XKPllKX.....",
    "....XkKkKkX.....",
    "....XKKKKKX.....",
    "......XKX.......",
    "......XKX.......",
    "......XkX.......",
    ".......X........",
    "................",
    "................",
    "................",
];

// ── Multi-cell weapon modules ────────────────────────────────────────────
// Sized to the module footprint: 16 px per cell. Anchor is the smallest
// (z, y, x) corner; the sprite is drawn with its top-left at that corner,
// occupying `width × depth` cells.

// ram_head_1x2 — 1 wide × 2 deep → 16×32 px
const RAM_HEAD_1X2_SIDE: &[&str] = &[
    "................",
    "................",
    "......XXX.......",
    ".....XKKKX......",
    "....XKPlPKX.....",
    "....XKKKKKX.....",
    ".....XKKKX......",
    "......XKX.......",
    "......XKX.......",
    "....DDDKDDD.....",
    "....DbBKBbD.....",
    "....DBtKtBD.....",
    "....DbBKBbD.....",
    "....DDDKDDD.....",
    "......XKX.......",
    "......XXX.......",
    "................",
    "......XKX.......",
    "......XKX.......",
    "......XKX.......",
    "......XKX.......",
    "......XKX.......",
    "......XKX.......",
    ".....XKKKX......",
    ".....XKPKX......",
    "....XKPPPKX.....",
    "...XKPPPPPKX....",
    "...XKKKKKKKX....",
    "....XKKKKKX.....",
    ".....XKKKX......",
    "......XKX.......",
    "................",
];

const RAM_HEAD_1X2_FRONT: &[&str] = &[
    "................",
    "................",
    "......XKKX......",
    ".....XKPPKX.....",
    "....XKPPPPKX....",
    "....XKKKKKKX....",
    ".....XKKKKX.....",
    "......XKKX......",
    "......XKKX......",
    "....DDDKKDDD....",
    "....DbBKKBbD....",
    "....DBtKKtBD....",
    "....DbBKKBbD....",
    "....DDDKKDDD....",
    "......XKKX......",
    "......XKKX......",
    "................",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    ".....XKKKKX.....",
    ".....XKPPKX.....",
    "....XKPPPPKX....",
    "....XKKKKKKX....",
    ".....XKKKKX.....",
    "......XKKX......",
    "......XKKX......",
    "................",
];

// battering_ram_2x3 — 2 wide × 3 deep → 32×48 px
// Two parallel rails carrying a great ram log on chains.
const BATTERING_RAM_2X3_SIDE: &[&str] = &[
    "................................",
    "................................",
    ".DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    ".DbBBBBBBBBBBBBBBBBBBBBBBBBBBbD.",
    ".DbBttttttttttttttttttttttttBbD.",
    ".DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "......XKX................XKX....",
    "......XKX................XKX....",
    "......XKX................XKX....",
    "......XKX................XKX....",
    "..XXXKKKXXXXXXXXXXXXXXXXKKKXXX..",
    ".XKKKKKKKKKKKKKKKKKKKKKKKKKKKKX.",
    ".XKlPlPlPlPlPlPlPlPlPlPlPlPlPKX.",
    ".XKPlPlPlPlPlPlPlPlPlPlPlPlPlKX.",
    ".XKlPlPlPlPlPlPlPlPlPlPlPlPlPKX.",
    ".XKKKKKKKKKKKKKKKKKKKKKKKKKKKKX.",
    "..XXXXXXXXXXXXXXXXXXXXXXXXXXXX..",
    "................................",
    ".DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    ".DbBBBBBBBBBBBBBBBBBBBBBBBBBBbD.",
    ".DbBttttttttttttttttttttttttBbD.",
    ".DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "..D............................D",
    "..d............................d",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
];

const BATTERING_RAM_2X3_FRONT: &[&str] = &[
    "................................",
    "................................",
    ".DDDDDDDDDDDD....DDDDDDDDDDDD...",
    ".DbBBBBBBBBbD....DbBBBBBBBBbD...",
    ".DbBttttttBbD....DbBttttttBbD...",
    ".DDDDDDDDDDDD....DDDDDDDDDDDD...",
    "......XKX............XKX........",
    "......XKX............XKX........",
    "......XKX............XKX........",
    ".....XKKKXXXXXXXXXXXXKKKXX......",
    "....XKKKKKKKKKKKKKKKKKKKKKX.....",
    "....XKPlPlPlPlPlPlPlPlPlPKX.....",
    "....XKlPlPlPlPlPlPlPlPlPlKX.....",
    "....XKPlPlPlPlPlPlPlPlPlPKX.....",
    "....XKKKKKKKKKKKKKKKKKKKKKX.....",
    ".....XXXXXXXXXXXXXXXXXXXXX......",
    "................................",
    ".DDDDDDDDDDDD....DDDDDDDDDDDD...",
    ".DbBBBBBBBBbD....DbBBBBBBBBbD...",
    ".DbBttttttBbD....DbBttttttBbD...",
    ".DDDDDDDDDDDD....DDDDDDDDDDDD...",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
    "................................",
];

// ballista_2x2 — 32×32
const BALLISTA_2X2_SIDE: &[&str] = &[
    "................................",
    ".................XXXX...........",
    "................XKKKKX..........",
    "...............XKKllKKX.........",
    "..............XKKlllPKKX........",
    ".............XKKllPlllKKX.......",
    "............XKKKKKKKKKKKKX......",
    "...........XKKlPlPlPlPlPKKX.....",
    "..........XKKlPlPlPlPlPlPKKX....",
    ".........XKKKKKKKKKKKKKKKKKKX...",
    ".........X......XKX.............",
    ".........X.....XKKKX............",
    ".........X....XKKKKKX...........",
    ".........X...XKKKPKKKX..........",
    ".........X..XKKKPPPKKKX.........",
    ".........X.XKKKPPPPPKKKX........",
    ".........XKKKKKKKKKKKKKKX.......",
    "....DDDDDDDDDDDDDDDDDDDDDDDDD...",
    "...DbBBBBBBBBBBBBBBBBBBBBBBBbD..",
    "...DbBttttttttttttttttttttttBbD.",
    "...DDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    ".....XKX......XKX.....XKX.......",
    ".....XKX......XKX.....XKX.......",
    "....XKKKX....XKKKX...XKKKX......",
    "...XKKlKKX..XKKlKKX.XKKlKKX.....",
    "..XKKKKKKKXXKKKKKKKXKKKKKKKX....",
    "..XKKKKKKKKKKKKKKKKKKKKKKKKX....",
    "...XKKKKKX..XKKKKKX.XKKKKKX.....",
    "....XKKKX....XKKKX...XKKKX......",
    ".....XKX......XKX.....XKX.......",
    "................................",
    "................................",
];

const BALLISTA_2X2_FRONT: &[&str] = &[
    "................................",
    "..............XXXX..............",
    ".............XKKKKX.............",
    "............XKKKKKKX............",
    "...........XKKllllKKX...........",
    "..........XKKllPlllKKX..........",
    ".........XKKllPPPllKKX..........",
    "........XKKKKKKKKKKKKKX.........",
    ".......XKKllPlPlPlPlllKX........",
    "......XKKllPlPlPlPlPllKKX.......",
    ".....XKKKKKKKKKKKKKKKKKKKX......",
    ".....XKKKKKKKPPPPPPKKKKKKX......",
    "......XX......XKX.......XX......",
    ".............XKKKX..............",
    "............XKKKKKX.............",
    "...........XKKPPPKKX............",
    "..........XKPPPPPPPKX...........",
    "..DDDDDDDDDDDDDDDDDDDDDDDDDDDD..",
    "..DbBBBBBBBBBBBBBBBBBBBBBBBBbD..",
    "..DbBttttttttttttttttttttttBbD..",
    "..DDDDDDDDDDDDDDDDDDDDDDDDDDDD..",
    "......XKX................XKX....",
    "......XKX................XKX....",
    ".....XKKKX..............XKKKX...",
    "....XKKlKKX............XKKlKKX..",
    "...XKKKKKKKX..........XKKKKKKKX.",
    "...XKKKKKKKX..........XKKKKKKKX.",
    "....XKKKKKX............XKKKKKX..",
    ".....XKKKX..............XKKKX...",
    "......XKX................XKX....",
    "................................",
    "................................",
];

// light_turret_2x2 — 32×32
const LIGHT_TURRET_2X2_SIDE: &[&str] = &[
    "................................",
    "................................",
    "........XXXKKKKKKKXX............",
    ".......XKKKKKKKKKKKKX...........",
    "......XKKlPllPlPllPKKX..........",
    ".....XKKllPllPlPllPlKKX.........",
    ".....XKKlPllPlPllPlPKKXXXXX.....",
    "....XkKKKKKKKKKKKKKKKKKKKKKX....",
    "....XKllPlPlPlPlPlPlPlKKKKKKX...",
    "....XKlPlPlPlPlPlPlPlPKKKKKKX...",
    "....XKllPlPlPlPlPlPlPlKKKKKKX...",
    "....XkKKKKKKKKKKKKKKKKKKKKKX....",
    ".....XKKlPllPlPllPlPKKXXXXX.....",
    ".....XKKllPllPlPllPlKKX.........",
    "......XKKlPllPlPllPKKX..........",
    ".......XKKKKKKKKKKKKX...........",
    "........XXXKKKKKKKXX............",
    "................................",
    "...DDDDDDDDDDDDDDDDDDDDDDDDD....",
    "...DbBBBBBBBBBBBBBBBBBBBBBBBbD..",
    "...DbBttttttttttttttttttttttBbD.",
    "...DDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "......XKX............XKX........",
    "......XKX............XKX........",
    "....XKKKKKX........XKKKKKX......",
    "...XKKllPKKX......XKKllPKKX.....",
    "...XKKKKKKKKX....XKKKKKKKKX.....",
    "...XKKKKKKKKX....XKKKKKKKKX.....",
    "...XKKllPKKX......XKKllPKKX.....",
    "....XKKKKKX........XKKKKKX......",
    "......XKX............XKX........",
    "................................",
];

const LIGHT_TURRET_2X2_FRONT: &[&str] = &[
    "................................",
    "................................",
    "...........XXXXXX...............",
    "..........XKKKKKKX..............",
    ".........XKKllPllKX.............",
    "........XKKllPPlllKX............",
    ".......XKKlPllPlPlPKX...........",
    "......XKKKKKKKKKKKKKKX..........",
    ".....XKKlPlPlPlPlPlPKKX.........",
    "....XKKlPlPlPlPlPlPlPKKX........",
    "....XKKKKKKKKKKKKKKKKKKX........",
    "....XKKKKKKKPPPPPPPKKKKX........",
    ".....XX.......XKX........XX.....",
    "..............XKX...............",
    "..............XKX...............",
    "..............XKX...............",
    "..............XKX...............",
    "..............XKX...............",
    "...DDDDDDDDDDDDDDDDDDDDDDDDD....",
    "...DbBBBBBBBBBBBBBBBBBBBBBBBbD..",
    "...DbBttttttttttttttttttttttBbD.",
    "...DDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "......XKX............XKX........",
    "......XKX............XKX........",
    "....XKKKKKX........XKKKKKX......",
    "...XKKllPKKX......XKKllPKKX.....",
    "...XKKKKKKKKX....XKKKKKKKKX.....",
    "...XKKKKKKKKX....XKKKKKKKKX.....",
    "...XKKllPKKX......XKKllPKKX.....",
    "....XKKKKKX........XKKKKKX......",
    "......XKX............XKX........",
    "................................",
];

// heavy_turret_3x3 — 48×48
// Larger turret on a wider chassis.
const HEAVY_TURRET_3X3_SIDE: &[&str] = &[
    "................................................",
    "................................................",
    "................XXXKKKKKKKKKKKKKXX..............",
    "..............XKKKKKKKKKKKKKKKKKKKKX............",
    ".............XKKlPlPlPlPlPlPlPlPlPKKX...........",
    "............XKKlPlPlPlPlPlPlPlPlPlPKKX..........",
    "...........XKKlPlPlPlPlPlPlPlPlPlPlPKKXXXXXXX...",
    "..........XkKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKX..",
    "..........XKKlPlPlPlPlPlPlPlPlPlPlPlPKKKKKKKKKX.",
    "..........XKKlPlPlPlPlPlPlPlPlPlPlPlPKKKKKKKKKX.",
    "..........XKKlPlPlPlPlPlPlPlPlPlPlPlPKKKKKKKKKX.",
    "..........XKKlPlPlPlPlPlPlPlPlPlPlPlPKKKKKKKKKX.",
    "..........XKKlPlPlPlPlPlPlPlPlPlPlPlPKKKKKKKKKX.",
    "..........XkKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKX..",
    "...........XKKlPlPlPlPlPlPlPlPlPlPlPKKXXXXXXX...",
    "............XKKlPlPlPlPlPlPlPlPlPlPKKX..........",
    ".............XKKlPlPlPlPlPlPlPlPlPKKX...........",
    "..............XKKKKKKKKKKKKKKKKKKKKX............",
    "................XXXKKKKKKKKKKKKKXX..............",
    "................................................",
    "..DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "..DbBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBbD.",
    "..DbBtttttttttttttttttttttttttttttttttttttttBbD.",
    "..DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "......XKX..........XKX..........XKX.............",
    "......XKX..........XKX..........XKX.............",
    "....XKKKKKX......XKKKKKX......XKKKKKX...........",
    "...XKKlPlKKX....XKKlPlKKX....XKKlPlKKX..........",
    "..XKKKKKKKKKX..XKKKKKKKKKX..XKKKKKKKKKX.........",
    "..XKKKKKKKKKX..XKKKKKKKKKX..XKKKKKKKKKX.........",
    "...XKKlPlKKX....XKKlPlKKX....XKKlPlKKX..........",
    "....XKKKKKX......XKKKKKX......XKKKKKX...........",
    "......XKX..........XKX..........XKX.............",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
];

const HEAVY_TURRET_3X3_FRONT: &[&str] = &[
    "................................................",
    "................................................",
    "...............XXXXXXXXXX.......................",
    "..............XKKKKKKKKKKX......................",
    ".............XKKllPlPlllKKX.....................",
    "............XKKlPlPlPlPlPKKX....................",
    "...........XKKlPlPlPlPlPlPKKX...................",
    "..........XKKKKKKKKKKKKKKKKKKX..................",
    ".........XKKKlPlPlPlPlPlPlPKKKX.................",
    "........XKKKKlPlPlPlPlPlPlPlKKKX................",
    ".......XKKKKKlPlPlPlPlPlPlPlPKKKKX..............",
    "......XKKKKKKKKKKKKKKKKKKKKKKKKKKKX.............",
    "......XKKKKKKKKKPPPPPPPPPKKKKKKKKKX.............",
    ".......XX..........XKX..............XX..........",
    "....................XKX.........................",
    "....................XKX.........................",
    "....................XKX.........................",
    "....................XKX.........................",
    "....................XKX.........................",
    "................................................",
    "..DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "..DbBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBbD.",
    "..DbBtttttttttttttttttttttttttttttttttttttttBbD.",
    "..DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD.",
    "......XKX..........XKX..........XKX.............",
    "......XKX..........XKX..........XKX.............",
    "....XKKKKKX......XKKKKKX......XKKKKKX...........",
    "...XKKlPlKKX....XKKlPlKKX....XKKlPlKKX..........",
    "..XKKKKKKKKKX..XKKKKKKKKKX..XKKKKKKKKKX.........",
    "..XKKKKKKKKKX..XKKKKKKKKKX..XKKKKKKKKKX.........",
    "...XKKlPlKKX....XKKlPlKKX....XKKlPlKKX..........",
    "....XKKKKKX......XKKKKKX......XKKKKKX...........",
    "......XKX..........XKX..........XKX.............",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
    "................................................",
];

// ── Registration ─────────────────────────────────────────────────────────

pub fn register_vehicle_part_sprites(lib: &mut SpriteLibrary, images: &mut Assets<Image>) {
    macro_rules! insert {
        ($name:expr, $art:expr) => {{
            let img = ascii_to_image($art, WARM_PALETTE);
            lib.sprites.insert($name.to_string(), images.add(img));
        }};
    }

    // ── Per-kind base sprites ────────────────────────────────────────
    insert!("vehicle_frame_base_side", FRAME_BASE_SIDE);
    insert!("vehicle_frame_base_front", FRAME_BASE_FRONT);
    insert!("vehicle_deck_base_side", DECK_BASE_SIDE);
    insert!("vehicle_deck_base_front", DECK_BASE_FRONT);
    insert!("vehicle_wall_base_side", WALL_BASE_SIDE);
    insert!("vehicle_wall_base_front", WALL_BASE_FRONT);
    insert!("vehicle_axle_base_side", AXLE_BASE_SIDE);
    insert!("vehicle_axle_base_front", AXLE_BASE_FRONT);
    insert!("vehicle_wheel_base_side", WHEEL_BASE_SIDE);
    insert!("vehicle_wheel_base_front", WHEEL_BASE_FRONT);
    insert!("vehicle_hitch_base_side", HITCH_BASE_SIDE);
    insert!("vehicle_hitch_base_front", HITCH_BASE_FRONT);
    insert!("vehicle_yoke_base_side", YOKE_BASE_SIDE);
    insert!("vehicle_yoke_base_front", YOKE_BASE_FRONT);
    insert!("vehicle_cargo_bay_base_side", CARGOBAY_BASE_SIDE);
    insert!("vehicle_cargo_bay_base_front", CARGOBAY_BASE_FRONT);
    insert!("vehicle_crew_seat_base_side", CREWSEAT_BASE_SIDE);
    insert!("vehicle_crew_seat_base_front", CREWSEAT_BASE_FRONT);
    insert!("vehicle_weapon_mount_base_side", WEAPONMOUNT_BASE_SIDE);
    insert!("vehicle_weapon_mount_base_front", WEAPONMOUNT_BASE_FRONT);
    insert!("vehicle_engine_base_side", ENGINE_BASE_SIDE);
    insert!("vehicle_engine_base_front", ENGINE_BASE_FRONT);
    insert!("vehicle_track_base_side", TRACK_BASE_SIDE);
    insert!("vehicle_track_base_front", TRACK_BASE_FRONT);
    insert!("vehicle_armor_plate_base_side", ARMORPLATE_BASE_SIDE);
    insert!("vehicle_armor_plate_base_front", ARMORPLATE_BASE_FRONT);
    insert!("vehicle_turret_base_side", TURRET_BASE_SIDE);
    insert!("vehicle_turret_base_front", TURRET_BASE_FRONT);

    // ── Visually distinct variants ───────────────────────────────────
    // Frame variants reuse the base (mass/support only; no readable
    // silhouette difference at 16 px). Provide truss + heavy + light as
    // distinct keys so the resolver hits them.
    insert!("vehicle_frame_light_chassis_side", FRAME_LIGHT_SIDE);
    insert!("vehicle_frame_light_chassis_front", FRAME_BASE_FRONT);
    insert!("vehicle_frame_heavy_chassis_side", FRAME_HEAVY_SIDE);
    insert!("vehicle_frame_heavy_chassis_front", FRAME_BASE_FRONT);
    insert!("vehicle_frame_truss_chassis_side", FRAME_TRUSS_SIDE);
    insert!("vehicle_frame_truss_chassis_front", FRAME_BASE_FRONT);

    // Wheel variants.
    insert!("vehicle_wheel_solid_wheel_side", WHEEL_SOLID_SIDE);
    insert!("vehicle_wheel_solid_wheel_front", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_spoked_wheel_side", WHEEL_SPOKED_SIDE);
    insert!("vehicle_wheel_spoked_wheel_front", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_iron_rim_wheel_side", WHEEL_IRON_RIM_SIDE);
    insert!("vehicle_wheel_iron_rim_wheel_front", WHEEL_IRON_RIM_FRONT);

    // Axle variants — fixed reuses base; steering + reinforced have
    // distinct silhouettes.
    insert!("vehicle_axle_fixed_axle_side", AXLE_BASE_SIDE);
    insert!("vehicle_axle_fixed_axle_front", AXLE_BASE_FRONT);
    insert!("vehicle_axle_steering_axle_side", AXLE_STEERING_SIDE);
    insert!("vehicle_axle_steering_axle_front", AXLE_BASE_FRONT);
    insert!("vehicle_axle_reinforced_axle_side", AXLE_REINFORCED_SIDE);
    insert!("vehicle_axle_reinforced_axle_front", AXLE_BASE_FRONT);

    // Track variants — wooden_track reuses base; metal_track distinct.
    insert!("vehicle_track_wooden_track_side", TRACK_BASE_SIDE);
    insert!("vehicle_track_wooden_track_front", TRACK_BASE_FRONT);
    insert!("vehicle_track_metal_track_side", TRACK_METAL_SIDE);
    insert!("vehicle_track_metal_track_front", TRACK_BASE_FRONT);

    // ── Weapon module composites ─────────────────────────────────────
    insert!("vehicle_module_ram_head_1x2_side", RAM_HEAD_1X2_SIDE);
    insert!("vehicle_module_ram_head_1x2_front", RAM_HEAD_1X2_FRONT);
    insert!(
        "vehicle_module_battering_ram_2x3_side",
        BATTERING_RAM_2X3_SIDE
    );
    insert!(
        "vehicle_module_battering_ram_2x3_front",
        BATTERING_RAM_2X3_FRONT
    );
    insert!("vehicle_module_ballista_2x2_side", BALLISTA_2X2_SIDE);
    insert!("vehicle_module_ballista_2x2_front", BALLISTA_2X2_FRONT);
    insert!(
        "vehicle_module_light_turret_2x2_side",
        LIGHT_TURRET_2X2_SIDE
    );
    insert!(
        "vehicle_module_light_turret_2x2_front",
        LIGHT_TURRET_2X2_FRONT
    );
    insert!(
        "vehicle_module_heavy_turret_3x3_side",
        HEAVY_TURRET_3X3_SIDE
    );
    insert!(
        "vehicle_module_heavy_turret_3x3_front",
        HEAVY_TURRET_3X3_FRONT
    );
}
