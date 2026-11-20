//! The arithmetic of levels, apart from Discord.

pub use plugin_kit::grouped;

/// How experience turns into levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    /// Each level takes `5 * level^2 + 50 * level + 100`.
    #[default]
    Standard,
    /// The total for a level is `100 * level^2`: each level takes `100 * (2 * level + 1)`.
    Square,
}

/// The experience that takes a member from `level` to the next one.
#[must_use]
pub const fn needed(level: u32, curve: Curve) -> u64 {
    let level = level as u64;
    match curve {
        Curve::Standard => 5 * level * level + 50 * level + 100,
        Curve::Square => 100 * (2 * level + 1),
    }
}

/// Where a total of experience puts a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub level: u32,
    /// Experience gathered towards the next level.
    pub into: u64,
    /// Experience the next level takes in all.
    pub needed: u64,
}

#[must_use]
pub fn progress(total: u64, curve: Curve) -> Progress {
    let (mut level, mut left) = (0_u32, total);
    loop {
        let needed = needed(level, curve);
        if left < needed {
            return Progress {
                level,
                into: left,
                needed,
            };
        }
        left -= needed;
        level += 1;
    }
}

/// A bar of `width` cells, filled as far as `into` is of `needed`.
#[must_use]
pub fn bar(into: u64, needed: u64, width: usize) -> String {
    let filled = if needed == 0 {
        0
    } else {
        usize::try_from(u128::from(into) * width as u128 / u128::from(needed)).unwrap_or(width)
    }
    .min(width);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(width - filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_level_takes_more_than_the_one_before() {
        assert_eq!(needed(0, Curve::Standard), 100);
        assert_eq!(needed(1, Curve::Standard), 155);
        assert_eq!(needed(10, Curve::Standard), 1100);
        assert!((0..50).all(|l| needed(l + 1, Curve::Standard) > needed(l, Curve::Standard)));
        assert!((0..50).all(|l| needed(l + 1, Curve::Square) > needed(l, Curve::Square)));
    }

    #[test]
    fn the_square_curve_puts_a_level_at_a_hundred_times_its_square() {
        for level in 0..60_u32 {
            let at = 100 * u64::from(level) * u64::from(level);
            assert_eq!(progress(at, Curve::Square).level, level);
            if level > 0 {
                assert_eq!(progress(at - 1, Curve::Square).level, level - 1);
            }
        }
        assert_eq!(
            progress(1_099_137, Curve::Square),
            Progress {
                level: 104,
                into: 1_099_137 - 100 * 104 * 104,
                needed: 100 * (2 * 104 + 1)
            }
        );
    }

    #[test]
    fn a_total_is_a_level_and_what_is_left_towards_the_next() {
        assert_eq!(
            progress(0, Curve::Standard),
            Progress {
                level: 0,
                into: 0,
                needed: 100
            }
        );
        assert_eq!(
            progress(99, Curve::Standard),
            Progress {
                level: 0,
                into: 99,
                needed: 100
            }
        );
        assert_eq!(
            progress(100, Curve::Standard),
            Progress {
                level: 1,
                into: 0,
                needed: 155
            }
        );
        assert_eq!(
            progress(100 + 155 + 7, Curve::Standard),
            Progress {
                level: 2,
                into: 7,
                needed: 220
            }
        );
    }

    #[test]
    fn the_bar_fills_with_the_progress_and_never_overflows() {
        assert_eq!(bar(0, 100, 10), "▱▱▱▱▱▱▱▱▱▱");
        assert_eq!(bar(50, 100, 10), "▰▰▰▰▰▱▱▱▱▱");
        assert_eq!(bar(100, 100, 10), "▰▰▰▰▰▰▰▰▰▰");
        assert_eq!(bar(500, 100, 4), "▰▰▰▰");
        assert_eq!(bar(1, 0, 4), "▱▱▱▱");
    }

    #[test]
    fn thousands_are_told_apart() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(12468), "12,468");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }
}
