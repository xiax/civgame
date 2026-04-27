use crate::economy::agent::EconomicAgent;
use crate::economy::goods::Good;
use crate::simulation::carve::{carve_tile, STONE_PER_BLOCK};
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::world::chunk::{ChunkMap, Z_MIN};
use crate::world::chunk_streaming::TileChangedEvent;
use crate::world::tile::TileKind;
use bevy::prelude::*;

const DIG_WORK_TICKS: u8 = 30;
const DIG_XP: u32 = 5;

pub fn dig_system(
    mut chunk_map: ResMut<ChunkMap>,
    mut tile_changed: EventWriter<TileChangedEvent>,
    clock: Res<SimClock>,
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
            ai.state = AiState::Idle;
            ai.task_id = PersonAI::UNEMPLOYED;
            continue;
        }

        // Dig down: carve the floor below the agent (target_floor_z = surf_z - 1).
        // The current surface tile at surf_z becomes Air (headspace), the tile
        // at surf_z - 1 becomes Dirt (the new floor). Surface drops by one.
        let target_floor_z = surf_z - 1;
        let blocks = carve_tile(
            &mut chunk_map,
            tx,
            ty,
            target_floor_z,
            &mut tile_changed,
        );

        agent.add_good(Good::Stone, blocks * STONE_PER_BLOCK);
        skills.gain_xp(SkillKind::Mining, DIG_XP);

        ai.state = AiState::Idle;
        ai.task_id = PersonAI::UNEMPLOYED;
    }
}
