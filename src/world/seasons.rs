use bevy::prelude::*;

const TICKS_PER_DAY: u32 = 3600;
const DAYS_PER_SEASON: u32 = 30;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Season {
    #[default]
    Spring = 0,
    Summer = 1,
    Autumn = 2,
    Winter = 3,
}

impl Season {
    pub fn name(self) -> &'static str {
        match self {
            Season::Spring => "Spring",
            Season::Summer => "Summer",
            Season::Autumn => "Autumn",
            Season::Winter => "Winter",
        }
    }

    fn next(self) -> Season {
        match self {
            Season::Spring => Season::Summer,
            Season::Summer => Season::Autumn,
            Season::Autumn => Season::Winter,
            Season::Winter => Season::Spring,
        }
    }
}

#[derive(Resource)]
pub struct Calendar {
    pub season:          Season,
    pub day:             u32,
    pub ticks_this_day:  u32,
    pub ticks_per_day:   u32,
    pub days_per_season: u32,
}

impl Default for Calendar {
    fn default() -> Self {
        Self {
            season:          Season::Spring,
            day:             1,
            ticks_this_day:  0,
            ticks_per_day:   TICKS_PER_DAY,
            days_per_season: DAYS_PER_SEASON,
        }
    }
}

impl Calendar {
    pub fn food_yield_multiplier(&self) -> f32 {
        match self.season {
            Season::Spring => 0.7,
            Season::Summer => 1.3,
            Season::Autumn => 1.0,
            Season::Winter => 0.15,
        }
    }

    pub fn total_days(&self) -> u32 {
        let season_idx = self.season as u32;
        season_idx * self.days_per_season + self.day
    }
}

pub fn advance_calendar_system(mut calendar: ResMut<Calendar>) {
    calendar.ticks_this_day += 1;
    if calendar.ticks_this_day >= calendar.ticks_per_day {
        calendar.ticks_this_day = 0;
        calendar.day += 1;
        if calendar.day > calendar.days_per_season {
            calendar.day = 1;
            calendar.season = calendar.season.next();
        }
    }
}
