use bevy::prelude::*;
use bevy_egui::EguiPlugin;

pub mod activity_log;
pub mod debug_panel;
pub mod economy_panel;
pub mod hover;
pub mod hud;
pub mod inspector;
pub mod job_board;
pub mod orders;
pub mod selection;
pub mod spawn_select;
pub mod tech_panel;
pub mod world_map;

pub use selection::{SelectedEntities, SelectedEntity, SelectionDrag};

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin)
            .insert_resource(SelectedEntity::default())
            .insert_resource(SelectedEntities::default())
            .insert_resource(SelectionDrag::default())
            .insert_resource(hud::DraftToggleRequest::default())
            .insert_resource(world_map::WorldMapOpen::default())
            .insert_resource(world_map::WorldMapTexture::default())
            .insert_resource(orders::ContextMenuState::default())
            .insert_resource(orders::MilitaryMenuState::default())
            .insert_resource(tech_panel::TechPanelOpen::default())
            .insert_resource(debug_panel::DebugPanelState::default())
            .insert_resource(job_board::JobBoardPanelState::default())
            .insert_resource(activity_log::ActivityLog::default())
            .insert_resource(activity_log::CameraFocusRequest::default())
            .insert_resource(inspector::PendingInspectorAction::default())
            .insert_resource(spawn_select::SpawnSelectTexture::default())
            .add_event::<activity_log::ActivityLogEvent>()
            .add_systems(
                Update,
                spawn_select::spawn_select_system
                    .run_if(in_state(crate::GameState::SpawnSelect)),
            )
            .add_systems(
                Update,
                (
                    world_map::world_map_toggle_system,
                    selection::selection_input_system,
                    selection::selection_gizmo_system,
                    orders::military_right_click_system,
                    orders::right_click_context_menu_system,
                    orders::player_order_completion_system,
                    inspector::inspector_panel_system,
                    inspector::inspector_action_system,
                    economy_panel::economy_panel_system,
                    hud::hud_system,
                    hud::apply_draft_toggle_system,
                    world_map::world_map_system,
                    tech_panel::tech_panel_system,
                    debug_panel::debug_panel_system,
                    job_board::job_board_panel_system,
                    hover::hover_info_system,
                    activity_log::activity_log_ingest_system,
                    activity_log::activity_log_panel_system,
                    activity_log::camera_focus_system,
                )
                    .run_if(in_state(crate::GameState::Playing)),
            );
    }
}
