use bevy::prelude::*;
use bevy_egui::EguiPlugin;

pub mod debug_panel;
pub mod economy_panel;
pub mod hover;
pub mod hud;
pub mod inspector;
pub mod orders;
pub mod selection;
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
                    economy_panel::economy_panel_system,
                    hud::hud_system,
                    hud::apply_draft_toggle_system,
                    world_map::world_map_system,
                    tech_panel::tech_panel_system,
                    debug_panel::debug_panel_system,
                    hover::hover_info_system,
                ),
            );
    }
}
