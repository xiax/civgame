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
//! front sprite drawn facing south, back sprite drawn facing north.

use crate::rendering::pixel_art::{ascii_to_image, WARM_PALETTE};
use crate::rendering::sprite_library::SpriteLibrary;
use crate::simulation::vehicle::VehiclePartKind;
use bevy::math::IVec3;
use bevy::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VehicleSpriteView {
    Side,
    Front,
    Back,
}

impl VehicleSpriteView {
    pub fn token(self) -> &'static str {
        match self {
            VehicleSpriteView::Side => "side",
            VehicleSpriteView::Front => "front",
            VehicleSpriteView::Back => "back",
        }
    }

    pub const ALL: [VehicleSpriteView; 3] = [
        VehicleSpriteView::Side,
        VehicleSpriteView::Front,
        VehicleSpriteView::Back,
    ];
}

/// Map a 0..4 heading to `(view, flip_x)`. Heading convention from
/// `VehicleFootprint::offsets_by_heading`: 0=N, 1=W, 2=S, 3=E. N renders
/// the back-of-vehicle silhouette (viewer south of vehicle's tail looking
/// north past the rear); S renders the front silhouette; E uses the
/// side sprite (drawn east-facing); W flips it horizontally.
pub fn view_for_heading(heading: u8) -> (VehicleSpriteView, bool) {
    match heading % 4 {
        0 => (VehicleSpriteView::Back, false),
        1 => (VehicleSpriteView::Side, true),
        2 => (VehicleSpriteView::Front, false),
        _ => (VehicleSpriteView::Side, false),
    }
}

/// Connector overlay direction in **camera-space** — encodes which screen
/// edge of the source cell the bridging hardware paints toward. Grid-space
/// neighbours rotate through the active heading into one of these four
/// before sprite-key resolution; chassis-forward likewise rotates into the
/// matching screen direction for the seat-facing indicator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectorDir {
    Up,
    Down,
    Left,
    Right,
}

impl ConnectorDir {
    pub fn token(self) -> &'static str {
        match self {
            ConnectorDir::Up => "up",
            ConnectorDir::Down => "down",
            ConnectorDir::Left => "left",
            ConnectorDir::Right => "right",
        }
    }
}

/// Sprite key for a connector overlay. `label` identifies the seam family
/// (e.g. `"axle_wheel"`, `"frame_seam"`, `"hitch_attach"`,
/// `"crew_seat_facing"`).
pub fn vehicle_connector_sprite_key(
    label: &str,
    view: VehicleSpriteView,
    dir: ConnectorDir,
) -> String {
    format!(
        "vehicle_connector_{}_{}_{}",
        label,
        view.token(),
        dir.token()
    )
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

// A frame side-view is a long horizontal beam: edges flush so adjacent
// frame cells form one continuous run. Vertical centre band leaves room
// for a deck or cargo bay on top, an axle below.
const FRAME_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DBBBBBBBBBBBBBBD",
    "DBttttttttttttBD",
    "DBtbtbtbtbtbtbtD",
    "DBttttttttttttBD",
    "DBBBBBBBBBBBBBBD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
];

// Frame front-view = beam cross-section. The cross fills the full cell
// width so the frame looks continuous when two frames sit side-by-side
// across the chassis width.
const FRAME_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DBBBBBBBBBBBBBBD",
    "DBttttttttttttBD",
    "DBtbtbtbtbtbtbtD",
    "DBttttttttttttBD",
    "DBBBBBBBBBBBBBBD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_LIGHT_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbBBBBBBBBBBBBbD",
    "DBtttttttttttttD",
    "Dt.t.t.t.t.t.t.D",
    "DtTtTtTtTtTtTtTD",
    "Dt.t.t.t.t.t.t.D",
    "DBtttttttttttttD",
    "DbBBBBBBBBBBBBbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_HEAVY_SIDE: &[&str] = &[
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DKKKKKKKKKKKKKKD",
    "DKbbbbbbbbbbbbKD",
    "DKbBBBBBBBBBBbKD",
    "DKbBKtKtKtKtBbKD",
    "DKbBKtKtKtKtBbKD",
    "DKbBBBBBBBBBBbKD",
    "DKbbbbbbbbbbbbKD",
    "DKKKKKKKKKKKKKKD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
];

const FRAME_TRUSS_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbBBBBBBBBBBBBbD",
    "DBXX.XX.XX.XX.BD",
    "DBX.X.X.X.X.X.BD",
    "DB.X.X.X.X.X.XBD",
    "DBX.X.X.X.X.X.BD",
    "DBXX.XX.XX.XX.BD",
    "DbBBBBBBBBBBBBbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
];

// ── Deck ─────────────────────────────────────────────────────────────────

// Deck: solid planked surface, edges flush so two adjacent decks form
// one continuous floor without visible seams.
const DECK_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DTtTtTtTtTtTtTtD",
    "DtTtTtTtTtTtTtTD",
    "DTtTtTtTtTtTtTtD",
    "DbBbBbBbBbBbBbBD",
    "DTtTtTtTtTtTtTtD",
    "DtTtTtTtTtTtTtTD",
    "DTtTtTtTtTtTtTtD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

const DECK_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DTTTTTTTTTTTTTTD",
    "DttttttttttttttD",
    "DTTTTTTTTTTTTTTD",
    "DttttttttttttttD",
    "DTTTTTTTTTTTTTTD",
    "DttttttttttttttD",
    "DTTTTTTTTTTTTTTD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

// ── Wall ─────────────────────────────────────────────────────────────────

// Wall: stacked plank courses, edges flush.
const WALL_BASE_SIDE: &[&str] = &[
    "DDDDDDDDDDDDDDDD",
    "DbBtBtBtBtBtBtbD",
    "DbBtBtBtBtBtBtbD",
    "DDDDDDDDDDDDDDDD",
    "DBtBtBtBtBtBtBtD",
    "DBtBtBtBtBtBtBtD",
    "DDDDDDDDDDDDDDDD",
    "DbBtBtBtBtBtBtbD",
    "DbBtBtBtBtBtBtbD",
    "DDDDDDDDDDDDDDDD",
    "DBtBtBtBtBtBtBtD",
    "DBtBtBtBtBtBtBtD",
    "DDDDDDDDDDDDDDDD",
    "DbBtBtBtBtBtBtbD",
    "DbBtBtBtBtBtBtbD",
    "DDDDDDDDDDDDDDDD",
];

