use crate::economy::item::Item;
use crate::simulation::carry::Carrier;
use crate::simulation::carve::carve_tile;
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::world::chunk::{ChunkMap, Z_MIN};
use crate::world::chunk_streaming::{TileCarvedEvent, TileChangedEvent};
use crate::world::globe::Globe;
use crate::world::terrain::{tile_to_world, WorldGen};
use crate::world::tile::TileKind;
use bevy::prelude::*;

const DIG_WORK_TICKS: u8 = 30;
const DIG_XP: u32 = 5;

pub fn dig_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    mut tile_carved: EventWriter<TileCarvedEvent>,
    clock: Res<SimClock>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    mut agent_query: Query<(
        &mut PersonAI,
        &mut crate::simulation::typed_task::ActionQueue,
        &mut Carrier,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
        Option<&crate::simulation::tools::ToolKit>,
    )>,
) {
    for (mut ai, mut aq, mut carrier, mut skills, slot, lod, toolkit) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if aq.current_task_kind() != TaskKind::Dig as u16 {
            continue;
        }

        if ai.work_progress < DIG_WORK_TICKS {
            continue;
        }
        ai.work_progress = 0;

        // Phase 3b-v: tile from typed `Task::Dig`; fall back to dest_tile
        // when the typed task is absent (un-migrated dispatch path).
        let (tx, ty) = aq
            .current
            .as_dig()
            .unwrap_or((ai.dest_tile.0 as i32, ai.dest_tile.1 as i32));
        let surf_z = chunk_map.surface_z_at(tx, ty);
        let kind = chunk_map.tile_kind_at(tx, ty).unwrap_or(TileKind::Air);

        if kind == TileKind::Air || kind.is_water_like() || surf_z <= Z_MIN {
            ai.state = AiState::Idle;
            aq.advance();
            continue;
        }

        // Dig down: carve the floor below the agent (target_floor_z = surf_z - 1).
        // The current surface tile at surf_z becomes Air (headspace), the tile
        // at surf_z - 1 becomes Dirt (the new floor). Surface drops by one.
        let target_floor_z = surf_z - 1;

        // Realistic Tool Overhaul: breaking rock needs a Pick. Soil stays
        // hand-diggable. A stone-like floor tile with no Pick is a failed /
        // no-target outcome — the worker idles without carving. No `ToolKit`
        // component at all degrades gracefully (treated as armed).
        let floor_kind = chunk_map.tile_at(tx, ty, target_floor_z).kind;
        if floor_kind.is_stone_like() {
            use crate::simulation::tools::{ToolRequirement, ToolUseKind};
            let pick_req = ToolRequirement::any(ToolUseKind::Mine);
            let has_pick = toolkit.map(|tk| tk.satisfies(&pick_req)).unwrap_or(true);
            if !has_pick {
                ai.state = AiState::Idle;
                aq.advance();
                continue;
            }
        }
        let drops = carve_tile(
            &mut chunk_map,
            &gen,
            &globe,
            tx,
            ty,
            target_floor_z,
            &mut tile_changed,
        );

        // Signal a real excavation distinct from any other tile mutation.
        // `aquifer_seep_emitter_system` consumes ONLY this event, so wall
        // stamping / road carving / plant lifecycle no longer false-trigger
        // groundwater seep on natural tiles whose climate-cell aquifer reads
        // a hair above their per-tile jittered surface.
        tile_carved.send(TileCarvedEvent {
            tx,
            ty,
            new_floor_z: target_floor_z,
        });

        for (resource_id, qty) in drops {
            if qty == 0 {
                continue;
            }
            let item = Item::new_commodity(resource_id);
            let leftover = carrier.try_pick_up(item, qty);
            if leftover > 0 {
                let pos = tile_to_world(tx, ty);
                commands.spawn((
                    GroundItem {
                        item,
                        qty: leftover,
                    },
                    Transform::from_xyz(pos.x, pos.y, 0.3),
                    GlobalTransform::default(),
                    Visibility::Visible,
                    InheritedVisibility::default(),
                    crate::world::spatial::Indexed::new(
                        crate::world::spatial::IndexedKind::GroundItem,
                    ),
                ));
            }
        }
        skills.gain_xp(SkillKind::Mining, DIG_XP);

        ai.state = AiState::Idle;
        aq.advance();
    }
}
