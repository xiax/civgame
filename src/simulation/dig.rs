use crate::economy::item::Item;
use crate::simulation::carry::Carrier;
use crate::simulation::excavation::{
    advance, excavation_depth_cap, AdvanceOutcome, ExcavationKey, ExcavationMap, ExcavationMode,
    LEVEL_WORK_TICKS,
};
use crate::simulation::items::GroundItem;
use crate::simulation::lod::LodLevel;
use crate::simulation::person::{AiState, PersonAI};
use crate::simulation::schedule::{BucketSlot, SimClock};
use crate::simulation::skills::{SkillKind, Skills};
use crate::simulation::tasks::TaskKind;
use crate::simulation::tools::{item_tool_tier, work_speed_mult, ToolRequirement, ToolUseKind};
use crate::world::chunk::{ChunkMap, Z_MIN};
use crate::world::chunk_streaming::{TileCarvedEvent, TileChangedEvent};
use crate::world::globe::Globe;
use crate::world::terrain::{tile_to_world, WorldGen};
use crate::world::tile::TileKind;
use bevy::prelude::*;

const DIG_XP_PER_LEVEL: u32 = 1; // 7 * 1 = 7 ≈ old DIG_XP=5 + finalize bonus

pub fn dig_system(
    mut commands: Commands,
    mut chunk_map: ResMut<ChunkMap>,
    mut excavation_map: ResMut<ExcavationMap>,
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

        // Per-level work budget, shortened by carried pick tier.
        let pick_req = ToolRequirement::any(ToolUseKind::Mine);
        let pick_speed = toolkit
            .and_then(|tk| tk.best_for(&pick_req))
            .map(|it| work_speed_mult(item_tool_tier(it)))
            .unwrap_or(1.0);
        let level_threshold = ((LEVEL_WORK_TICKS as f32 / pick_speed).ceil() as i32)
            .clamp(1, 255) as u8;
        if ai.work_progress < level_threshold {
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

        // Dig down: incremental excavation on the floor at surf_z - 1.
        // (For dual stone-on-stone columns we could chip the head at surf_z
        // independently in lockstep; v1 keeps the floor-only key so we stay
        // close to today's "one carve == surface drops by one" tempo.)
        let target_floor_z = surf_z - 1;
        let floor_kind = chunk_map.tile_at(tx, ty, target_floor_z).kind;

        // Tool gate: stone-like below HAND_DEPTH_LIMIT needs a Pick. Soil is
        // hand-diggable to level 7.
        let depth_cap = excavation_depth_cap(toolkit, floor_kind);
        let key = ExcavationKey {
            tile: (tx, ty),
            z: target_floor_z as i8,
            mode: ExcavationMode::DigDown,
        };
        let current_level = excavation_map.level_at(&key);
        if current_level >= depth_cap {
            // Worker can't push past their tool limit on this material.
            // Cancel chain so HTN can re-route to a workable site / acquire a pick.
            ai.state = AiState::Idle;
            aq.cancel_chain(&mut ai);
            continue;
        }

        let mut yields = Vec::with_capacity(2);
        let outcome = advance(
            &mut excavation_map,
            &mut chunk_map,
            &gen,
            &globe,
            key,
            &mut tile_changed,
            &mut tile_carved,
            &mut yields,
        );

        let (agent_tx, agent_ty) = (tx, ty);
        for (resource_id, qty) in yields {
            if qty == 0 {
                continue;
            }
            let item = Item::new_commodity(resource_id);
            let leftover = carrier.try_pick_up(item, qty);
            if leftover > 0 {
                let pos = tile_to_world(agent_tx, agent_ty);
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
        skills.gain_xp(SkillKind::Mining, DIG_XP_PER_LEVEL);

        // Keep the task alive across levels. Only retire at level 7 (the
        // carve), if the carrier is full, or the next level would exceed the
        // tool cap.
        // Dig yields stone; "should return to deposit" is keyed on the
        // stone item so volume + weight both gate the haul cycle.
        let stone_item =
            Item::new_commodity(crate::economy::core_ids::stone());
        let next_level_blocked = match outcome {
            AdvanceOutcome::Carved => true,
            AdvanceOutcome::Levelled { new_level } => {
                new_level >= depth_cap || carrier.should_return_to_deposit(stone_item)
            }
        };
        if next_level_blocked {
            ai.state = AiState::Idle;
            aq.advance();
        }
        // else: leave ai.state = Working, work_progress resumes from 0 above.
    }
}
