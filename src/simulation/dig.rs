use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::world::chunk::{ChunkMap, Z_MIN};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::globe::Globe;
use crate::world::terrain::{tile_at_3d, WorldGen};
use crate::world::tile::{TileData, TileKind};
use bevy::prelude::*;

const DIG_WORK_TICKS: u8 = 30;
const DIG_STONE_YIELD: u32 = 2;
const DIG_XP: u32 = 5;

pub fn dig_system(
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    clock: Res<SimClock>,
    gen: Res<WorldGen>,
    globe: Res<Globe>,
    mut agent_query: Query<(
        &mut PersonAI,
        &mut EconomicAgent,
        &mut Skills,
        &BucketSlot,
        &LodLevel,
    )>,
) {
    for (mut ai, mut agent, mut skills, slot, lod) in agent_query.iter_mut() {
        if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
            continue;
        }
        if ai.state != AiState::Working {
            continue;
        }
        if ai.task_id != TaskKind::Dig as u16 {
            continue;
        }

        if ai.work_progress < DIG_WORK_TICKS {
            continue;
        }
        ai.work_progress = 0;

        let tx = ai.target_tile.0 as i32;
        let ty = ai.target_tile.1 as i32;
        let surf_z = chunk_map.surface_z_at(tx, ty);
        let kind = chunk_map.tile_kind_at(tx, ty).unwrap_or(TileKind::Air);

        if kind == TileKind::Air || kind == TileKind::Water || surf_z <= Z_MIN {
            // Can't dig further down
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        // Dig Down: remove the surface tile to lower terrain by one Z level
        chunk_map.set_tile(
            tx,
            ty,
            surf_z,
            TileData {
                kind: TileKind::Air,
                elevation: 0,
                fertility: 0,
                flags: 0,
            },
        );
        let new_surf_z = chunk_map.surface_z_at(tx, ty);
        let new_tile = tile_at_3d(&chunk_map, &gen, &globe, tx, ty, new_surf_z);
        if new_tile.kind == TileKind::Wall {
            chunk_map.set_tile(
                tx,
                ty,
                new_surf_z,
                TileData {
                    kind: TileKind::Dirt,
                    elevation: 0,
                    fertility: 0,
                    flags: 0,
                },
            );
        } else if chunk_map.tile_delta_at(tx, ty, new_surf_z).is_none() {
            // Write proc-gen tile as delta so surface_kind cache stays correct
            chunk_map.set_tile(tx, ty, new_surf_z, new_tile);
        }

        agent.add_good(Good::Stone, DIG_STONE_YIELD);
        skills.gain_xp(SkillKind::Mining, DIG_XP);
        tile_changed.send(TileChangedEvent {
            tx: ai.target_tile.0,
            ty: ai.target_tile.1,
        });

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
    }
}
