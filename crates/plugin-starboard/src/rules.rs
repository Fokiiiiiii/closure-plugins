//! The rules of the starboard, apart from Discord: when a message is posted, changed or taken
//! down, how the count reads, and which emoji counts.

/// What to do with a message when its stars changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Post it.
    Post,
    /// Change the count of its post.
    Update,
    /// Take its post down: nobody stars it any more.
    Remove,
}

/// A message is posted once it has `threshold` stars. Its post stays when the stars fall below
/// that, so that it does not flicker, and goes when none are left.
#[must_use]
pub fn decide(stars: u32, threshold: u32, posted: bool) -> Step {
    match (posted, stars) {
        (false, stars) if stars >= threshold => Step::Post,
        (true, 0) => Step::Remove,
        (true, _) => Step::Update,
        (false, _) => Step::Nothing,
    }
}

/// The text over a post: the emoji, the count and the channel the message is from.
#[must_use]
pub fn count_line(emoji: &str, stars: u32, channel: u64) -> String {
    format!("{emoji} **{stars}** | <#{channel}>")
}

/// Whether a reaction is the configured emoji. `configured` is a Unicode emoji, or `<:name:id>` or
/// `<a:name:id>` for a custom one, which is told by its id.
#[must_use]
pub fn emoji_matches(configured: &str, unicode: Option<&str>, custom: Option<u64>) -> bool {
    match custom_id(configured) {
        Some(id) => custom == Some(id),
        None => unicode == Some(configured.trim()),
    }
}

/// The id in `<:name:id>`, when the text is a custom emoji.
#[must_use]
pub fn custom_id(text: &str) -> Option<u64> {
    let inner = text.trim().strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner
        .strip_prefix("a:")
        .or_else(|| inner.strip_prefix(':'))?;
    inner.rsplit_once(':')?.1.parse().ok()
}

/// The name in `<:name:id>`.
#[must_use]
pub fn custom_name(text: &str) -> Option<&str> {
    let inner = text.trim().strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner
        .strip_prefix("a:")
        .or_else(|| inner.strip_prefix(':'))?;
    inner.rsplit_once(':').map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_posted_at_the_threshold_and_not_before() {
        assert_eq!(decide(2, 3, false), Step::Nothing);
        assert_eq!(decide(3, 3, false), Step::Post);
        assert_eq!(decide(9, 3, false), Step::Post);
    }

    #[test]
    fn a_post_follows_the_count_and_goes_only_when_no_star_is_left() {
        assert_eq!(decide(5, 3, true), Step::Update);
        assert_eq!(decide(1, 3, true), Step::Update, "it does not flicker");
        assert_eq!(decide(0, 3, true), Step::Remove);
        assert_eq!(decide(0, 3, false), Step::Nothing);
    }

    #[test]
    fn the_count_line_names_the_emoji_the_count_and_the_channel() {
        assert_eq!(count_line("⭐", 4, 55), "⭐ **4** | <#55>");
    }

    #[test]
    fn a_unicode_emoji_is_told_by_its_text_and_a_custom_one_by_its_id() {
        assert!(emoji_matches("⭐", Some("⭐"), None));
        assert!(!emoji_matches("⭐", Some("🌟"), None));
        assert!(!emoji_matches("⭐", None, Some(1)));
        assert!(emoji_matches("<:star:123>", None, Some(123)));
        assert!(emoji_matches("<a:spin:123>", None, Some(123)));
        assert!(!emoji_matches("<:star:123>", None, Some(124)));
        assert!(!emoji_matches("<:star:123>", Some("⭐"), None));
    }

    #[test]
    fn the_id_and_the_name_of_a_custom_emoji_are_read_from_its_text() {
        assert_eq!(custom_id("<:star:123>"), Some(123));
        assert_eq!(custom_name("<:star:123>"), Some("star"));
        assert_eq!(custom_name("<a:spin:9>"), Some("spin"));
        assert_eq!(custom_id("⭐"), None);
        assert_eq!(custom_id("<:star:x>"), None);
    }
}
