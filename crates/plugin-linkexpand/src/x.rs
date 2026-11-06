//! A post on X, as the `FxTwitter` service gives it, and the embeds it becomes.
//!
//! A post is shown with what it answers or quotes: the post it replies to comes before it, the
//! post it quotes after it, each as a small embed of its own.

use anyhow::{Context as _, Result};
use plugin_kit::{grouped, image, truncate};
use serde::Deserialize;
use twilight_model::channel::message::Embed;
use twilight_model::util::Timestamp;
use twilight_util::builder::embed::{EmbedAuthorBuilder, EmbedBuilder, EmbedFooterBuilder};

const X_COLOR: u32 = 0x1D_9BF0;
/// The colour of a post that is shown for the sake of another.
const RELATED_COLOR: u32 = 0x53_5C_65;
/// Discord shows at most four pictures of one post together.
const MAX_PHOTOS: usize = 4;
/// The most characters of a post that is shown for the sake of another.
const RELATED_CHARS: usize = 500;

#[derive(Debug, Deserialize)]
struct FxResponse {
    tweet: Option<Tweet>,
}

/// A post on X.
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
    /// The post this one quotes.
    #[serde(default)]
    pub quote: Option<Box<Tweet>>,
    /// The id of the post this one replies to.
    #[serde(default)]
    pub replying_to_status: Option<String>,
    /// Who reposted it, when the link is to a repost.
    #[serde(default)]
    pub reposted_by: Option<TweetAuthor>,
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

impl Tweet {
    /// The id of the post this one replies to, when it is one.
    #[must_use]
    pub fn parent_id(&self) -> Option<u64> {
        self.replying_to_status
            .as_deref()
            .and_then(|id| id.parse().ok())
            .filter(|id| *id > 0)
    }
}

/// The words around the embeds, in the language of the server.
#[derive(Debug, Clone)]
pub struct Labels {
    /// On the post a post replies to.
    pub replied_to: String,
    /// On the post a post quotes.
    pub quoted: String,
    /// Before the name of who reposted.
    pub reposted_by: String,
}

/// What a post becomes in Discord: its embeds, the link of its video (posted as plain text, which
/// Discord plays), and its own link.
#[derive(Debug)]
pub struct Expanded {
    pub embeds: Vec<Embed>,
    pub video: Option<String>,
    pub link: String,
}

fn author_line(author: &TweetAuthor) -> EmbedAuthorBuilder {
    let mut line = EmbedAuthorBuilder::new(truncate(
        &format!("{} (@{})", author.name, author.screen_name),
        250,
    ))
    .url(format!("https://x.com/{}", author.screen_name));
    if let Some(icon) = author.avatar_url.as_deref().and_then(image) {
        line = line.icon_url(icon);
    }
    line
}

/// A post shown for the sake of another: who, what, a picture, and why it is there.
fn related(tweet: &Tweet, label: &str) -> Embed {
    let mut embed = EmbedBuilder::new()
        .color(RELATED_COLOR)
        .author(author_line(&tweet.author))
        .footer(EmbedFooterBuilder::new(label))
        .url(&tweet.url);
    if !tweet.text.trim().is_empty() {
        embed = embed.description(truncate(&tweet.text, RELATED_CHARS));
    }
    if let Some(picture) = tweet
        .media
        .photos
        .first()
        .and_then(|photo| image(&photo.url))
    {
        embed = embed.image(picture);
    }
    embed.build()
}

/// The counts under a post, such as `💬 32  🔁 1,628  ❤ 12,468`.
fn counts(tweet: &Tweet) -> String {
    format!(
        "💬 {}  🔁 {}  ❤ {}",
        grouped(tweet.replies),
        grouped(tweet.retweets),
        grouped(tweet.likes)
    )
}