const WALL_BASE_FRONT: &[&str] = &[
    "DDDDDDDDDDDDDDDD",
    "DbBtBtBtBtBtBtbD",
    "DbBtBtBtBtBtBtbD",
    "DDDDDDDDDDDDDDDD",
    "DBtBtBtBtBtBtBtD",
    "DBtBtBtBtBtBtBtD",
    "DDDDDDDDDDDDDDDD",
    "DbBtBtBtBtBtBtbD",
    "DbBtBtBtBtBtBtbD",
    "DDDDDDDDDDDDDDDD",
    "DBtBtBtBtBtBtBtD",
    "DBtBtBtBtBtBtBtD",
    "DDDDDDDDDDDDDDDD",
    "DbBtBtBtBtBtBtbD",
    "DbBtBtBtBtBtBtbD",
    "DDDDDDDDDDDDDDDD",
];

// ── Axle ─────────────────────────────────────────────────────────────────

// Axle: dark iron bar spanning the full cell width.
const AXLE_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "XXXXXXXXXXXXXXXX",
    "XKKKKKKKKKKKKKKX",
    "XkkkkkkkkkkkkkkX",
    "XkKKKKKKKKKKKKkX",
    "XkkkkkkkkkkkkkkX",
    "XKKKKKKKKKKKKKKX",
    "XXXXXXXXXXXXXXXX",
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
    "XXXXXXXXXXXXXXXX",
    "XKKKKKKKKKKKKKKX",
    "XkkkkkkkkkkkkkkX",
    "XkKKKKKKKKKKKKkX",
    "XkkkkkkkkkkkkkkX",
    "XKKKKKKKKKKKKKKX",
    "XXXXXXXXXXXXXXXX",
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
    "XXXXXXX.........",
    "XKKKKKKX........",
    "XkkkkkkX........",
    "XkKKKKKKXXXXXXXX",
    "XkkkkkkkkkkkkkkX",
    "XKKKKKKKXKKKKKKX",
    "XXXXXXXX.XkkkkkX",
    "........XKKKKKKX",
    "........XXXXXXXX",
    "................",
    "................",
    "................",
];

const AXLE_REINFORCED_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "XXXXXXXXXXXXXXXX",
    "XKKKKKKKKKKKKKKX",
    "XKlPlPlPlPlPlPKX",
    "XKPKKKKKKKKKKPKX",
    "XKPKkkkkkkkkKPKX",
    "XKPKKKKKKKKKKPKX",
    "XKlPlPlPlPlPlPKX",
    "XKKKKKKKKKKKKKKX",
    "XXXXXXXXXXXXXXXX",
    "................",
    "................",
    "................",
    "................",
];

// ── Wheel ────────────────────────────────────────────────────────────────

// Wheel side-view: big round wheel filling the full cell, dark outline
// touching all four edges so it reads as a circle even at small zoom.
const WHEEL_BASE_SIDE: &[&str] = &[
    "....XXXXXXXX....",
    "..XXDDDDDDDDXX..",
    ".XDDbbbbbbbbDDX.",
    ".XDbBBBBBBBBbDX.",
    "XDbBttttttttBbDX",
    "XDBttttBBttttBDX",
    "XDBttBKXXKBttBDX",
    "XDbBtBKxxKBtBbDX",
    "XDbBtBKxxKBtBbDX",
    "XDBttBKXXKBttBDX",
    "XDBttttBBttttBDX",
    "XDbBttttttttBbDX",
    ".XDbBBBBBBBBbDX.",
    ".XDDbbbbbbbbDDX.",
    "..XXDDDDDDDDXX..",
    "....XXXXXXXX....",
];

// Wheel front-view: tyre seen edge-on. Tall and chunky so it actually
// looks like a wheel (not a barrel); reaches the cell top and bottom so
// stacked verticals don't break.
const WHEEL_BASE_FRONT: &[&str] = &[
    "....XXXXXXXX....",
    "....XDDDDDDX....",
    "....XDbbbbDX....",
    "...XXDBBBBDXX...",
    "...XDDBBBBDDX...",
    "...XDbBttBbDX...",
    "...XDbBttBbDX...",
    "...XDbBttBbDX...",
    "...XDbBttBbDX...",
    "...XDbBttBbDX...",
    "...XDbBttBbDX...",
    "...XDDBBBBDDX...",
    "...XXDBBBBDXX...",
    "....XDbbbbDX....",
    "....XDDDDDDX....",
    "....XXXXXXXX....",
];

const WHEEL_SOLID_SIDE: &[&str] = &[
    "....XXXXXXXX....",
    "..XXDDDDDDDDXX..",
    ".XDDbbbbbbbbDDX.",
    ".XDbBBBBBBBBbDX.",
    "XDbBBBBBBBBBBbDX",
    "XDBBBBBBBBBBBBDX",
    "XDBBBBBKKBBBBBDX",
    "XDbBBBBKXBBBBbDX",
    "XDbBBBBXKBBBBbDX",
    "XDBBBBBKKBBBBBDX",
    "XDBBBBBBBBBBBBDX",
    "XDbBBBBBBBBBBbDX",
    ".XDbBBBBBBBBbDX.",
    ".XDDbbbbbbbbDDX.",
    "..XXDDDDDDDDXX..",
    "....XXXXXXXX....",
];

const WHEEL_SPOKED_SIDE: &[&str] = &[
    "....XXXXXXXX....",
    "..XXDDDDDDDDXX..",
    ".XDDbb.BB.bbDDX.",
    ".XDb.BBttBB.bDX.",
    "XDbBB.tttt.BBbDX",
    "XDB.Btt..ttB.BDX",
    "XDBBtt.KK.ttBBDX",
    "XDbBt..KXKB.tBbX",
    "XDbBt.XKXK.tBbDX",
    "XDBBtt.KK.ttBBDX",
    "XDB.Btt..ttB.BDX",
    "XDbBB.tttt.BBbDX",
    ".XDb.BBttBB.bDX.",
    ".XDDbb.BB.bbDDX.",
    "..XXDDDDDDDDXX..",
    "....XXXXXXXX....",
];

