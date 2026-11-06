//! A post on X, as the `FxTwitter` service gives it, and the embeds it becomes.

use anyhow::{Context as _, Result};
use plugin_kit::{image, truncate};
use serde::Deserialize;
use twilight_model::channel::message::Embed;
use twilight_model::util::Timestamp;
use twilight_util::builder::embed::{EmbedAuthorBuilder, EmbedBuilder, EmbedFooterBuilder};

const X_COLOR: u32 = 0x1D_9BF0;
/// Discord shows at most four pictures of one post together.
const MAX_PHOTOS: usize = 4;

#[derive(Debug, Deserialize)]
struct FxResponse {
    tweet: Option<Tweet>,
}

/// A post on X, as the `FxTwitter` service gives it.
#[derive(Debug, Deserialize)]
pub struct Tweet {
    pub url: String,
    #[serde(default)]
    pub text: String,
    pub author: TweetAuthor,
    #[serde(default)]
    pub media: Media,
    #[serde(default)]
    pub likes: u64,
    #[serde(default)]
    pub retweets: u64,
    #[serde(default)]
    pub replies: u64,
    pub created_timestamp: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TweetAuthor {
    pub name: String,
    pub screen_name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Media {
    #[serde(default)]
    pub photos: Vec<Picture>,
    #[serde(default)]
    pub videos: Vec<Picture>,
}

#[derive(Debug, Deserialize)]
pub struct Picture {
    pub url: String,
}

/// Reads the answer of the `FxTwitter` service.
///
/// # Errors
/// The answer is not JSON, or has no post in it (the post is gone, or is private).
pub fn parse_tweet(bytes: &[u8]) -> Result<Tweet> {
    let response: FxResponse = serde_json::from_slice(bytes).context("reading the post")?;
    response.tweet.context("the answer has no post")
}

/// What a post becomes in Discord: its embeds, and the link of its video, which is posted as
/// plain text so that Discord plays it.
#[derive(Debug)]
pub struct Expanded {
    pub embeds: Vec<Embed>,
    pub video: Option<String>,
}

#[must_use]
pub fn tweet_embeds(tweet: &Tweet, limit: usize) -> Expanded {
    let mut author = EmbedAuthorBuilder::new(truncate(
        &format!("{} (@{})", tweet.author.name, tweet.author.screen_name),
        250,
    ))
    .url(format!("https://x.com/{}", tweet.author.screen_name));
    if let Some(icon) = tweet.author.avatar_url.as_deref().and_then(image) {
        author = author.icon_url(icon);
    }
    let footer = EmbedFooterBuilder::new(format!(
        "X  ·  💬 {}  🔁 {}  ❤ {}",
        tweet.replies, tweet.retweets, tweet.likes
    ));
    let mut first = EmbedBuilder::new()
        .color(X_COLOR)
        .author(author)
        .footer(footer)
        .url(&tweet.url);
    if !tweet.text.trim().is_empty() {
        first = first.description(truncate(&tweet.text, limit));
    }
    if let Some(at) = tweet
        .created_timestamp
        .and_then(|s| Timestamp::from_secs(s).ok())
    {
        first = first.timestamp(at);
    }
    let mut photos = tweet
        .media
        .photos
        .iter()
        .take(MAX_PHOTOS)
        .filter_map(|p| image(&p.url));
    if let Some(picture) = photos.next() {
        first = first.image(picture);
    }
    let mut embeds = vec![first.build()];
    // Embeds with the same link are shown as one gallery.
    embeds.extend(photos.map(|picture| EmbedBuilder::new().url(&tweet.url).image(picture).build()));
    Expanded {
        embeds,
        video: tweet.media.videos.first().map(|v| v.url.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const POST: &str = r#"{"code":200,"message":"OK","tweet":{
        "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
        "author":{"name":"jack","screen_name":"jack","avatar_url":"https://pbs.example/a.jpg"},
        "likes":10,"retweets":3,"replies":1,"created_timestamp":1142974214,
        "media":{"photos":[{"url":"https://pbs.example/1.jpg"},{"url":"https://pbs.example/2.jpg"}],
                 "videos":[{"url":"https://video.example/v.mp4"}]}}}"#;

    #[test]
    fn a_post_becomes_a_gallery_and_a_video_link() {
        let tweet = parse_tweet(POST.as_bytes()).unwrap();
        let shown = tweet_embeds(&tweet, 1000);
        assert_eq!(shown.embeds.len(), 2, "one embed per picture");
        let first = &shown.embeds[0];
        assert_eq!(first.author.as_ref().unwrap().name, "jack (@jack)");
        assert_eq!(
            first.description.as_deref(),
            Some("just setting up my twttr")
        );
        assert_eq!(first.url.as_deref(), Some("https://x.com/jack/status/20"));
        assert_eq!(shown.embeds[1].url, first.url);
        assert!(first.footer.as_ref().unwrap().text.contains("❤ 10"));
        assert_eq!(shown.video.as_deref(), Some("https://video.example/v.mp4"));
    }

    #[test]
    fn at_most_four_pictures_are_shown() {
        let photos: Vec<String> = (0..6)
            .map(|n| format!(r#"{{"url":"https://p.example/{n}.jpg"}}"#))
            .collect();
        let json = format!(
            r#"{{"tweet":{{"url":"https://x.com/a/status/1","author":{{"name":"a","screen_name":"a"}},
            "media":{{"photos":[{}]}}}}}}"#,
            photos.join(",")
        );
        let shown = tweet_embeds(&parse_tweet(json.as_bytes()).unwrap(), 100);
        assert_eq!(shown.embeds.len(), MAX_PHOTOS);
        assert!(shown.video.is_none());
    }

    #[test]
    fn an_answer_without_a_post_is_an_error() {
        assert!(parse_tweet(br#"{"code":404,"message":"NOT_FOUND"}"#).is_err());
        assert!(parse_tweet(b"not json").is_err());
    }
}
