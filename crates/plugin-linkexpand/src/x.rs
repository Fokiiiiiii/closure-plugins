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
#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub quote: Option<Box<Tweet>>,
    #[serde(default)]
    pub replying_to_status: Option<String>,
    /// Who reposted it, when the link is to a repost.
    #[serde(default)]
    pub reposted_by: Option<TweetAuthor>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TweetAuthor {
    pub name: String,
    pub screen_name: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Media {
    #[serde(default)]
    pub photos: Vec<Picture>,
    #[serde(default)]
    pub videos: Vec<Picture>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Picture {
    pub url: String,
}

/// # Errors
/// The answer is not JSON, or has no post in it (the post is gone, or is private).
pub fn parse_tweet(bytes: &[u8]) -> Result<Tweet> {
    let response: FxResponse = serde_json::from_slice(bytes).context("reading the post")?;
    response.tweet.context("the answer has no post")
}

impl Tweet {
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
    pub replied_to: String,
    pub quoted: String,
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

/// Whether `url` is a file at the media servers of X. The service names the files of a post, and
/// what it names is what the bot fetches or links, so a name that points anywhere else is not
/// taken: not another host, not an address with a user name in it, not a plain `http` one.
#[must_use]
pub fn is_media(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains(['@', '\\', ':']) {
        return false;
    }
    let host = authority.to_ascii_lowercase();
    host == "twimg.com"
        || host
            .strip_suffix(".twimg.com")
            .is_some_and(|name| !name.is_empty())
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
            .map(|p| p.url.clone())
            .filter(|url| is_media(url))
            .take(MAX_PHOTOS)
            .collect(),
        video: tweet
            .media
            .videos
            .first()
            .map(|v| v.url.clone())
            .filter(|url| is_media(url)),
        link: tweet.url.clone(),
        main,
    }
}

/// A post with the posts around it: every post it answers up to the first one, and the posts it
/// quotes, one inside the other.
#[derive(Debug, Clone)]
pub struct Thread {
    /// What the post answers: the first post of the conversation first, the post it answers last.
    pub ancestors: Vec<Tweet>,
    pub post: Tweet,
    /// The post it quotes, the post that one quotes, and so on.
    pub quotes: Vec<Tweet>,
}

impl Thread {
    #[must_use]
    pub fn new(post: Tweet) -> Self {
        Self {
            ancestors: Vec::new(),
            post,
            quotes: Vec::new(),
        }
    }

    /// How many posts there are, the one that was linked among them.
    #[must_use]
    pub fn size(&self) -> usize {
        1 + self.ancestors.len() + self.quotes.len()
    }

    /// Drops the posts farthest from the one that was linked until at most `room` are left.
    pub fn fit(&mut self, room: usize) {
        while self.size() > room.max(1) {
            if self.ancestors.len() > self.quotes.len() {
                self.ancestors.remove(0);
            } else {
                self.quotes.pop();
            }
        }
    }
}

/// `label`, with how far the post is from the one that was linked when there are several.
fn numbered(label: &str, distance: usize, of: usize) -> String {
    if of > 1 {
        format!("{label} {distance}")
    } else {
        label.to_owned()
    }
}

/// The messages of a thread, in the order they are read: what the post answers, the post, and what
/// it quotes.
#[must_use]
pub fn pieces(thread: &Thread, labels: &Labels, limit: usize) -> Vec<Piece> {
    let mut all = Vec::new();
    let answered = thread.ancestors.len();
    for (at, parent) in thread.ancestors.iter().enumerate() {
        let title = numbered(&labels.replied_to, answered - at, answered);
        all.push(piece(parent, Some(title), REPLIED_COLOR, limit, false));
    }
    let tweet = &thread.post;
    all.push(match &tweet.reposted_by {
        Some(who) => piece(
            tweet,
            Some(format!("{} {}", labels.reposted_by, who.name)),
            REPOST_COLOR,
            limit,
            true,
        ),
        None => piece(tweet, None, X_COLOR, limit, true),
    });
    let quoted = thread.quotes.len();
    for (at, quote) in thread.quotes.iter().enumerate() {
        let title = numbered(&labels.quoted, at + 1, quoted);
        all.push(piece(quote, Some(title), QUOTED_COLOR, limit, false));
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    const POST: &str = r#"{"code":200,"message":"OK","tweet":{
        "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
        "author":{"name":"jack","screen_name":"jack","avatar_url":"https://pbs.twimg.com/a.jpg"},
        "likes":12468,"retweets":1628,"replies":32,"created_timestamp":1142974214,
        "replying_to_status":"19",
        "quote":{"url":"https://x.com/dom/status/5","text":"quoted words",
                 "author":{"name":"dom","screen_name":"dom"},
                 "media":{"photos":[{"url":"https://pbs.twimg.com/q.jpg"}]}},
        "media":{"photos":[{"url":"https://pbs.twimg.com/1.jpg"},{"url":"https://pbs.twimg.com/2.jpg"}],
                 "videos":[{"url":"https://video.twimg.com/v.mp4"}]}}}"#;

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
        let shown = pieces(&Thread::new(tweet), &labels(), 1000);
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
            ["https://pbs.twimg.com/1.jpg", "https://pbs.twimg.com/2.jpg"]
        );
        assert_eq!(
            first.video.as_deref(),
            Some("https://video.twimg.com/v.mp4")
        );
        assert_eq!(first.link, "https://x.com/jack/status/20");
    }

    #[test]
    fn the_post_it_replies_to_comes_first_and_the_post_it_quotes_last() {
        let tweet = parse_tweet(POST.as_bytes()).unwrap();
        assert_eq!(tweet.parent_id(), Some(19));
        let parent = parse_tweet(PARENT.as_bytes()).unwrap();
        let thread = Thread {
            ancestors: vec![parent],
            quotes: tweet.quote.as_deref().cloned().into_iter().collect(),
            post: tweet,
        };
        let shown = pieces(&thread, &labels(), 1000);
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
        assert_eq!(shown[2].photos, ["https://pbs.twimg.com/q.jpg"]);
    }

    #[test]
    fn a_quote_can_be_left_out_and_a_repost_is_named() {
        let json = POST.replace(
            r#""replying_to_status":"19","#,
            r#""reposted_by":{"name":"kit","screen_name":"kit"},"#,
        );
        let tweet = parse_tweet(json.as_bytes()).unwrap();
        assert_eq!(tweet.parent_id(), None);
        let shown = pieces(&Thread::new(tweet), &labels(), 1000);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].embed.title.as_deref(), Some("Reposted by kit"));
        assert_eq!(shown[0].embed.color, Some(REPOST_COLOR));
        assert_eq!(
            shown[0].embed.footer.as_ref().unwrap().text,
            "💬 32  🔁 1,628  ❤ 12,468"
        );
    }

    #[test]
    fn a_long_thread_is_numbered_from_the_post_that_was_linked_and_fits_by_its_far_ends() {
        let post = |id: u32| {
            parse_tweet(
                format!(
                    r#"{{"tweet":{{"url":"https://x.com/a/status/{id}","text":"p{id}",
                    "author":{{"name":"a","screen_name":"a"}}}}}}"#
                )
                .as_bytes(),
            )
            .unwrap()
        };
        let mut thread = Thread {
            ancestors: vec![post(1), post(2), post(3)],
            post: post(4),
            quotes: vec![post(5), post(6)],
        };
        let titles = |thread: &Thread| -> Vec<Option<String>> {
            pieces(thread, &labels(), 100)
                .into_iter()
                .map(|p| p.embed.title)
                .collect()
        };
        assert_eq!(
            titles(&thread),
            [
                Some("In reply to 3".to_owned()),
                Some("In reply to 2".to_owned()),
                Some("In reply to 1".to_owned()),
                None,
                Some("Quoted 1".to_owned()),
                Some("Quoted 2".to_owned()),
            ]
        );
        thread.fit(4);
        assert_eq!(thread.size(), 4);
        assert_eq!(thread.ancestors.len(), 2);
        assert_eq!(thread.quotes.len(), 1);
        thread.fit(1);
        assert_eq!(thread.size(), 1);
    }

    #[test]
    fn only_files_at_the_media_servers_of_x_are_taken() {
        for good in [
            "https://pbs.twimg.com/media/a.jpg",
            "https://video.twimg.com/amplify_video/1/vid/avc1/1920x1080/a.mp4?tag=29",
            "https://TWIMG.com/a",
        ] {
            assert!(is_media(good), "{good}");
        }
        for bad in [
            "http://pbs.twimg.com/a.jpg",
            "https://twimg.com.evil.example/a.jpg",
            "https://evil.example/pbs.twimg.com/a.jpg",
            "https://pbs.twimg.com@evil.example/a.jpg",
            "https://pbs.twimg.com:8080/a.jpg",
            "https://127.0.0.1/a.jpg",
            "https://nottwimg.com/a.jpg",
            "https://pbs.twimg.com.example/a",
            "",
        ] {
            assert!(!is_media(bad), "{bad}");
        }
    }

    #[test]
    fn at_most_four_pictures_are_taken() {
        let photos: Vec<String> = (0..6)
            .map(|n| format!(r#"{{"url":"https://pbs.twimg.com/{n}.jpg"}}"#))
            .collect();
        let json = format!(
            r#"{{"tweet":{{"url":"https://x.com/a/status/1","author":{{"name":"a","screen_name":"a"}},
            "media":{{"photos":[{}]}}}}}}"#,
            photos.join(",")
        );
        let shown = pieces(
            &Thread::new(parse_tweet(json.as_bytes()).unwrap()),
            &labels(),
            100,
        );
        assert_eq!(shown[0].photos.len(), MAX_PHOTOS);
        assert!(shown[0].video.is_none());
    }

    #[test]
    fn an_answer_without_a_post_is_an_error() {
        assert!(parse_tweet(br#"{"code":404,"message":"NOT_FOUND"}"#).is_err());
        assert!(parse_tweet(b"not json").is_err());
    }
}