const WHEEL_IRON_RIM_SIDE: &[&str] = &[
    "....KKKKKKKK....",
    "..KKkkkkkkkkKK..",
    ".KkkPPPPPPPPkkK.",
    ".KkPbBBBBBBbPkK.",
    "KkPbBttttttBbPkK",
    "KkPBttttBBttBPkK",
    "KkPBttBKXKBtBPkK",
    "KkPbBtBKxKBtBPkK",
    "KkPbBtBKxKBtBPkK",
    "KkPBttBKXKBtBPkK",
    "KkPBttttBBttBPkK",
    "KkPbBttttttBbPkK",
    ".KkPbBBBBBBbPkK.",
    ".KkkPPPPPPPPkkK.",
    "..KKkkkkkkkkKK..",
    "....KKKKKKKK....",
];

const WHEEL_IRON_RIM_FRONT: &[&str] = &[
    "....KKKKKKKK....",
    "....KkkkkkkK....",
    "....KkPPPPkK....",
    "...KKkPPPPkKK...",
    "...KkkPPPPkkK...",
    "...KkPbBBBPkK...",
    "...KkPbBBBPkK...",
    "...KkPbBBBPkK...",
    "...KkPbBBBPkK...",
    "...KkPbBBBPkK...",
    "...KkPbBBBPkK...",
    "...KkkPPPPkkK...",
    "...KKkPPPPkKK...",
    "....KkPPPPkK....",
    "....KkkkkkkK....",
    "....KKKKKKKK....",
];

// ── Hitch / Yoke ─────────────────────────────────────────────────────────

// Hitch: a single drawbar with a ring/eye at the forward end. Bar
// stretches full cell length so it connects the body cell behind it.
const HITCH_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    ".....DDDD.......",
    "....XKKKKX......",
    "....XKllPX......",
    "....XKlPKXDDDDDD",
    "....XKlPKXbBBBBb",
    "....XKKKKXbttttt",
    "....XKllPXbBBBBb",
    "....XKlPKXbbbbbb",
    "....XKKKKXDDDDDD",
    ".....DDDD.......",
    "................",
];

const HITCH_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    ".....DDDD.......",
    "....DKKKKD......",
    "....DKllPD......",
    "....DKlPKD......",
    "....DKlPKD......",
    "....DKKKKD......",
    "...DDDKKDDD.....",
    "...DbBKKBbD.....",
    "...DBtKKtBD.....",
    "...DbBKKBbD.....",
    "...DDDKKDDD.....",
    "................",
    "................",
];

// HITCH viewed from behind: the drawbar is hidden behind the chassis end-cap
// so we show the cap with the eye-loop opening (vs. front view's exposed
// pin + cotter-key detail).
const HITCH_BASE_BACK: &[&str] = &[
    "................",
    "................",
    "................",
    "...DDDDDDDDDD...",
    "...DKKKKKKKKD...",
    "...DKKlPlPKKD...",
    "...DKPKKKKPKD...",
    "...DKPKllKPKD...",
    "...DKPKllKPKD...",
    "...DKPKKKKPKD...",
    "...DKKlPlPKKD...",
    "...DKKKKKKKKD...",
    "...DDDDDDDDDD...",
    "................",
    "................",
    "................",
];

// Yoke: two attachment points on a crossbar.
const YOKE_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "................",
    "DD............DD",
    "DbDD........DDbD",
    "DBbDD......DDbBD",
    "DBBbD......DbBBD",
    "DBBBDDDDDDDDBBBD",
    "DBttBBBBBBBBttBD",
    "DBttttttttttttBD",
    "DBBBBBBBBBBBBBBD",
    "DbbDD......DDbbD",
    "DDDD........DDDD",
    "................",
    "................",
    "................",
];

const YOKE_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "................",
    "...DD......DD...",
    "...DKD....DKD...",
    "...DKKDDDDKKD...",
    "...DKKKKKKKKD...",
    "...DKllPlPlKD...",
    "...DKKKKKKKKD...",
    "...DKKDDDDKKD...",
    "...DKD....DKD...",
    "...DD......DD...",
    "................",
    "................",
    "................",
    "................",
];

// YOKE from behind: viewer sees the inside of the harness — strap buckles
// rather than the leading crossbar end-caps.
const YOKE_BASE_BACK: &[&str] = &[
    "................",
    "................",
    "................",
    "....DD....DD....",
    "...DBBDDDDBBD...",
    "...DBTTTTTTBD...",
    "...DBTKllPKBD...",
    "...DBTKPKKPKBD..",
    "...DBTKKKKKKBD..",
    "...DBTTTTTTBD...",
    "...DBBDDDDBBD...",
    "....DD....DD....",
    "................",
    "................",
    "................",
    "................",
];

// ── CargoBay ─────────────────────────────────────────────────────────────

