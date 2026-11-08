//! Android's vibration effects and composition primitives as hapticd
//! patterns. Values are the frozen `android.hardware.vibrator` V4 enums.
//!
//! Durations suit the short linear motors of phones: a click is felt from
//! about 10 ms. A pattern's strengths are relative; hapticd maps them to the
//! motor, or plays every nonzero step at full strength where it cannot.

use droidloom_device_protocol::{MAX_STEPS, Step};

/// `Effect` values this service plays.
pub mod effect {
    #![allow(missing_docs)]
    pub const CLICK: i32 = 0;
    pub const DOUBLE_CLICK: i32 = 1;
    pub const TICK: i32 = 2;
    pub const THUD: i32 = 3;
    pub const POP: i32 = 4;
    pub const HEAVY_CLICK: i32 = 5;
    pub const TEXTURE_TICK: i32 = 21;
}

/// `CompositePrimitive` values this service plays.
pub mod primitive {
    #![allow(missing_docs)]
    pub const NOOP: i32 = 0;
    pub const CLICK: i32 = 1;
    pub const THUD: i32 = 2;
    pub const SPIN: i32 = 3;
    pub const QUICK_RISE: i32 = 4;
    pub const SLOW_RISE: i32 = 5;
    pub const QUICK_FALL: i32 = 6;
    pub const LIGHT_TICK: i32 = 7;
    pub const LOW_TICK: i32 = 8;
}

/// Effects [`effect_steps`] plays.
pub const EFFECTS: [i32; 7] = [
    effect::CLICK,
    effect::DOUBLE_CLICK,
    effect::TICK,
    effect::THUD,
    effect::POP,
    effect::HEAVY_CLICK,
    effect::TEXTURE_TICK,
];

/// Primitives [`compose`] plays.
pub const PRIMITIVES: [i32; 9] = [
    primitive::NOOP,
    primitive::CLICK,
    primitive::THUD,
    primitive::SPIN,
    primitive::QUICK_RISE,
    primitive::SLOW_RISE,
    primitive::QUICK_FALL,
    primitive::LIGHT_TICK,
    primitive::LOW_TICK,
];

/// Most primitives in one composition: each takes at most four steps.
pub const COMPOSITION_SIZE_MAX: usize = MAX_STEPS / 4;
/// Longest pause before a primitive.
pub const COMPOSITION_DELAY_MAX_MS: i32 = 1000;

const fn step(ms: u32, intensity: f32) -> Step {
    Step { ms, intensity }
}

/// The pattern of `effect` at `strength` (`EffectStrength`: light 0,
/// medium 1, strong 2).
pub fn effect_steps(effect: i32, strength: i8) -> Option<Vec<Step>> {
    let scale = match strength {
        0 => 0.5,
        1 => 0.75,
        2 => 1.0,
        _ => return None,
    };
    let steps: &[Step] = match effect {
        effect::CLICK => const { &[step(14, 0.8)] },
        effect::DOUBLE_CLICK => const { &[step(14, 0.8), step(80, 0.0), step(14, 0.8)] },
        effect::TICK => const { &[step(8, 0.5)] },
        effect::THUD => const { &[step(30, 1.0)] },
        effect::POP => const { &[step(10, 0.6)] },
        effect::HEAVY_CLICK => const { &[step(20, 1.0)] },
        effect::TEXTURE_TICK => const { &[step(5, 0.35)] },
        _ => return None,
    };
    Some(scaled(steps, scale))
}

fn primitive_steps(primitive: i32) -> Option<&'static [Step]> {
    Some(match primitive {
        primitive::NOOP => &[],
        primitive::CLICK => const { &[step(14, 0.9)] },
        primitive::THUD => const { &[step(30, 1.0)] },
        primitive::SPIN => const { &[step(25, 0.5), step(25, 0.8), step(25, 0.5)] },
        primitive::QUICK_RISE => const { &[step(15, 0.3), step(15, 0.6), step(15, 1.0)] },
        primitive::SLOW_RISE => const { &[step(50, 0.2), step(50, 0.5), step(50, 0.8)] },
        primitive::QUICK_FALL => const { &[step(15, 1.0), step(15, 0.6), step(15, 0.3)] },
        primitive::LIGHT_TICK => const { &[step(6, 0.45)] },
        primitive::LOW_TICK => const { &[step(10, 0.6)] },
        _ => return None,
    })
}

