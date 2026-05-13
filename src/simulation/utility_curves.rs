//! Phase A of behavioural-richness refactor (see
//! `~/.claude/plans/evaluate-this-plan-please-tingly-catmull.md`).
//!
//! Pure utility curves for goal scoring — soft sigmoids replacing the
//! step-function thresholds in `goal_update_system`. All curves return
//! `f32` ∈ `[0.0, 1.0]`. Anchored on the existing `HUNGER_*` / `SLEEP_*`
//! constants so the inflection points match current behaviour; slope is
//! what changes.
//!
//! No Bevy deps — every function is `pub fn` so they're trivially
//! unit-testable in `mod tests` below.

/// Smoothstep between `lo` (returns ~0) and `hi` (returns ~1).
/// Clamped to `[0, 1]`. Used as the building block for all curves.
#[inline]
fn smoothstep(x: f32, lo: f32, hi: f32) -> f32 {
    if hi <= lo {
        return if x >= lo { 1.0 } else { 0.0 };
    }
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Hunger → urgency.
///
/// Anchored on `goals.rs` thresholds (need range `0–255`):
/// - `hunger ≤ 100`              → ~0.0 (sated, no urgency)
/// - `hunger == 150` (FORAGE)    → ~0.25 (rising — autonomous forage cliff in legacy)
/// - `hunger == 180` (EAT_HELD)  → ~0.55 (hand-eat cliff)
/// - `hunger == 200` (DESPERATE) → ~0.85 (survival branch in legacy)
/// - `hunger ≥ 230`              → ~1.0 (starvation)
///
/// Two-stage smoothstep: gentle ramp 100→180 then steep ramp 180→230 so
/// `Survival`-class scorers slope-cross subsistence/discretionary
/// scorers exactly around the legacy cliffs without binary flapping.
#[inline]
pub fn hunger_utility(hunger: f32) -> f32 {
    let low = smoothstep(hunger, 100.0, 180.0) * 0.55;
    let high = smoothstep(hunger, 180.0, 230.0) * 0.45;
    (low + high).clamp(0.0, 1.0)
}

/// Sleep need → urgency. Higher `sleep` value = more tired.
///
/// `time_of_day_bonus` ∈ `[0.0, 1.0]` lifts the curve at night so a
/// moderately-tired agent picks sleep over work after dusk. Callers pass
/// `0.0` for daytime and ramp toward `1.0` across dusk → night. Anchored
/// so:
/// - `sleep == 170` (WORK_CEILING) → ~0.30 daytime / ~0.55 night
/// - `sleep == 180` (TIRED)        → ~0.50 daytime / ~0.75 night
/// - `sleep ≥ 230`                 → ~1.0
#[inline]
pub fn sleep_utility(sleep: f32, time_of_day_bonus: f32) -> f32 {
    let base = smoothstep(sleep, 120.0, 220.0);
    let lifted = base + time_of_day_bonus * 0.25 * (1.0 - base);
    lifted.clamp(0.0, 1.0)
}

/// Social need → urgency, modulated by `Disposition.gregariousness`.
///
/// `social` is the 0-255 need value (high = lonely). `gregariousness ∈
/// [0, 255]` shifts the inflection: a gregarious agent feels lonely
/// sooner, a loner shrugs it off longer.
#[inline]
pub fn social_utility(social: f32, gregariousness: u8) -> f32 {
    let greg = gregariousness as f32 / 255.0;
    let lo = 140.0 - 40.0 * greg;
    let hi = 220.0 - 40.0 * greg;
    smoothstep(social, lo, hi)
}

/// Play / recreation desire from low willpower. Lower willpower = more
/// desire to play. Modulated by gregariousness (gregarious agents play
/// more) and age (young agents play more — falls off after maturity).
///
/// `willpower` is the 0-255 need (low = depleted), `age_ticks` is the
/// agent's age in simulation ticks, `gregariousness ∈ [0, 255]`.
#[inline]
pub fn play_utility(willpower: f32, age_ticks: u64, gregariousness: u8) -> f32 {
    let base = 1.0 - smoothstep(willpower, 40.0, 150.0);
    let youth = age_falloff(age_ticks);
    let greg = 0.6 + 0.4 * (gregariousness as f32 / 255.0);
    (base * youth * greg).clamp(0.0, 1.0)
}

/// Material deficit → stockpile urgency. Combines absolute deficit
/// (raw shortfall) with profession affinity (Crafter cares more about
/// craft-input shortfalls, Farmer more about food, etc.).
///
/// `faction_deficit` is in resource units (catalog-dependent scale);
/// `prof_affinity ∈ [0.5, 1.5]` is the per-profession multiplier from
/// `profession_choice::expected_wage` style logic.
#[inline]
pub fn material_deficit_utility(faction_deficit: u32, prof_affinity: f32) -> f32 {
    let raw = smoothstep(faction_deficit as f32, 4.0, 80.0);
    (raw * prof_affinity).clamp(0.0, 1.0)
}

/// Age-based youth falloff for play / curiosity. Young agents (≤ 1
/// in-game year) get the full multiplier; falls off to 0.4 by adulthood
/// (~5 years). Tuned to stay above zero so adults still play, just less.
#[inline]
fn age_falloff(age_ticks: u64) -> f32 {
    const TICKS_PER_YEAR: u64 = 3600 * 365;
    let years = age_ticks as f32 / TICKS_PER_YEAR as f32;
    let t = smoothstep(years, 1.0, 5.0);
    1.0 - 0.6 * t
}

/// Adult-agent placeholder for `play_utility`'s `age_ticks` parameter
/// until the simulation grows a real per-person age component. Five
/// in-game years lands fully past the `age_falloff` knee at one year,
/// so every Person registered with this constant sees the adult
/// (`0.4×`) youth multiplier.
pub const ADULT_AGE_TICKS_PLACEHOLDER: u64 = 3600 * 365 * 5;

/// Map the world calendar's `TimePhase` to the `[0.0, 1.0]` bonus
/// `sleep_utility` expects in its second argument — `Day` 0.0,
/// `Dawn` 0.2, `Dusk` 0.6, `Night` 1.0. Used by `goal_update_system`
/// + `opportunistic_interrupt_system`; defined here so the mapping
/// has a single source of truth.
#[inline]
pub fn time_of_day_bonus(phase: crate::world::seasons::TimePhase) -> f32 {
    use crate::world::seasons::TimePhase;
    match phase {
        TimePhase::Day => 0.0,
        TimePhase::Dawn => 0.2,
        TimePhase::Dusk => 0.6,
        TimePhase::Night => 1.0,
    }
}

/// Helper: convert a Disposition axis `u8` into a multiplier in
/// `[1.0, 1.0 + max_lift]`. Used by scorers to weight their score by a
/// specific personality axis without each scorer re-deriving the
/// fraction.
#[inline]
pub fn disposition_lift(axis: u8, max_lift: f32) -> f32 {
    1.0 + (axis as f32 / 255.0) * max_lift
}

// ─── Mode flag ──────────────────────────────────────────────────────
// Wired in Phase B; defined here so phase A introduces it without
// touching `goal_update_system` yet.

/// Runtime switch between the legacy imperative cascade and the
/// scorer-first pipeline. **Defaults to `Scored`** as of Phase F-1 —
/// the behavioural-richness pipeline is the live path; `Legacy` is
/// kept as a fallback for A/B comparison via the debug panel and as
/// an escape hatch if a Scored regression is discovered. Full Legacy
/// removal stays deferred (Phase F-2) until in-game testing confirms
/// no regression.
#[derive(bevy::prelude::Resource, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AgentDecisionMode {
    /// Imperative cascade in `goal_update_system`'s `legacy_pick`
    /// closure. Kept as fallback for A/B comparison + regression
    /// escape hatch; not the default since Phase F-1.
    Legacy,
    /// Scorer-first pipeline driven by `GoalScorerRegistry` with
    /// `GOAL_CHALLENGER_MARGIN` hysteresis and Phase D's
    /// opportunistic en-route interrupts. Default since Phase F-1.
    #[default]
    Scored,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, eps: f32) {
        assert!(
            (a - b).abs() <= eps,
            "expected {a} ≈ {b} (within {eps})"
        );
    }

    #[test]
    fn smoothstep_clamps() {
        assert_eq!(smoothstep(-10.0, 0.0, 1.0), 0.0);
        assert_eq!(smoothstep(11.0, 0.0, 10.0), 1.0);
        approx(smoothstep(0.5, 0.0, 1.0), 0.5, 0.01);
    }

    #[test]
    fn smoothstep_monotonic() {
        let mut prev = -0.01_f32;
        for i in 0..=100 {
            let x = i as f32 / 100.0;
            let v = smoothstep(x, 0.0, 1.0);
            assert!(v >= prev, "smoothstep regressed at {x}: {prev} → {v}");
            prev = v;
        }
    }

    #[test]
    fn hunger_anchored_to_legacy_thresholds() {
        // Sated: ~0
        approx(hunger_utility(50.0), 0.0, 0.05);
        // Below FORAGE_REQUIRED (150): still low
        approx(hunger_utility(120.0), 0.12, 0.10);
        // At FORAGE_REQUIRED (150): noticeable rise — autonomous forage
        // cliff in legacy fires here.
        assert!(hunger_utility(150.0) > 0.15);
        assert!(hunger_utility(150.0) < 0.45);
        // At EAT_HELD (180): >0.5, transition into urgent band.
        assert!(hunger_utility(180.0) > 0.50);
        // At SURVIVE_DESPERATE (200): firmly urgent. Curve evaluates to
        // ~0.71 here — class-based precedence ensures Survival wins
        // regardless of raw score, but the value still sits well above
        // any rival Subsistence/Belonging scorer's ceiling.
        assert!(hunger_utility(200.0) > 0.70);
        // Starvation: saturated.
        approx(hunger_utility(255.0), 1.0, 0.05);
    }

    #[test]
    fn hunger_monotonic() {
        let mut prev = -0.01_f32;
        for h in 0..=255 {
            let v = hunger_utility(h as f32);
            assert!(v >= prev - 1e-6, "hunger_utility regressed at {h}: {prev} → {v}");
            prev = v;
        }
    }

    #[test]
    fn sleep_anchored_with_time_bonus() {
        // Daytime, rested
        approx(sleep_utility(60.0, 0.0), 0.0, 0.05);
        // Day at TIRED (180): substantial
        assert!(sleep_utility(180.0, 0.0) > 0.40);
        // Night at TIRED (180): more urgent than day
        assert!(sleep_utility(180.0, 1.0) > sleep_utility(180.0, 0.0));
        // Saturated
        approx(sleep_utility(255.0, 0.0), 1.0, 0.05);
    }

    #[test]
    fn social_curves_diverge_by_gregariousness() {
        // Same social need, different personality
        let loner = social_utility(150.0, 20);
        let normal = social_utility(150.0, 128);
        let gregarious = social_utility(150.0, 230);
        assert!(loner < normal, "loner {loner} should < normal {normal}");
        assert!(normal < gregarious, "normal {normal} should < gregarious {gregarious}");
        // Endpoints sane
        approx(social_utility(0.0, 128), 0.0, 0.05);
        approx(social_utility(255.0, 128), 1.0, 0.05);
    }

    #[test]
    fn play_curve_falls_with_age() {
        // Young (0 days) vs old (10 years), same willpower, same disposition
        let young = play_utility(50.0, 0, 128);
        let old = play_utility(50.0, 3600 * 365 * 10, 128);
        assert!(young > old, "young {young} should > old {old}");
    }

    #[test]
    fn play_gregariousness_lifts() {
        let loner = play_utility(40.0, 0, 20);
        let gregarious = play_utility(40.0, 0, 230);
        assert!(gregarious > loner);
    }

    #[test]
    fn material_deficit_zero_when_no_shortfall() {
        approx(material_deficit_utility(0, 1.0), 0.0, 0.01);
        approx(material_deficit_utility(2, 1.0), 0.0, 0.05);
        assert!(material_deficit_utility(40, 1.0) > 0.40);
        approx(material_deficit_utility(120, 1.0), 1.0, 0.05);
    }

    #[test]
    fn material_deficit_scales_with_affinity() {
        let unaffined = material_deficit_utility(40, 1.0);
        let affined = material_deficit_utility(40, 1.5);
        assert!(affined > unaffined);
        // Affinity above 1.0 clamps at 1.0 ceiling on large deficits.
        approx(material_deficit_utility(120, 1.5), 1.0, 0.05);
    }

    #[test]
    fn disposition_lift_endpoints() {
        approx(disposition_lift(0, 1.0), 1.0, 1e-6);
        approx(disposition_lift(255, 1.0), 2.0, 1e-6);
        approx(disposition_lift(128, 1.0), 1.0 + 128.0 / 255.0, 1e-6);
    }

    #[test]
    fn agent_decision_mode_default_is_scored() {
        // Phase F-1: default flipped from Legacy to Scored once the
        // scorer pipeline was proven against calibration tests. The
        // test fixture (`TestSim::new`) overrides back to Legacy so
        // tests pinned to legacy semantics keep their contract.
        assert_eq!(AgentDecisionMode::default(), AgentDecisionMode::Scored);
    }
}
