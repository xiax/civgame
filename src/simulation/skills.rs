use bevy::prelude::*;

use crate::simulation::schedule::SimClock;
use crate::world::seasons::TICKS_PER_DAY;

pub const SKILL_COUNT: usize = 8;
/// Phase 1: per-skill ceiling. Previously skills grew without bound; the
/// new EV-based profession choice path needs a normalised competence
/// score, so we clamp.
pub const SKILL_MAX: u32 = 255;
/// Phase 1: floor every skill returns to after long disuse if it never
/// reached the mastery line. Matches the legacy `Skills::default` value.
pub const SKILL_FLOOR_BASE: u32 = 5;
/// Phase 1: a skill that peaks at or above this line is considered
/// "mastered" — even after long disuse it decays only to
/// `SKILL_MASTERED_FLOOR`, not down to `SKILL_FLOOR_BASE`.
pub const SKILL_MASTERY_LINE: u32 = 80;
/// Phase 1: floor for mastered skills (peak ≥ `SKILL_MASTERY_LINE`).
pub const SKILL_MASTERED_FLOOR: u32 = 30;
/// Phase 1: floor multiplier for skills whose peak is below the mastery
/// line: `floor = max(SKILL_FLOOR_BASE, peak * SKILL_PEAK_FLOOR_FRACTION)`.
pub const SKILL_PEAK_FLOOR_FRACTION: f32 = 0.30;
/// Phase 1: half-life (in game days) for the slow exponential decay
/// toward the floor. A skill at twice the floor halves the gap every
/// 90 days.
pub const SKILL_DECAY_HALF_LIFE_DAYS: u32 = 90;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillKind {
    Farming = 0,
    Mining = 1,
    Building = 2,
    Trading = 3,
    Combat = 4,
    Crafting = 5,
    Social = 6,
    Medicine = 7,
}

impl SkillKind {
    pub fn name(self) -> &'static str {
        match self {
            SkillKind::Farming => "Farming",
            SkillKind::Mining => "Mining",
            SkillKind::Building => "Building",
            SkillKind::Trading => "Trading",
            SkillKind::Combat => "Combat",
            SkillKind::Crafting => "Crafting",
            SkillKind::Social => "Social",
            SkillKind::Medicine => "Medicine",
        }
    }
}

/// One u32 XP slot per skill, clamped to `SKILL_MAX`. Phase 1: was
/// previously `[u32; 8]` unbounded; clamping happens at `gain_xp` time.
#[derive(Component, Clone, Copy)]
pub struct Skills(pub [u32; SKILL_COUNT]);

impl Default for Skills {
    fn default() -> Self {
        Skills([SKILL_FLOOR_BASE; SKILL_COUNT])
    }
}

impl Skills {
    pub fn get(&self, kind: SkillKind) -> u32 {
        self.0[kind as usize]
    }

    /// Add `amount` XP to `kind`, clamped at `SKILL_MAX`. Peaks and
    /// use-ticks are reconciled by `skill_peaks_tracker_system`
    /// (an observer pass over `Changed<Skills>` agents) so call sites
    /// don't have to thread `&mut SkillPeaks` / `&mut SkillUseTicks`
    /// through every executor.
    pub fn gain_xp(&mut self, kind: SkillKind, amount: u32) {
        let i = kind as usize;
        self.0[i] = self.0[i].saturating_add(amount).min(SKILL_MAX);
    }
}

/// Phase 1: per-agent record of the highest XP ever held in each skill.
/// The decay system uses `peak[i]` to compute the floor: mastered
/// skills (peak ≥ `SKILL_MASTERY_LINE`) sink only to
/// `SKILL_MASTERED_FLOOR`; lower-peak skills sink to
/// `max(SKILL_FLOOR_BASE, peak * 0.30)`. Initialised to today's
/// default skill value at spawn.
#[derive(Component, Clone, Copy)]
pub struct SkillPeaks(pub [u32; SKILL_COUNT]);

impl Default for SkillPeaks {
    fn default() -> Self {
        SkillPeaks([SKILL_FLOOR_BASE; SKILL_COUNT])
    }
}

/// Phase 1: per-agent tick at which each skill last earned XP. Updated
/// by `skill_peaks_tracker_system` when it observes an increase in
/// `Skills`. Decay only fires for skills whose last-use is older than
/// one game day.
#[derive(Component, Clone, Copy)]
pub struct SkillUseTicks(pub [u32; SKILL_COUNT]);

