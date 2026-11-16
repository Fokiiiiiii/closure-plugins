//! X, as the `FxTwitter` service gives its posts: how their links are found, and how a post is read.
//!
//! A post is shown with what it answers or quotes, see `thread`. Its pictures and video are not in the
//! embed: the caller attaches them as files.

use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use plugin_kit::grouped;
use regex::Regex;
use serde::Deserialize;

use crate::Config;
use crate::links::{self, PostLink};
use crate::model::{Author, Post};
use crate::provider::{Provider, is_at_hosts};

const X_COLOR: u32 = 0x1D_9BF0;
/// X's limit on the pictures of one post.
const MAX_PHOTOS: usize = 20;
const MEDIA_HOSTS: &[&str] = &["twimg.com"];

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static POST_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:www\.|mobile\.)?(?:x|twitter)\.com/(?P<user>[A-Za-z0-9_]{1,15})/status/(?P<id>\d{1,20})(?P<close>>)?",
    )
    .expect("a fixed pattern")
});

/// X: the links to its posts, and its service.
#[derive(Debug, Clone, Copy)]
pub struct XProvider;

impl Provider for XProvider {
    fn enabled(&self, config: &Config) -> bool {
        config.x_links
    }

    fn detect(&self, text: &str) -> Vec<PostLink> {
        post_links(text)
    }

    fn api_url(&self, id: u64) -> String {
        format!("https://api.fxtwitter.com/status/{id}")
    }

    fn parse(&self, bytes: &[u8]) -> Result<Post> {
        parse_tweet(bytes)
    }

    fn media_hosts(&self) -> &'static [&'static str] {
        MEDIA_HOSTS
    }
}

#[derive(Debug, Deserialize)]
struct FxResponse {
    tweet: Option<Tweet>,
}

/// A post on X, as the service answers it.
#[derive(Debug, Clone, Deserialize)]
struct Tweet {
    url: String,
    #[serde(default)]
    text: String,
    author: TweetAuthor,
    #[serde(default)]
    media: Media,
    /// The counts, when the service gave them: a quoted post inside another may come without.
    #[serde(default)]
    likes: Option<u64>,
    #[serde(default)]
    retweets: Option<u64>,
    #[serde(default)]
    replies: Option<u64>,
    created_timestamp: Option<i64>,
    #[serde(default)]
    quote: Option<Box<Tweet>>,
    #[serde(default)]
    replying_to_status: Option<String>,
    /// Who reposted it, when the link is to a repost.
    #[serde(default)]
    reposted_by: Option<TweetAuthor>,
    /// Whether the service marks the post for adults.
    #[serde(default)]
    possibly_sensitive: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct TweetAuthor {
    name: String,
    screen_name: String,
    avatar_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Media {
    #[serde(default)]
    photos: Vec<Picture>,
    #[serde(default)]
    videos: Vec<Picture>,
}

#[derive(Debug, Clone, Deserialize)]
struct Picture {
    url: String,
}

/// # Errors
/// The answer is not JSON, or has no post in it (the post is gone, or is private).
pub fn parse_tweet(bytes: &[u8]) -> Result<Post> {
    let response: FxResponse = serde_json::from_slice(bytes).context("reading the post")?;
    let tweet = response.tweet.context("the answer has no post")?;
    Ok(tweet.into_post())
}

/// The id in a link to a post, which is not 0 when there is one.
fn status_id(url: &str) -> Option<u64> {
    url.split('?')
        .next()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
        .filter(|id| *id > 0)
}

impl Tweet {
    fn into_post(self) -> Post {
        let id = status_id(&self.url).unwrap_or_default();
        let images = self
            .media
            .photos
            .iter()
            .map(|photo| photo.url.clone())
            .filter(|url| is_media(url))
            .take(MAX_PHOTOS)
            .collect();
        let video = self
            .media
            .videos
            .first()
            .map(|video| video.url.clone())
            .filter(|url| is_media(url));
        let counts = counts(&self);
        Post {
            id,
            url: self.url,
            author: author(&self.author),
            text: self.text,
            images,
            video,
            counts,
            created: self.created_timestamp,
            sensitive: self.possibly_sensitive == Some(true),
            color: X_COLOR,
            reply_to: self
                .replying_to_status
                .as_deref()
                .and_then(|id| id.parse().ok())
                .filter(|id| *id > 0),
            quote: self
                .quote
                .map(|quote| (*quote).into_post())
                .filter(|quote| quote.id > 0)
                .map(Box::new),
            reposted_by: self.reposted_by.as_ref().map(author),
            media_referer: None,
        }
    }
}

fn author(author: &TweetAuthor) -> Author {
    Author {
        name: author.name.clone(),
        handle: author.screen_name.clone(),
        avatar: author.avatar_url.clone(),
        profile: format!("https://x.com/{}", author.screen_name),
    }
}

/// The counts under a post, such as `💬 32`, `🔁 1,628` and `❤ 12,468`; the ones the service did not
/// give are left out.
fn counts(tweet: &Tweet) -> Vec<String> {
    [
        ("💬", tweet.replies),
        ("🔁", tweet.retweets),
        ("❤", tweet.likes),
    ]
    .into_iter()
    .filter_map(|(mark, count)| Some(format!("{mark} {}", grouped(count?))))
    .collect()
}

/// Whether `url` is a file at the media servers of X. The service names the files of a post, and
/// what it names is what the bot fetches or links, so a name that points anywhere else is not
/// taken: not another host, not an address with a user name in it, not a plain `http` one.
#[must_use]
pub fn is_media(url: &str) -> bool {
    is_at_hosts(url, MEDIA_HOSTS)
}

/// The links to posts on X in `text`, each once, in order, at most `MAX_PER_MESSAGE`.
#[must_use]
pub fn post_links(text: &str) -> Vec<PostLink> {
    links::scan(&POST_LINK, text, |captures| {
        let id = captures["id"].parse::<u64>().ok().filter(|id| *id > 0)?;
        Some(PostLink {
            user: captures["user"].to_owned(),
            id,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{Labels, REPOST_COLOR, Thread, pieces};

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
        assert_eq!(tweet.reply_to, Some(19));
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
        assert_eq!(tweet.reply_to, None);
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
    fn at_most_twenty_pictures_are_taken() {
        let photos: Vec<String> = (0..25)
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
        assert_eq!(shown[0].photos.len(), 20);
        assert!(shown[0].video.is_none());
    }

    #[test]
    fn an_answer_without_a_post_is_an_error() {
        assert!(parse_tweet(br#"{"code":404,"message":"NOT_FOUND"}"#).is_err());
        assert!(parse_tweet(b"not json").is_err());
    }

    #[test]
    fn a_post_marked_for_adults_is_marked_so_and_an_unmarked_one_is_not() {
        let marked = parse_tweet(
            br#"{"tweet":{"url":"https://x.com/a/status/1","possibly_sensitive":true,
            "author":{"name":"a","screen_name":"a"}}}"#,
        )
        .unwrap();
        assert!(marked.sensitive);
        let unmarked = parse_tweet(POST.as_bytes()).unwrap();
        assert!(!unmarked.sensitive);
    }
}