// CargoBay: a slatted wooden crate filling almost the whole cell.
const CARGOBAY_BASE_SIDE: &[&str] = &[
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DBBBBBBBBBBBBBBD",
    "DBTTTTTTTTTTTTBD",
    "DBTtTtTtTtTtTTBD",
    "DBTtTtToTtTtTTBD",
    "DBTtTtToTtTtTTBD",
    "DBTtTtTtTtTtTTBD",
    "DBTTTTTTTTTTTTBD",
    "DBBBBBBBBBBBBBBD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

const CARGOBAY_BASE_FRONT: &[&str] = &[
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DBBBBBBBBBBBBBBD",
    "DBTTTTTTTTTTTTBD",
    "DBTtTtTtTtTtTTBD",
    "DBTtTtToTtTtTTBD",
    "DBTtTtToTtTtTTBD",
    "DBTtTtTtTtTtTTBD",
    "DBTTTTTTTTTTTTBD",
    "DBBBBBBBBBBBBBBD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

// ── CrewSeat ─────────────────────────────────────────────────────────────

// CrewSeat: a tall red seatback rising out of a wooden platform.
const CREWSEAT_BASE_SIDE: &[&str] = &[
    "................",
    "....DDDDDDDD....",
    "....DrRrRrRD....",
    "....DRRRRRRD....",
    "....DrRrRrRD....",
    "....DRRRRRRD....",
    "....DrRrRrRD....",
    "....DRRRRRRD....",
    "DDDDDRrRrRrDDDDD",
    "DbBBBRRRRRRBBBbD",
    "DBttttttttttttBD",
    "DbBBBBBBBBBBBBbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

const CREWSEAT_BASE_FRONT: &[&str] = &[
    "................",
    "...DDDDDDDDDD...",
    "...DRrRrRrRrRD..",
    "...DRRRRRRRRRD..",
    "...DRrRrRrRrRD..",
    "...DRRRRRRRRRD..",
    "...DRrRrRrRrRD..",
    "...DRRRRRRRRRD..",
    "DDDDDRrRrRrRDDDD",
    "DbBBBRRRRRRRBBBD",
    "DBttttttttttttBD",
    "DbBBBBBBBBBBBBbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

// CrewSeat from behind: viewer sees the rounded headrest top of the
// backrest (front view shows the seat opening / cushion edge). Same
// platform underneath so stacked-seats run continues.
const CREWSEAT_BASE_BACK: &[&str] = &[
    "................",
    "....DDDDDDDD....",
    "...DRRRRRRRRD...",
    "...DRrRrRrRrD...",
    "...DRRRRRRRRD...",
    "...DRrRrRrRrD...",
    "...DRRRRRRRRD...",
    "...DRrRrRrRrD...",
    "DDDDRRRRRRRRDDDD",
    "DbBBRRRRRRRRBBBD",
    "DBttttttttttttBD",
    "DbBBBBBBBBBBBBbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
];

// ── WeaponMount ──────────────────────────────────────────────────────────

// WeaponMount: stubby barrel rising from a reinforced base platform.
const WEAPONMOUNT_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    ".......XKX......",
    ".......XKKX.....",
    ".......XKKKKKKKK",
    ".......XKllPllPK",
    ".......XKKKKKKKK",
    ".......XKKX.....",
    ".......XKX......",
    "DDDDDDDXKXDDDDDD",
    "DKKKKKKKKKKKKKKD",
    "DKlPlPlPlPlPlPKD",
    "DKPKKKKKKKKKKPKD",
    "DKKKKKKKKKKKKKKD",
    "DDDDDDDDDDDDDDDD",
    "................",
];

const WEAPONMOUNT_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    ".....XKKKKX.....",
    "....XKKKKKKX....",
    "DDDDXKKKKKKXDDDD",
    "DKKKKKKKKKKKKKKD",
    "DKlPlPlPlPlPlPKD",
    "DKPKKKKKKKKKKPKD",
    "DKKKKKKKKKKKKKKD",
    "DDDDDDDDDDDDDDDD",
    "................",
];

// WeaponMount from behind: no barrel forward of the platform — instead
// the breech-block and recoil-dampener stand proud at chassis-rear.
const WEAPONMOUNT_BASE_BACK: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    ".....XKKKKX.....",
    "....XKllPlKX....",
    "....XKPKKPKX....",
    "DDDDXKKKKKKXDDDD",
    "DKKKKKKKKKKKKKKD",
    "DKlPlPlPlPlPlPKD",
    "DKPKKKKKKKKKKPKD",
    "DKKKKKKKKKKKKKKD",
    "DDDDDDDDDDDDDDDD",
    "................",
];

// ── Engine ───────────────────────────────────────────────────────────────

// Engine: heavy iron block with smokestack and exhaust glow.
const ENGINE_BASE_SIDE: &[&str] = &[
    "................",
    "................",
    "......XKKX......",
    "......XKoX......",
    "......XKKX......",
    "XXXXXXXKKXXXXXXX",
    "XKKKKKKKKKKKKKKX",
    "XKlPlPlPlPlPlPKX",
    "XKKKKKKKKKKKKKKX",
    "XKlPRRRRRRRRPlKX",
    "XKlPRoooooooRPKX",
    "XKlPRoXXXXXoRPKX",
    "XKlPRRRRRRRRPlKX",
    "XKKKKKKKKKKKKKKX",
    "XXXXXXXXXXXXXXXX",
    "................",
];

const ENGINE_BASE_FRONT: &[&str] = &[
    "................",
    "................",
    "......XKKX......",
    "......XKoX......",
    "......XKKX......",
    "XXXXXXXKKXXXXXXX",
    "XKKKKKKKKKKKKKKX",
    "XKlPRRRRRRRRPlKX",
    "XKlPRoooooooRPKX",
    "XKlPRoXXXXXoRPKX",
    "XKlPRoXXXXXoRPKX",
    "XKlPRoooooooRPKX",
    "XKlPRRRRRRRRPlKX",
    "XKKKKKKKKKKKKKKX",
    "XXXXXXXXXXXXXXXX",
    "................",
];

// Engine from behind: exhaust manifold + cooling louvers; no firebox
// glow visible from the rear face.
const ENGINE_BASE_BACK: &[&str] = &[
    "................",
    "....XKX..XKX....",
    "....XoX..XoX....",
    "....XKX..XKX....",
    "....XKX..XKX....",
    "XXXXXKXXXXKXXXXX",
    "XKKKKKKKKKKKKKKX",
    "XKlPlPlPlPlPlPKX",
    "XKPKKKKKKKKKKPKX",
    "XKlPlPlPlPlPlPKX",
    "XKPKKKKKKKKKKPKX",
    "XKlPlPlPlPlPlPKX",
    "XKPKKKKKKKKKKPKX",
    "XKKKKKKKKKKKKKKX",
    "XXXXXXXXXXXXXXXX",
    "................",
];

// ── Track ────────────────────────────────────────────────────────────────

// Track: continuous tread band, full cell width, twin treadlines top/bottom.
const TRACK_BASE_SIDE: &[&str] = &[
    "................",
    "XXXXXXXXXXXXXXXX",
    "XDDDDDDDDDDDDDDX",
    "XDKKKKKKKKKKKKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKPXPXPXPXPXKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKPXPXPXPXPXKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKPXPXPXPXPXKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKKKKKKKKKKKKDX",
    "XDDDDDDDDDDDDDDX",
    "XXXXXXXXXXXXXXXX",
    "................",
    "................",
];

const TRACK_BASE_FRONT: &[&str] = &[
    "................",
    "XXXXXXXXXXXXXXXX",
    "XDDDDDDDDDDDDDDX",
    "XDKKKKKKKKKKKKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKPXPXPXPXPXKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKPXPXPXPXPXKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKPXPXPXPXPXKDX",
    "XDKlPlPlPlPlPKDX",
    "XDKKKKKKKKKKKKDX",
    "XDDDDDDDDDDDDDDX",
    "XXXXXXXXXXXXXXXX",
    "................",
    "................",
];

const TRACK_METAL_SIDE: &[&str] = &[
    "................",
    "XXXXXXXXXXXXXXXX",
    "XkkkkkkkkkkkkkkX",
    "XkPPPPPPPPPPPPkX",
    "XkPlPlPlPlPlPlkX",
    "XkPXXXXXXXXXXPkX",
    "XkPlPlPlPlPlPlkX",
    "XkPXXXXXXXXXXPkX",
    "XkPlPlPlPlPlPlkX",
    "XkPXXXXXXXXXXPkX",
    "XkPlPlPlPlPlPlkX",
    "XkPPPPPPPPPPPPkX",
    "XkkkkkkkkkkkkkkX",
    "XXXXXXXXXXXXXXXX",
    "................",
    "................",
];

// ── ArmorPlate ───────────────────────────────────────────────────────────

// ArmorPlate: bolted metal panel filling the cell.
const ARMORPLATE_BASE_SIDE: &[&str] = &[
    "KKKKKKKKKKKKKKKK",
    "KlPlPlPlPlPlPlPK",
    "KPsKKKKKKKKKKsPK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPsKKKKKKKKKKsPK",
    "KlPlPlPlPlPlPlPK",
    "KKKKKKKKKKKKKKKK",
    "................",
];

const ARMORPLATE_BASE_FRONT: &[&str] = &[
    "KKKKKKKKKKKKKKKK",
    "KlPlPlPlPlPlPlPK",
    "KPsKKKKKKKKKKsPK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPKPlPlPlPlPKPlK",
    "KlKlPlPlPlPlKlPK",
    "KPsKKKKKKKKKKsPK",
    "KlPlPlPlPlPlPlPK",
    "KKKKKKKKKKKKKKKK",
    "................",
];

// ── Turret ───────────────────────────────────────────────────────────────

// Turret: round dome filling the cell, barrel pointing forward.
const TURRET_BASE_SIDE: &[&str] = &[
    "...XXXXXXXX.....",
    ".XXkkkkkkkkXX...",
    "XkkKKKKKKKKkkX..",
    "XkKlPlPlPlPKkX..",
    "XkKPKKKKKKKPKkX.",
    "XkKKKKKKKKKKKkXX",
    "XkKlPlPlPlPlKkKK",
    "XkKKKKKKKKKKKkKK",
    "XkKKKKKKKKKKKkXX",
    "XkKlPlPlPlPlKkX.",
    "XkKPKKKKKKKPKkX.",
    "XkKKKKKKKKKKKkX.",
    "XkkKKKKKKKKkkX..",
    ".XXkkkkkkkkXX...",
    "...XXXXXXXX.....",
    "................",
];

const TURRET_BASE_FRONT: &[&str] = &[
    "....XXXXXXXX....",
    "..XXkkkkkkkkXX..",
    ".XkkKKKKKKKKkkX.",
    ".XkKlPlPlPlPKkX.",
    "XkKlPlPlPlPlPKkX",
    "XkKPKKKKKKKKKPkX",
    "XkKKKKKKKKKKKKkX",
    "XkKlPKKKKKKlPKkX",
    "XkKlPKxxxxKlPKkX",
    "XkKPKKKKKKKKKPkX",
    "XkKlPlPlPlPlPKkX",
    ".XkKKKKKKKKKKkX.",
    ".XkkKKKKKKKKkkX.",
    "..XXkkkkkkkkXX..",
    "....XXXXXXXX....",
    "................",
];

// Turret from behind: no muzzle aperture; cooling-louver grille instead.
const TURRET_BASE_BACK: &[&str] = &[
    "....XXXXXXXX....",
    "..XXkkkkkkkkXX..",
    ".XkkKKKKKKKKkkX.",
    ".XkKPlPlPlPlKkX.",
    "XkKPlPlPlPlPlKkX",
    "XkKKKKKKKKKKKKkX",
    "XkKlPKKllKKlPKkX",
    "XkKlPKllllKlPKkX",
    "XkKlPKllllKlPKkX",
    "XkKlPKKllKKlPKkX",
    "XkKKKKKKKKKKKKkX",
    "XkKPlPlPlPlPlKkX",
    ".XkKPlPlPlPlKkX.",
    ".XkkKKKKKKKKkkX.",
    "..XXkkkkkkkkXX..",
    "....XXXXXXXX....",
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

// ── Connector overlays ───────────────────────────────────────────────────
// Each 16×16 sprite is mostly transparent, painting only the hardware that
// bridges the seam to a same-direction neighbor. Drawn on the *source*
// cell at +0.001 z-order so it lays over the source's transparent border
// rows; the matching neighbor (when present) emits its own mirror overlay,
// so a stacked-frame pair gets two overlays meeting at the seam.

// Axle ↔ Wheel hub stub — wheel sits directly below the axle (screen-down).
// Fills the bottom 4 transparent rows of the axle cell with a centred iron
// stub aligning with the wheel hub at the next cell's top.
const CONNECTOR_AXLE_WHEEL_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
];

// Axle ↔ Wheel hub stub — wheel sits directly above the axle (screen-up).
const CONNECTOR_AXLE_WHEEL_UP: &[&str] = &[
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "......XKKX......",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// Axle ↔ Wheel hub stub — wheel on chassis-left (camera-left) in front view.
const CONNECTOR_AXLE_WHEEL_LEFT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "XXXX............",
    "KKKK............",
    "KKKK............",
    "XXXX............",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// Axle ↔ Wheel hub stub — wheel on chassis-right in front view.
const CONNECTOR_AXLE_WHEEL_RIGHT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "............XXXX",
    "............KKKK",
    "............KKKK",
    "............XXXX",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// Frame seam — vertical stack (axisY): paint the cell's bottom 4 transparent
// rows (rows 12-15) with beam continuation so the gap to the cell below
// closes. Emitted on a cell whose grid −Z (screen-down) neighbour is also
// a Frame.
const CONNECTOR_FRAME_SEAM_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DBBBBBBBBBBBBBBD",
    "DBttttttttttttBD",
];

// Frame seam — vertical stack (axisY): paint the cell's top 3 transparent
// rows with beam continuation so the gap to the cell above closes.
const CONNECTOR_FRAME_SEAM_UP: &[&str] = &[
    "DbBBBBBBBBBBBBbD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

// Frame seam — horizontal (axisX): replace the cell's right outline column
// with beam fill so two horizontally-adjacent frames read as one beam.
const CONNECTOR_FRAME_SEAM_RIGHT: &[&str] = &[
    "................",
    "................",
    "................",
    "...............D",
    "...............b",
    "...............B",
    "...............t",
    "...............b",
    "...............t",
    "...............B",
    "...............b",
    "...............D",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_FRAME_SEAM_LEFT: &[&str] = &[
    "................",
    "................",
    "................",
    "D...............",
    "b...............",
    "B...............",
    "t...............",
    "b...............",
    "t...............",
    "B...............",
    "b...............",
    "D...............",
    "................",
    "................",
    "................",
    "................",
];

// Deck seam — vertical stack: deck has rows 0-1 + 13-15 transparent, so
// gap is 2 + 3 = 5 rows.
const CONNECTOR_DECK_SEAM_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DbbbbbbbbbbbbbbD",
    "DTtTtTtTtTtTtTtD",
];

const CONNECTOR_DECK_SEAM_UP: &[&str] = &[
    "DTtTtTtTtTtTtTtD",
    "DbbbbbbbbbbbbbbD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_DECK_SEAM_RIGHT: &[&str] = &[
    "................",
    "................",
    "...............D",
    "...............b",
    "...............T",
    "...............t",
    "...............T",
    "...............b",
    "...............T",
    "...............t",
    "...............T",
    "...............b",
    "...............D",
    "................",
    "................",
    "................",
];

const CONNECTOR_DECK_SEAM_LEFT: &[&str] = &[
    "................",
    "................",
    "D...............",
    "b...............",
    "T...............",
    "t...............",
    "T...............",
    "b...............",
    "T...............",
    "t...............",
    "T...............",
    "b...............",
    "D...............",
    "................",
    "................",
    "................",
];

// Wall already fills its cell edge-to-edge; only the inner seam between
// two stacked walls benefits from a slim bridging course.
const CONNECTOR_WALL_SEAM_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "DDDDDDDDDDDDDDDD",
    "DBtBtBtBtBtBtBtD",
];

const CONNECTOR_WALL_SEAM_UP: &[&str] = &[
    "DBtBtBtBtBtBtBtD",
    "DDDDDDDDDDDDDDDD",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_WALL_SEAM_RIGHT: &[&str] = &[
    "...............D",
    "...............B",
    "...............b",
    "...............D",
    "...............B",
    "...............t",
    "...............D",
    "...............B",
    "...............b",
    "...............D",
    "...............B",
    "...............t",
    "...............D",
    "...............B",
    "...............b",
    "...............D",
];

const CONNECTOR_WALL_SEAM_LEFT: &[&str] = &[
    "D...............",
    "B...............",
    "b...............",
    "D...............",
    "B...............",
    "t...............",
    "D...............",
    "B...............",
    "b...............",
    "D...............",
    "B...............",
    "t...............",
    "D...............",
    "B...............",
    "b...............",
    "D...............",
];

// Hitch ↔ Frame attachment — tongue extends toward the frame neighbour.
// One template per screen direction (the emission logic maps the
// chassis-rear grid direction to camera-space).
const CONNECTOR_HITCH_ATTACH_RIGHT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "............DDDD",
    "............DKKD",
    "............DKKD",
    "............DKKD",
    "............DKKD",
    "............DDDD",
    "................",
    "................",
    "................",
];

const CONNECTOR_HITCH_ATTACH_LEFT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "DDDD............",
    "DKKD............",
    "DKKD............",
    "DKKD............",
    "DKKD............",
    "DDDD............",
    "................",
    "................",
    "................",
];

