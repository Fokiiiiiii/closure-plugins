//! A post on X, as the `FxTwitter` service gives it, and the messages it becomes.
//!
//! A post is shown with what it answers or quotes: the post it replies to comes before it, the
//! post it quotes after it, each as a message of its own (the caller chains them as replies). Its
//! pictures and video are not in the embed: the caller attaches them as files.

use anyhow::{Context as _, Result};
use plugin_kit::{grouped, image, truncate};
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

/// A post on X.
#[derive(Debug, Deserialize)]
pub struct Tweet {
    pub url: String,
    #[serde(default)]
    pub text: String,
    pub author: TweetAuthor,
    #[serde(default)]
    pub media: Media,
    /// The counts, when the service gave them: a quoted post inside another may come without.
    #[serde(default)]
    pub likes: Option<u64>,
    #[serde(default)]
    pub retweets: Option<u64>,
    #[serde(default)]
    pub replies: Option<u64>,
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
    /// The id of the post this one quotes.
    #[must_use]
    pub fn quote_id(&self) -> Option<u64> {
        let url = &self.quote.as_ref()?.url;
        url.split('?')
            .next()?
            .rsplit('/')
            .next()?
            .parse()
            .ok()
            .filter(|id| *id > 0)
    }

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

/// A post as one message: its embed, its pictures and video to attach, and its own link.
#[derive(Debug)]
pub struct Piece {
    pub embed: Embed,
    /// The pictures, at most four.
    pub photos: Vec<String>,
    /// The video, or the animation.
    pub video: Option<String>,
    pub link: String,
    /// Whether this is the post that was linked, and not the one it answers or quotes.
    pub main: bool,
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

/// The counts under a post, such as `💬 32  🔁 1,628  ❤ 12,468`; the ones the service did not give
/// are left out.
fn counts(tweet: &Tweet) -> Option<String> {
    let parts: Vec<String> = [
        ("💬", tweet.replies),
        ("🔁", tweet.retweets),
        ("❤", tweet.likes),
    ]
    .into_iter()
    .filter_map(|(mark, count)| Some(format!("{mark} {}", grouped(count?))))
    .collect();
    (!parts.is_empty()).then(|| parts.join("  "))
}

/// The colour of the bar of a post that is shown for the sake of another: what a reader needs to
/// tell it from the post that was linked.
const REPLIED_COLOR: u32 = 0x8E_95_9E;
const QUOTED_COLOR: u32 = 0xA3_6C_F5;
const REPOST_COLOR: u32 = 0x00_BA_7C;

/// One post as a message. `title` says what the post is to the one that was linked (the post it
/// answers, the post it quotes, or a repost) and `color` is its bar.
fn piece(tweet: &Tweet, title: Option<String>, color: u32, limit: usize, main: bool) -> Piece {
    let mut embed = EmbedBuilder::new()
        .color(color)
        .author(author_line(&tweet.author))
        .url(&tweet.url);
    if let Some(counts) = counts(tweet) {
        embed = embed.footer(EmbedFooterBuilder::new(counts));
    }
    if let Some(title) = title {
        embed = embed.title(title);
    }
    if !tweet.text.trim().is_empty() {
        embed = embed.description(truncate(&tweet.text, limit));
    }
    if let Some(at) = tweet
        .created_timestamp
        .and_then(|s| Timestamp::from_secs(s).ok())
    {
        embed = embed.timestamp(at);
    }
    Piece {
        embed: embed.build(),
        photos: tweet
            .media
            .photos
            .iter()
            .take(MAX_PHOTOS)
            .map(|p| p.url.clone())
            .collect(),
        video: tweet.media.videos.first().map(|v| v.url.clone()),
        link: tweet.url.clone(),
        main,
    }
}

/// The messages of `tweet`: the post it replies to (`parent`, fetched by the caller) first, then
/// the post itself, then the post it quotes (`quoted`: the nested one is enough, but the caller can
/// give the whole post, which has the counts).
#[must_use]
pub fn pieces(
    tweet: &Tweet,
    parent: Option<&Tweet>,
    quoted: Option<&Tweet>,
    labels: &Labels,
    limit: usize,
) -> Vec<Piece> {
    let mut all: Vec<Piece> = parent
        .into_iter()
        .map(|parent| {
            piece(
                parent,
                Some(labels.replied_to.clone()),
                REPLIED_COLOR,
                limit,
                false,
            )
        })
        .collect();
    match &tweet.reposted_by {
        Some(who) => all.push(piece(
            tweet,
            Some(format!("{} {}", labels.reposted_by, who.name)),
            REPOST_COLOR,
            limit,
            true,
        )),
        None => all.push(piece(tweet, None, X_COLOR, limit, true)),
    }
    if let Some(quoted) = quoted {
        all.push(piece(
            quoted,
            Some(labels.quoted.clone()),
            QUOTED_COLOR,
            limit,
            false,
        ));
    }
    all
}

/// The address of a post at a service that Discord shows with a player, for a video that is too
/// large to attach.
#[must_use]
pub fn player_link(link: &str) -> String {
    link.replacen("https://x.com/", "https://fixupx.com/", 1)
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
    fn a_post_is_one_message_with_its_pictures_and_video_to_attach() {
        let tweet = parse_tweet(POST.as_bytes()).unwrap();
        let shown = pieces(&tweet, None, None, &labels(), 1000);
        assert_eq!(shown.len(), 1);
        let first = &shown[0];
        assert!(first.main);
        assert_eq!(first.embed.author.as_ref().unwrap().name, "jack (@jack)");
        assert_eq!(
            first.embed.description.as_deref(),
            Some("just setting up my twttr")
        );
        assert!(
            first.embed.image.is_none(),
            "the pictures are attached, not embedded"
        );
        assert_eq!(
            first.embed.footer.as_ref().unwrap().text,
            "💬 32  🔁 1,628  ❤ 12,468"
        );
        assert_eq!(
            first.photos,
            ["https://pbs.example/1.jpg", "https://pbs.example/2.jpg"]
        );
        assert_eq!(first.video.as_deref(), Some("https://video.example/v.mp4"));
        assert_eq!(first.link, "https://x.com/jack/status/20");
    }

    #[test]
    fn the_post_it_replies_to_comes_first_and_the_post_it_quotes_last() {
        let tweet = parse_tweet(POST.as_bytes()).unwrap();
        assert_eq!(tweet.parent_id(), Some(19));
        let parent = parse_tweet(PARENT.as_bytes()).unwrap();
        let shown = pieces(
            &tweet,
            Some(&parent),
            tweet.quote.as_deref(),
            &labels(),
            1000,
        );
        let texts: Vec<Option<&str>> = shown
            .iter()
            .map(|p| p.embed.description.as_deref())
            .collect();
        assert_eq!(
            texts,
            [
                Some("the first post"),
                Some("just setting up my twttr"),
                Some("quoted words")
            ]
        );
        assert_eq!(
            shown.iter().map(|p| p.main).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert_eq!(shown[0].embed.title.as_deref(), Some("In reply to"));
        assert_eq!(shown[2].embed.title.as_deref(), Some("Quoted"));
        assert_ne!(shown[0].embed.color, shown[2].embed.color);
        assert_eq!(shown[2].photos, ["https://pbs.example/q.jpg"]);
    }

    #[test]
    fn a_quote_can_be_left_out_and_a_repost_is_named() {
        let json = POST.replace(
            r#""replying_to_status":"19","#,
            r#""reposted_by":{"name":"kit","screen_name":"kit"},"#,
        );
        let tweet = parse_tweet(json.as_bytes()).unwrap();
        assert_eq!(tweet.parent_id(), None);
        let shown = pieces(&tweet, None, None, &labels(), 1000);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].embed.title.as_deref(), Some("Reposted by kit"));
        assert_eq!(shown[0].embed.color, Some(REPOST_COLOR));
        assert_eq!(
            shown[0].embed.footer.as_ref().unwrap().text,
            "💬 32  🔁 1,628  ❤ 12,468"
        );
    }

    #[test]
    fn at_most_four_pictures_are_taken() {
        let photos: Vec<String> = (0..6)
            .map(|n| format!(r#"{{"url":"https://p.example/{n}.jpg"}}"#))
            .collect();
        let json = format!(
            r#"{{"tweet":{{"url":"https://x.com/a/status/1","author":{{"name":"a","screen_name":"a"}},
            "media":{{"photos":[{}]}}}}}}"#,
            photos.join(",")
        );
        let shown = pieces(
            &parse_tweet(json.as_bytes()).unwrap(),
            None,
            None,
            &labels(),
            100,
        );
        assert_eq!(shown[0].photos.len(), MAX_PHOTOS);
        assert!(shown[0].video.is_none());
    }

    #[test]
    fn a_video_too_large_to_attach_is_shown_through_a_player_link() {
        assert_eq!(
            player_link("https://x.com/jack/status/20"),
            "https://fixupx.com/jack/status/20"
        );
    }

    #[test]
    fn an_answer_without_a_post_is_an_error() {
        assert!(parse_tweet(br#"{"code":404,"message":"NOT_FOUND"}"#).is_err());
        assert!(parse_tweet(b"not json").is_err());
    }
}