/// How long `primitive` plays, in milliseconds.
pub fn primitive_ms(primitive: i32) -> Option<u32> {
    primitive_steps(primitive).map(total_ms)
}

/// Why a composition does not play.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refused {
    /// A primitive this service does not play.
    Unsupported,
    /// Too many primitives, a pause out of range, or a scale outside 0 to 1.
    OutOfRange,
}

/// The pattern of a composition: each `(delay_ms, primitive, scale)` waits,
/// then plays the primitive at `scale` of its strength.
///
/// # Errors
/// [`Refused`] when a primitive or a bound is not met.
pub fn compose(composite: &[(i32, i32, f32)]) -> Result<Vec<Step>, Refused> {
    if composite.is_empty() || composite.len() > COMPOSITION_SIZE_MAX {
        return Err(Refused::OutOfRange);
    }
    let mut steps: Vec<Step> = Vec::with_capacity(MAX_STEPS);
    for &(delay, primitive, scale) in composite {
        if !(0..=COMPOSITION_DELAY_MAX_MS).contains(&delay) || !(0.0..=1.0).contains(&scale) {
            return Err(Refused::OutOfRange);
        }
        let shape = primitive_steps(primitive).ok_or(Refused::Unsupported)?;
        if delay > 0 {
            push(&mut steps, step(delay.unsigned_abs(), 0.0));
        }
        for &part in &scaled(shape, scale) {
            push(&mut steps, part);
        }
    }
    // Trailing pauses play nothing.
    while steps.last().is_some_and(|last| last.intensity == 0.0) {
        steps.pop();
    }
    if steps.is_empty() {
        return Err(Refused::OutOfRange);
    }
    Ok(steps)
}

/// Adds `next`, joining it to a pause before it.
fn push(steps: &mut Vec<Step>, next: Step) {
    if next.intensity == 0.0
        && let Some(last) = steps.last_mut()
        && last.intensity == 0.0
    {
        last.ms += next.ms;
        return;
    }
    steps.push(next);
}

fn scaled(steps: &[Step], scale: f32) -> Vec<Step> {
    steps
        .iter()
        .map(|part| step(part.ms, (part.intensity * scale).clamp(0.0, 1.0)))
        .collect()
}

/// How long `steps` play, in milliseconds.
pub fn total_ms(steps: &[Step]) -> u32 {
    steps.iter().map(|part| part.ms).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidloom_device_protocol::valid_pattern;

    #[test]
    fn every_effect_plays_at_every_strength() {
        for effect in EFFECTS {
            for strength in 0..=2 {
                let steps = effect_steps(effect, strength).unwrap();
                assert!(valid_pattern(&steps), "{effect} {strength}");
            }
        }
        assert!(effect_steps(6, 1).is_none());
        assert!(effect_steps(effect::CLICK, 3).is_none());
    }

    #[test]
    fn a_full_composition_fits_hapticd() {
        let composite = vec![(COMPOSITION_DELAY_MAX_MS, primitive::SLOW_RISE, 1.0); COMPOSITION_SIZE_MAX];
        let steps = compose(&composite).unwrap();
        assert!(valid_pattern(&steps));
        assert_eq!(steps.len(), COMPOSITION_SIZE_MAX * 4);
    }

    #[test]
    fn pauses_join_and_the_end_is_trimmed() {
        let steps = compose(&[
            (0, primitive::CLICK, 1.0),
            (20, primitive::NOOP, 1.0),
            (30, primitive::LOW_TICK, 0.5),
            (40, primitive::NOOP, 1.0),
        ])
        .unwrap();
        assert_eq!(
            steps,
            vec![step(14, 0.9), step(50, 0.0), step(10, 0.3)]
        );
        assert_eq!(total_ms(&steps), 74);
    }

    #[test]
    fn bad_compositions_are_refused() {
        assert_eq!(compose(&[]), Err(Refused::OutOfRange));
        assert_eq!(compose(&[(0, 99, 1.0)]), Err(Refused::Unsupported));
        assert_eq!(compose(&[(-1, primitive::CLICK, 1.0)]), Err(Refused::OutOfRange));
        assert_eq!(compose(&[(0, primitive::CLICK, 1.5)]), Err(Refused::OutOfRange));
        assert_eq!(compose(&[(10, primitive::NOOP, 1.0)]), Err(Refused::OutOfRange));
    }
}