/// Expands `tweet`, with the post it replies to (`parent`, fetched by the caller) before it and
/// the post it quotes after it, when asked.
#[must_use]
pub fn expand(
    tweet: &Tweet,
    parent: Option<&Tweet>,
    show_quote: bool,
    labels: &Labels,
    limit: usize,
) -> Expanded {
    let mut footer_text = counts(tweet);
    if let Some(reposter) = &tweet.reposted_by {
        footer_text = format!("{} {}  ·  {footer_text}", labels.reposted_by, reposter.name);
    }
    let mut first = EmbedBuilder::new()
        .color(X_COLOR)
        .author(author_line(&tweet.author))
        .footer(EmbedFooterBuilder::new(footer_text))
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
    let mut embeds: Vec<Embed> = parent
        .into_iter()
        .map(|parent| related(parent, &labels.replied_to))
        .collect();
    embeds.push(first.build());
    // Embeds with the same link are shown as one gallery.
    embeds.extend(photos.map(|picture| EmbedBuilder::new().url(&tweet.url).image(picture).build()));
    if show_quote && let Some(quoted) = &tweet.quote {
        embeds.push(related(quoted, &labels.quoted));
    }
    Expanded {
        embeds,
        video: tweet.media.videos.first().map(|v| v.url.clone()),
        link: tweet.url.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POST: &str = r#"{"code":200,"message":"OK","tweet":{
        "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
        "author":{"name":"jack","screen_name":"jack","avatar_url":"https://pbs.example/a.jpg"},
        "likes":12468,"retweets":1628,"replies":32,"created_timestamp":1142974214,
        "replying_to_status":"19",
        "quote":{"url":"https://x.com/dom/status/5","text":"quoted words",
                 "author":{"name":"dom","screen_name":"dom"},
                 "media":{"photos":[{"url":"https://pbs.example/q.jpg"}]}},
        "media":{"photos":[{"url":"https://pbs.example/1.jpg"},{"url":"https://pbs.example/2.jpg"}],
                 "videos":[{"url":"https://video.example/v.mp4"}]}}}"#;

    const PARENT: &str = r#"{"tweet":{"url":"https://x.com/ev/status/19","text":"the first post",
        "author":{"name":"ev","screen_name":"ev"}}}"#;

    fn labels() -> Labels {
        Labels {
            replied_to: "In reply to".into(),
            quoted: "Quoted".into(),
            reposted_by: "Reposted by".into(),
        }
    }

    #[test]
    fn a_post_becomes_a_gallery_a_video_link_and_counts() {
        let tweet = parse_tweet(POST.as_bytes()).unwrap();
        let shown = expand(&tweet, None, false, &labels(), 1000);
        assert_eq!(shown.embeds.len(), 2, "one embed per picture");
        let first = &shown.embeds[0];
        assert_eq!(first.author.as_ref().unwrap().name, "jack (@jack)");
        assert_eq!(
            first.description.as_deref(),
            Some("just setting up my twttr")
        );
        assert_eq!(first.url.as_deref(), Some("https://x.com/jack/status/20"));
        assert_eq!(shown.embeds[1].url, first.url);
        let footer = &first.footer.as_ref().unwrap().text;
        assert_eq!(footer, "💬 32  🔁 1,628  ❤ 12,468");
        assert_eq!(shown.video.as_deref(), Some("https://video.example/v.mp4"));
        assert_eq!(shown.link, "https://x.com/jack/status/20");
    }

    #[test]
    fn the_post_it_replies_to_comes_first_and_the_post_it_quotes_last() {
        let tweet = parse_tweet(POST.as_bytes()).unwrap();
        assert_eq!(tweet.parent_id(), Some(19));
        let parent = parse_tweet(PARENT.as_bytes()).unwrap();
        let shown = expand(&tweet, Some(&parent), true, &labels(), 1000);
        let texts: Vec<Option<&str>> = shown
            .embeds
            .iter()
            .map(|e| e.description.as_deref())
            .collect();
        assert_eq!(
            texts,
            [
                Some("the first post"),
                Some("just setting up my twttr"),
                None,
                Some("quoted words")
            ]
        );
        assert_eq!(shown.embeds[0].footer.as_ref().unwrap().text, "In reply to");
        let last = shown.embeds.last().unwrap();
        assert_eq!(last.footer.as_ref().unwrap().text, "Quoted");
        assert_eq!(
            last.image.as_ref().unwrap().url,
            "https://pbs.example/q.jpg"
        );
    }

    #[test]
    fn a_quote_can_be_left_out_and_a_repost_is_named() {
        let json = POST.replace(
            r#""replying_to_status":"19","#,
            r#""reposted_by":{"name":"kit","screen_name":"kit"},"#,
        );
        let tweet = parse_tweet(json.as_bytes()).unwrap();
        assert_eq!(tweet.parent_id(), None);
        let shown = expand(&tweet, None, false, &labels(), 1000);
        assert_eq!(shown.embeds.len(), 2);
        let footer = &shown.embeds[0].footer.as_ref().unwrap().text;
        assert!(footer.starts_with("Reposted by kit  ·  💬 32"), "{footer}");
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
        let shown = expand(
            &parse_tweet(json.as_bytes()).unwrap(),
            None,
            true,
            &labels(),
            100,
        );
        assert_eq!(shown.embeds.len(), MAX_PHOTOS);
        assert!(shown.video.is_none());
    }

    #[test]
    fn an_answer_without_a_post_is_an_error() {
        assert!(parse_tweet(br#"{"code":404,"message":"NOT_FOUND"}"#).is_err());
        assert!(parse_tweet(b"not json").is_err());
    }
}
