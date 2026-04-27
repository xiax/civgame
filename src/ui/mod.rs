use bevy::prelude::*;
use bevy_egui::EguiPlugin;

pub mod economy_panel;
pub mod hover;
pub mod hud;
pub mod inspector;
pub mod orders;
pub mod selection;
pub mod tech_panel;
pub mod world_map;

pub use selection::SelectedEntity;

pub struct UiPlugin;

impl Plugin for UiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EguiPlugin)
            .insert_resource(SelectedEntity::default())
            .insert_resource(world_map::WorldMapOpen::default())
            .insert_resource(world_map::WorldMapTexture::default())
            .insert_resource(orders::ContextMenuState::default())
            .insert_resource(tech_panel::TechPanelOpen::default())
            .add_systems(
                Update,
                (
                    world_map::world_map_toggle_system,
                    selection::click_to_select_system,
                    orders::right_click_context_menu_system,
                    orders::player_order_completion_system,
                    inspector::inspector_panel_system,
                    economy_panel::economy_panel_system,
                    hud::hud_system,
                    world_map::world_map_system,
                    tech_panel::tech_panel_system,
                    hover::hover_info_system,
                ),
            );
    }
}