impl Default for SkillUseTicks {
    fn default() -> Self {
        SkillUseTicks([0; SKILL_COUNT])
    }
}

/// Phase 1: per-agent snapshot of the previous tick's `Skills` values.
/// Drives the observer pattern: when `skills.0[i] > last_seen.0[i]`,
/// the tracker stamps `use_ticks[i] = now` and bumps `peaks[i]` if
/// needed. This lets every existing `gain_xp` call site keep its
/// current signature while peaks/use-ticks update centrally.
#[derive(Component, Clone, Copy)]
pub struct SkillsLastSeen(pub [u32; SKILL_COUNT]);

impl Default for SkillsLastSeen {
    fn default() -> Self {
        SkillsLastSeen([SKILL_FLOOR_BASE; SKILL_COUNT])
    }
}

/// Phase 1 observer: ratchets `SkillPeaks` upward and stamps
/// `SkillUseTicks` whenever a `Skills` slot increases. Decreases (from
/// `skill_decay_system`) leave peaks and use-ticks alone, so
/// well-practised skills retain their mastery floor even after the
/// agent stops using them.
pub fn skill_peaks_tracker_system(
    clock: Res<SimClock>,
    mut q: Query<
        (
            &Skills,
            &mut SkillPeaks,
            &mut SkillUseTicks,
            &mut SkillsLastSeen,
        ),
        Changed<Skills>,
    >,
) {
    let now = clock.tick as u32;
    for (skills, mut peaks, mut use_ticks, mut last_seen) in q.iter_mut() {
        for i in 0..SKILL_COUNT {
            let cur = skills.0[i];
            if cur > last_seen.0[i] {
                use_ticks.0[i] = now;
                if cur > peaks.0[i] {
                    peaks.0[i] = cur;
                }
            }
            last_seen.0[i] = cur;
        }
    }
}

/// Peak-derived floor for a skill — the value the decay system pulls
/// toward. Exposed publicly so the inspector / UI can surface the same
/// number the decay system applies. Mastered skills (peak ≥
/// `SKILL_MASTERY_LINE`) sink only to `SKILL_MASTERED_FLOOR`; lower-peak
/// skills sink to `max(SKILL_FLOOR_BASE, peak × SKILL_PEAK_FLOOR_FRACTION)`.
pub fn skill_floor(peak: u32) -> u32 {
    if peak >= SKILL_MASTERY_LINE {
        SKILL_MASTERED_FLOOR
    } else {
        let proportional = (peak as f32 * SKILL_PEAK_FLOOR_FRACTION) as u32;
        SKILL_FLOOR_BASE.max(proportional)
    }
}

/// Phase 1: half-life decay toward a peak-derived floor. Runs once per
/// game day. For each skill whose last-use is at least one game day old:
/// `floor = peak ≥ MASTERY_LINE ? MASTERED_FLOOR : max(FLOOR_BASE, peak * 0.30)`
/// `skill ← floor + (skill - floor) * 0.5^(1 / HALF_LIFE_DAYS)`.
pub fn skill_decay_system(
    clock: Res<SimClock>,
    mut q: Query<(&mut Skills, &SkillPeaks, &SkillUseTicks)>,
) {
    if clock.tick % (TICKS_PER_DAY as u64) != 0 {
        return;
    }
    let now = clock.tick as u32;
    let decay_factor = 0.5_f32.powf(1.0 / SKILL_DECAY_HALF_LIFE_DAYS as f32);
    for (mut skills, peaks, use_ticks) in q.iter_mut() {
        for i in 0..SKILL_COUNT {
            let last = use_ticks.0[i];
            if now.saturating_sub(last) < TICKS_PER_DAY as u32 {
                continue;
            }
            let peak = peaks.0[i];
            let floor = skill_floor(peak);
            let s = skills.0[i];
            if s <= floor {
                continue;
            }
            let new = floor as f32 + (s as f32 - floor as f32) * decay_factor;
            // Round toward floor so we converge in finite steps even
            // for skills only one unit above the floor.
            let new_u = (new.floor() as u32).max(floor);
            if new_u < s {
                skills.0[i] = new_u;
            }
        }
    }
}
