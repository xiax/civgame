use bevy::prelude::*;

pub const TICKS_PER_DAY: u32 = 3600;
pub const DAYS_PER_SEASON: u32 = 5; // ← change this to adjust timescale
pub const TICKS_PER_SEASON: u32 = TICKS_PER_DAY * DAYS_PER_SEASON;

// Day-cycle phase cuts as a fraction of the day (`ticks_this_day / ticks_per_day`).
// 0.0 is sunrise (start-of-day). The day rolls Dawn → Day → Dusk → Night and
// loops back. Tuned so Day occupies the largest band, Dusk gets a meaningful
// "light is fading" window, and short-lived sims (which start at tick 0) sit
// firmly in Dawn/Day rather than Night — preserving daytime ranking for the
// tens of behavioural tests that didn't previously care about time of day.
pub const PHASE_DAWN_START: f32 = 0.00;
pub const PHASE_DAY_START: f32 = 0.05;
pub const PHASE_DUSK_START: f32 = 0.65;
pub const PHASE_NIGHT_START: f32 = 0.85;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TimePhase {
    #[default]
    Day,
    Dawn,
    Dusk,
    Night,
}

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
    pub season: Season,
    pub day: u32,
    pub ticks_this_day: u32,
    pub ticks_per_day: u32,
    pub days_per_season: u32,
}

impl Default for Calendar {
    fn default() -> Self {
        Self {
            season: Season::Spring,
            day: 1,
            ticks_this_day: 0,
            ticks_per_day: TICKS_PER_DAY,
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

    /// Fraction of the day elapsed, in `[0.0, 1.0)`.
    pub fn day_fraction(&self) -> f32 {
        (self.ticks_this_day as f32 / self.ticks_per_day.max(1) as f32).clamp(0.0, 1.0)
    }

    /// Bucketed time-of-day phase. See `PHASE_*_START` constants for the cuts.
    pub fn time_phase(&self) -> TimePhase {
        let f = self.day_fraction();
        if f < PHASE_DAWN_START {
            TimePhase::Night
        } else if f < PHASE_DAY_START {
            TimePhase::Dawn
        } else if f < PHASE_DUSK_START {
            TimePhase::Day
        } else if f < PHASE_NIGHT_START {
            TimePhase::Dusk
        } else {
            TimePhase::Night
        }
    }

    /// Within the dusk band, fraction of dusk daylight remaining (`1.0` at the
    /// start of dusk, `0.0` at the dusk → night flip). Returns `1.0` outside
    /// dusk so callers can pass it unconditionally.
    pub fn dusk_fraction_remaining(&self) -> f32 {
        let f = self.day_fraction();
        if f < PHASE_DUSK_START || f >= PHASE_NIGHT_START {
            return 1.0;
        }
        let span = PHASE_NIGHT_START - PHASE_DUSK_START;
        if span <= 0.0 {
            return 1.0;
        }
        ((PHASE_NIGHT_START - f) / span).clamp(0.0, 1.0)
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
