//! The arithmetic of levels, apart from Discord.

pub use plugin_kit::grouped;

/// The experience that takes a member from `level` to the next one.
#[must_use]
pub const fn needed(level: u32) -> u64 {
    let level = level as u64;
    5 * level * level + 50 * level + 100
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
pub fn progress(total: u64) -> Progress {
    let (mut level, mut left) = (0_u32, total);
    loop {
        let needed = needed(level);
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
        assert_eq!(needed(0), 100);
        assert_eq!(needed(1), 155);
        assert_eq!(needed(10), 1100);
        assert!((0..50).all(|l| needed(l + 1) > needed(l)));
    }

    #[test]
    fn a_total_is_a_level_and_what_is_left_towards_the_next() {
        assert_eq!(
            progress(0),
            Progress {
                level: 0,
                into: 0,
                needed: 100
            }
        );
        assert_eq!(
            progress(99),
            Progress {
                level: 0,
                into: 99,
                needed: 100
            }
        );
        assert_eq!(
            progress(100),
            Progress {
                level: 1,
                into: 0,
                needed: 155
            }
        );
        assert_eq!(
            progress(100 + 155 + 7),
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
