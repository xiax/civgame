use super::faction::{FactionMember, FactionRegistry};
use super::items::{Equipment, EquipmentSlot};
use super::lod::LodLevel;
use super::needs::Needs;
use super::schedule::{BucketSlot, SimClock};
use super::technology::LOOM_WEAVING;
use bevy::prelude::*;

/// -128 = despairing, 0 = neutral, 127 = ecstatic.
#[derive(Component, Clone, Copy, Default)]
pub struct Mood(pub i8);

/// Mood penalty for a bare torso when the faction can weave. Workers want to
/// wear clothes once the technology exists — see `plans/...purrfect-rose.md`.
/// Roughly a third of a mood band, so a bare-torso member reads "Unhappy"-ward.
pub const CLOTHING_MOOD_PENALTY: f32 = 12.0;

impl Mood {
    pub fn label(self) -> &'static str {
        match self.0 {
            100..=127 => "Ecstatic",
            60..=99 => "Happy",
            20..=59 => "Content",
            -19..=19 => "Neutral",
            -59..=-20 => "Unhappy",
            -99..=-60 => "Miserable",
            _ => "Despairing",
        }
    }
}

pub fn derive_mood_system(
    clock: Res<SimClock>,
    registry: Res<FactionRegistry>,
    mut query: Query<(
        &Needs,
        &mut Mood,
        &BucketSlot,
        &LodLevel,
        &Equipment,
        &FactionMember,
    )>,
) {
    query
        .par_iter_mut()
        .for_each(|(needs, mut mood, slot, lod, equipment, member)| {
            if *lod == LodLevel::Dormant || !clock.is_active(slot.0) {
                return;
            }

            // Distress 0..255 → mood 127..-128
            let distress = needs.avg_distress();
            let mut raw = 127.0 - (distress / 255.0) * 255.0;

            // Clothing dissatisfaction: a bare TorsoArmor slot (no cloth or
            // armor worn) in a faction that knows weaving is a real morale
            // hit — the functional consumer that justifies autonomous Cloth
            // craft demand (`jobs::compute_craft_demand`).
            let bare_torso = !equipment
                .items
                .contains_key(&EquipmentSlot::TorsoArmor);
            if bare_torso
                && registry
                    .factions
                    .get(&member.faction_id)
                    .map_or(false, |f| f.techs.has(LOOM_WEAVING))
            {
                raw -= CLOTHING_MOOD_PENALTY;
            }

            mood.0 = raw.clamp(-128.0, 127.0) as i8;
        });
}