const CONNECTOR_HITCH_ATTACH_UP: &[&str] = &[
    "......DDDD......",
    "......DKKD......",
    "......DKKD......",
    "......DKKD......",
    "......DKKD......",
    "......DDDD......",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_HITCH_ATTACH_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "......DDDD......",
    "......DKKD......",
    "......DKKD......",
    "......DKKD......",
    "......DKKD......",
    "......DDDD......",
];

// Yoke ↔ Frame attachment — strap-buckle plate on the side facing the
// frame neighbour.
const CONNECTOR_YOKE_ATTACH_RIGHT: &[&str] = &[
    "................",
    "................",
    "................",
    "............DDDD",
    "............DBBD",
    "............DBKD",
    "............DKKD",
    "............DKKD",
    "............DKKD",
    "............DKKD",
    "............DBKD",
    "............DBBD",
    "............DDDD",
    "................",
    "................",
    "................",
];

const CONNECTOR_YOKE_ATTACH_LEFT: &[&str] = &[
    "................",
    "................",
    "................",
    "DDDD............",
    "DBBD............",
    "DKBD............",
    "DKKD............",
    "DKKD............",
    "DKKD............",
    "DKKD............",
    "DKBD............",
    "DBBD............",
    "DDDD............",
    "................",
    "................",
    "................",
];

