use bevy::prelude::*;
use super::needs::Needs;

/// -128 = despairing, 0 = neutral, 127 = ecstatic.
#[derive(Component, Clone, Copy, Default)]
pub struct Mood(pub i8);

impl Mood {
    pub fn label(self) -> &'static str {
        match self.0 {
            100..=127  => "Ecstatic",
            60..=99    => "Happy",
            20..=59    => "Content",
            -19..=19   => "Neutral",
            -59..=-20  => "Unhappy",
            -99..=-60  => "Miserable",
            _          => "Despairing",
        }
    }
}

pub fn derive_mood_system(mut query: Query<(&Needs, &mut Mood)>) {
    query.par_iter_mut().for_each(|(needs, mut mood)| {
        // Distress 0..255 → mood 127..-128
        let distress = needs.avg_distress();
        let raw = 127.0 - (distress / 255.0) * 255.0;
        mood.0 = raw.clamp(-128.0, 127.0) as i8;
    });
}
