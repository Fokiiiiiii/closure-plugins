//! What the plugins of closure share: a message of the server, quoted as an embed.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod net;
pub mod safe_fetch;
pub mod translate;

use twilight_model::channel::message::Embed;
use twilight_model::util::Timestamp;
use twilight_util::builder::embed::{
    EmbedAuthorBuilder, EmbedBuilder, EmbedFieldBuilder, ImageSource,
};

const QUOTE_COLOR: u32 = 0x58_65F2;

/// Cuts `text` to `limit` characters, with `…` where it was cut.
#[must_use]
pub fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[must_use]
pub fn image(url: &str) -> Option<ImageSource> {
    ImageSource::url(url).ok()
}

/// A number with its thousands told apart: `12468` is `12,468`.
#[must_use]
pub fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A message of the server, as it is quoted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quoted {
    pub author: String,
    pub avatar: Option<String>,
    pub content: String,
    /// Unix seconds.
    pub timestamp: Option<i64>,
    /// The first picture attached.
    pub image: Option<String>,
    /// The link to the message.
    pub jump: String,
    /// Where it was said, such as `#general`.
    pub place: String,
}

/// The embed of a quoted message. `source` names the field that links back to the message.
#[must_use]
pub fn quote_embed(quoted: &Quoted, limit: usize, source: &str) -> Embed {
    let mut author = EmbedAuthorBuilder::new(truncate(&quoted.author, 250));
    if let Some(icon) = quoted.avatar.as_deref().and_then(image) {
        author = author.icon_url(icon);
    }
    let mut embed =
        EmbedBuilder::new()
            .color(QUOTE_COLOR)
            .author(author)
            .field(EmbedFieldBuilder::new(
                source,
                format!("[{}]({})", quoted.place, quoted.jump),
            ));
    if !quoted.content.trim().is_empty() {
        embed = embed.description(truncate(&quoted.content, limit));
    }
    if let Some(picture) = quoted.image.as_deref().and_then(image) {
        embed = embed.image(picture);
    }
    if let Some(at) = quoted.timestamp.and_then(|s| Timestamp::from_secs(s).ok()) {
        embed = embed.timestamp(at);
    }
    embed.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_text_is_cut_with_an_ellipsis_and_a_short_one_is_kept() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("あいうえお", 3), "あい…");
    }

    #[test]
    fn a_quoted_message_shows_its_author_text_picture_and_a_link_back() {
        let quoted = Quoted {
            author: "Alice".into(),
            avatar: Some("https://cdn.example/a.png".into()),
            content: "hello".into(),
            timestamp: Some(1_700_000_000),
            image: Some("https://cdn.example/p.png".into()),
            jump: "https://discord.com/channels/1/2/3".into(),
            place: "#general".into(),
        };
        let embed = quote_embed(&quoted, 100, "Source");
        assert_eq!(embed.author.unwrap().name, "Alice");
        assert_eq!(embed.description.as_deref(), Some("hello"));
        assert_eq!(embed.image.unwrap().url, "https://cdn.example/p.png");
        assert_eq!(
            embed.fields[0].value,
            "[#general](https://discord.com/channels/1/2/3)"
        );
        assert!(embed.timestamp.is_some());
    }

    #[test]
    fn a_quote_without_text_has_no_description() {
        let embed = quote_embed(&Quoted::default(), 100, "Source");
        assert!(embed.description.is_none() && embed.image.is_none());
    }
}