const CONNECTOR_YOKE_ATTACH_UP: &[&str] = &[
    "...DDBBKKKKBBDD.",
    "...DBKKKKKKKKBD.",
    "...DBKllPlPllKD.",
    "...DDBBKKKKBBDD.",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_YOKE_ATTACH_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "...DDBBKKKKBBDD.",
    "...DBKllPlPllKD.",
    "...DBKKKKKKKKBD.",
    "...DDBBKKKKBBDD.",
];

// Crew-seat facing indicator — small tiller/headrest stub pointing at
// chassis-forward in screen coordinates, telling the player which way
// the occupant is looking. The four directions cover the four heading
// rotations after view selection.
const CONNECTOR_CREW_SEAT_FACING_UP: &[&str] = &[
    ".......XKKX.....",
    "......XKllKX....",
    "......XKllKX....",
    ".......XKKX.....",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_CREW_SEAT_FACING_DOWN: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    ".......XKKX.....",
    "......XKllKX....",
    "......XKllKX....",
    ".......XKKX.....",
    "................",
];

const CONNECTOR_CREW_SEAT_FACING_RIGHT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "..............XK",
    ".........XXXXXXX",
    ".........XKKKKKK",
    "..............XK",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
];

const CONNECTOR_CREW_SEAT_FACING_LEFT: &[&str] = &[
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
    "KX..............",
    "XXXXXXX.........",
    "KKKKKKKX........",
    "KX..............",
    "................",
    "................",
    "................",
    "................",
    "................",
    "................",
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
    // Symmetric parts (Frame, Deck, Wall, Axle, Wheel, CargoBay, Track,
    // ArmorPlate) re-register Front art under the `_back` key — the rear
    // silhouette is indistinguishable from the front. Asymmetric parts
    // (Hitch, Yoke, CrewSeat, WeaponMount, Engine, Turret) ship distinct
    // `_BACK` templates.
    insert!("vehicle_frame_base_side", FRAME_BASE_SIDE);
    insert!("vehicle_frame_base_front", FRAME_BASE_FRONT);
    insert!("vehicle_frame_base_back", FRAME_BASE_FRONT);
    insert!("vehicle_deck_base_side", DECK_BASE_SIDE);
    insert!("vehicle_deck_base_front", DECK_BASE_FRONT);
    insert!("vehicle_deck_base_back", DECK_BASE_FRONT);
    insert!("vehicle_wall_base_side", WALL_BASE_SIDE);
    insert!("vehicle_wall_base_front", WALL_BASE_FRONT);
    insert!("vehicle_wall_base_back", WALL_BASE_FRONT);
    insert!("vehicle_axle_base_side", AXLE_BASE_SIDE);
    insert!("vehicle_axle_base_front", AXLE_BASE_FRONT);
    insert!("vehicle_axle_base_back", AXLE_BASE_FRONT);
    insert!("vehicle_wheel_base_side", WHEEL_BASE_SIDE);
    insert!("vehicle_wheel_base_front", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_base_back", WHEEL_BASE_FRONT);
    insert!("vehicle_hitch_base_side", HITCH_BASE_SIDE);
    insert!("vehicle_hitch_base_front", HITCH_BASE_FRONT);
    insert!("vehicle_hitch_base_back", HITCH_BASE_BACK);
    insert!("vehicle_yoke_base_side", YOKE_BASE_SIDE);
    insert!("vehicle_yoke_base_front", YOKE_BASE_FRONT);
    insert!("vehicle_yoke_base_back", YOKE_BASE_BACK);
    insert!("vehicle_cargo_bay_base_side", CARGOBAY_BASE_SIDE);
    insert!("vehicle_cargo_bay_base_front", CARGOBAY_BASE_FRONT);
    insert!("vehicle_cargo_bay_base_back", CARGOBAY_BASE_FRONT);
    insert!("vehicle_crew_seat_base_side", CREWSEAT_BASE_SIDE);
    insert!("vehicle_crew_seat_base_front", CREWSEAT_BASE_FRONT);
    insert!("vehicle_crew_seat_base_back", CREWSEAT_BASE_BACK);
    insert!("vehicle_weapon_mount_base_side", WEAPONMOUNT_BASE_SIDE);
    insert!("vehicle_weapon_mount_base_front", WEAPONMOUNT_BASE_FRONT);
    insert!("vehicle_weapon_mount_base_back", WEAPONMOUNT_BASE_BACK);
    insert!("vehicle_engine_base_side", ENGINE_BASE_SIDE);
    insert!("vehicle_engine_base_front", ENGINE_BASE_FRONT);
    insert!("vehicle_engine_base_back", ENGINE_BASE_BACK);
    insert!("vehicle_track_base_side", TRACK_BASE_SIDE);
    insert!("vehicle_track_base_front", TRACK_BASE_FRONT);
    insert!("vehicle_track_base_back", TRACK_BASE_FRONT);
    insert!("vehicle_armor_plate_base_side", ARMORPLATE_BASE_SIDE);
    insert!("vehicle_armor_plate_base_front", ARMORPLATE_BASE_FRONT);
    insert!("vehicle_armor_plate_base_back", ARMORPLATE_BASE_FRONT);
    insert!("vehicle_turret_base_side", TURRET_BASE_SIDE);
    insert!("vehicle_turret_base_front", TURRET_BASE_FRONT);
    insert!("vehicle_turret_base_back", TURRET_BASE_BACK);

    // ── Visually distinct variants ───────────────────────────────────
    // Frame variants reuse the base (mass/support only; no readable
    // silhouette difference at 16 px). Provide truss + heavy + light as
    // distinct keys so the resolver hits them.
    insert!("vehicle_frame_light_chassis_side", FRAME_LIGHT_SIDE);
    insert!("vehicle_frame_light_chassis_front", FRAME_BASE_FRONT);
    insert!("vehicle_frame_light_chassis_back", FRAME_BASE_FRONT);
    insert!("vehicle_frame_heavy_chassis_side", FRAME_HEAVY_SIDE);
    insert!("vehicle_frame_heavy_chassis_front", FRAME_BASE_FRONT);
    insert!("vehicle_frame_heavy_chassis_back", FRAME_BASE_FRONT);
    insert!("vehicle_frame_truss_chassis_side", FRAME_TRUSS_SIDE);
    insert!("vehicle_frame_truss_chassis_front", FRAME_BASE_FRONT);
    insert!("vehicle_frame_truss_chassis_back", FRAME_BASE_FRONT);

    // Wheel variants.
    insert!("vehicle_wheel_solid_wheel_side", WHEEL_SOLID_SIDE);
    insert!("vehicle_wheel_solid_wheel_front", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_solid_wheel_back", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_spoked_wheel_side", WHEEL_SPOKED_SIDE);
    insert!("vehicle_wheel_spoked_wheel_front", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_spoked_wheel_back", WHEEL_BASE_FRONT);
    insert!("vehicle_wheel_iron_rim_wheel_side", WHEEL_IRON_RIM_SIDE);
    insert!("vehicle_wheel_iron_rim_wheel_front", WHEEL_IRON_RIM_FRONT);
    insert!("vehicle_wheel_iron_rim_wheel_back", WHEEL_IRON_RIM_FRONT);

    // Axle variants — fixed reuses base; steering + reinforced have
    // distinct silhouettes.
    insert!("vehicle_axle_fixed_axle_side", AXLE_BASE_SIDE);
    insert!("vehicle_axle_fixed_axle_front", AXLE_BASE_FRONT);
    insert!("vehicle_axle_fixed_axle_back", AXLE_BASE_FRONT);
    insert!("vehicle_axle_steering_axle_side", AXLE_STEERING_SIDE);
    insert!("vehicle_axle_steering_axle_front", AXLE_BASE_FRONT);
    insert!("vehicle_axle_steering_axle_back", AXLE_BASE_FRONT);
    insert!("vehicle_axle_reinforced_axle_side", AXLE_REINFORCED_SIDE);
    insert!("vehicle_axle_reinforced_axle_front", AXLE_BASE_FRONT);
    insert!("vehicle_axle_reinforced_axle_back", AXLE_BASE_FRONT);

    // Track variants — wooden_track reuses base; metal_track distinct.
    insert!("vehicle_track_wooden_track_side", TRACK_BASE_SIDE);
    insert!("vehicle_track_wooden_track_front", TRACK_BASE_FRONT);
    insert!("vehicle_track_wooden_track_back", TRACK_BASE_FRONT);
    insert!("vehicle_track_metal_track_side", TRACK_METAL_SIDE);
    insert!("vehicle_track_metal_track_front", TRACK_BASE_FRONT);
    insert!("vehicle_track_metal_track_back", TRACK_BASE_FRONT);

    // ── Weapon module composites ─────────────────────────────────────
    // Module composites stay symmetric front↔back at module-asset
    // resolution (drawing 32×48 rear views per module would be a
    // separate pass of pixel art); they reuse `_front` for `_back`.
    insert!("vehicle_module_ram_head_1x2_side", RAM_HEAD_1X2_SIDE);
    insert!("vehicle_module_ram_head_1x2_front", RAM_HEAD_1X2_FRONT);
    insert!("vehicle_module_ram_head_1x2_back", RAM_HEAD_1X2_FRONT);
    insert!(
        "vehicle_module_battering_ram_2x3_side",
        BATTERING_RAM_2X3_SIDE
    );
    insert!(
        "vehicle_module_battering_ram_2x3_front",
        BATTERING_RAM_2X3_FRONT
    );
    insert!(
        "vehicle_module_battering_ram_2x3_back",
        BATTERING_RAM_2X3_FRONT
    );
    insert!("vehicle_module_ballista_2x2_side", BALLISTA_2X2_SIDE);
    insert!("vehicle_module_ballista_2x2_front", BALLISTA_2X2_FRONT);
    insert!("vehicle_module_ballista_2x2_back", BALLISTA_2X2_FRONT);
    insert!(
        "vehicle_module_light_turret_2x2_side",
        LIGHT_TURRET_2X2_SIDE
    );
    insert!(
        "vehicle_module_light_turret_2x2_front",
        LIGHT_TURRET_2X2_FRONT
    );
    insert!(
        "vehicle_module_light_turret_2x2_back",
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
    insert!(
        "vehicle_module_heavy_turret_3x3_back",
        HEAVY_TURRET_3X3_FRONT
    );

    // ── Connector overlays ───────────────────────────────────────────
    // Naming: `vehicle_connector_<label>_<view>_<dir>`. Axle↔wheel and
    // crew-seat-facing overlays are symmetric across views (down stub is
    // down stub regardless of camera), so the same constant registers
    // under all three views. Same-kind seams reuse the same per-axis
    // template for every view too.
    macro_rules! insert_for_views {
        ($label:expr, $dir:expr, $art:expr) => {{
            for view in &["side", "front", "back"] {
                let key = format!("vehicle_connector_{}_{}_{}", $label, view, $dir);
                let img = ascii_to_image($art, WARM_PALETTE);
                lib.sprites.insert(key, images.add(img));
            }
        }};
    }
    insert_for_views!("axle_wheel", "down", CONNECTOR_AXLE_WHEEL_DOWN);
    insert_for_views!("axle_wheel", "up", CONNECTOR_AXLE_WHEEL_UP);
    insert_for_views!("axle_wheel", "left", CONNECTOR_AXLE_WHEEL_LEFT);
    insert_for_views!("axle_wheel", "right", CONNECTOR_AXLE_WHEEL_RIGHT);

    insert_for_views!("frame_seam", "down", CONNECTOR_FRAME_SEAM_DOWN);
    insert_for_views!("frame_seam", "up", CONNECTOR_FRAME_SEAM_UP);
    insert_for_views!("frame_seam", "right", CONNECTOR_FRAME_SEAM_RIGHT);
    insert_for_views!("frame_seam", "left", CONNECTOR_FRAME_SEAM_LEFT);

    insert_for_views!("deck_seam", "down", CONNECTOR_DECK_SEAM_DOWN);
    insert_for_views!("deck_seam", "up", CONNECTOR_DECK_SEAM_UP);
    insert_for_views!("deck_seam", "right", CONNECTOR_DECK_SEAM_RIGHT);
    insert_for_views!("deck_seam", "left", CONNECTOR_DECK_SEAM_LEFT);

    insert_for_views!("wall_seam", "down", CONNECTOR_WALL_SEAM_DOWN);
    insert_for_views!("wall_seam", "up", CONNECTOR_WALL_SEAM_UP);
    insert_for_views!("wall_seam", "right", CONNECTOR_WALL_SEAM_RIGHT);
    insert_for_views!("wall_seam", "left", CONNECTOR_WALL_SEAM_LEFT);

    insert_for_views!("hitch_attach", "up", CONNECTOR_HITCH_ATTACH_UP);
    insert_for_views!("hitch_attach", "down", CONNECTOR_HITCH_ATTACH_DOWN);
    insert_for_views!("hitch_attach", "left", CONNECTOR_HITCH_ATTACH_LEFT);
    insert_for_views!("hitch_attach", "right", CONNECTOR_HITCH_ATTACH_RIGHT);
    insert_for_views!("yoke_attach", "up", CONNECTOR_YOKE_ATTACH_UP);
    insert_for_views!("yoke_attach", "down", CONNECTOR_YOKE_ATTACH_DOWN);
    insert_for_views!("yoke_attach", "left", CONNECTOR_YOKE_ATTACH_LEFT);
    insert_for_views!("yoke_attach", "right", CONNECTOR_YOKE_ATTACH_RIGHT);

    insert_for_views!(
        "crew_seat_facing",
        "up",
        CONNECTOR_CREW_SEAT_FACING_UP
    );
    insert_for_views!(
        "crew_seat_facing",
        "down",
        CONNECTOR_CREW_SEAT_FACING_DOWN
    );
    insert_for_views!(
        "crew_seat_facing",
        "right",
        CONNECTOR_CREW_SEAT_FACING_RIGHT
    );
    insert_for_views!(
        "crew_seat_facing",
        "left",
        CONNECTOR_CREW_SEAT_FACING_LEFT
    );
}
