//! Reddit and Instagram, read through the embed fixer the operator names for each: the link's host is
//! replaced by that host, and the page it answers a crawler with is read for its `og:` tags.
//! shortcut: the shape of a fixer's answer is not yet checked against a live one; a page without
//! those tags is an error and the link is left alone.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use anyhow::{Result, bail};
use regex::{Captures, Regex};

use crate::Config;
use crate::links::{self, PostLink};
use crate::model::{Author, Post};
use crate::provider::{Provider, is_at_hosts};

/// The User-Agent of Discord's link crawler: an embed fixer answers it with the tags.
const CRAWLER_AGENT: &str = "Mozilla/5.0 (compatible; Discordbot/2.0; +https://discordapp.com)";
/// The tags that are read from a page; the others are not used.
const TAGS: &[&str] = &[
    "og:title",
    "og:description",
    "og:image",
    "og:video",
    "og:video:url",
    "og:video:secure_url",
    "og:site_name",
    "article:published_time",
    "twitter:creator",
];
/// The id of a Reddit post, or the code of an Instagram post, is this many characters.
const REDDIT_ID_LENGTH: std::ops::RangeInclusive<usize> = 5..=8;
const INSTAGRAM_CODE_LENGTH: std::ops::RangeInclusive<usize> = 5..=20;
const MAX_HOST_LEN: usize = 253;
/// The longest run after `&` that can be an entity such as `&#x1F600;`.
const MAX_ENTITY: usize = 12;

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static REDDIT_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:(?:(?:www|old|new|np|m|i)\.)?reddit\.com(?P<path>/r/[A-Za-z0-9_]+/comments/(?P<id>[A-Za-z0-9]+)(?:/[A-Za-z0-9_-]*)*)|(?:www\.)?redd\.it/(?P<short>[A-Za-z0-9]+))(?P<close>>)?",
    )
    .expect("a fixed pattern")
});

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static INSTAGRAM_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:(?:www|m)\.)?instagram\.com/(?P<kind>p|reels?|tv)/(?P<code>[A-Za-z0-9_-]+)(?P<close>>)?",
    )
    .expect("a fixed pattern")
});

/// A site that is answered from the embed fixer the operator gives for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site {
    Reddit,
    Instagram,
}

impl Site {
    /// The key of the site in the operator's `proxies`.
    fn key(self) -> &'static str {
        match self {
            Self::Reddit => "reddit",
            Self::Instagram => "instagram",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Reddit => "Reddit",
            Self::Instagram => "Instagram",
        }
    }

    fn color(self) -> u32 {
        match self {
            Self::Reddit => 0xFF_45_00,
            Self::Instagram => 0xE1_30_6C,
        }
    }

    /// The fixer used when the operator names none; a site without one is off until it is named.
    fn default_proxy(self) -> Option<&'static str> {
        match self {
            Self::Reddit => Some("rxddit.com"),
            Self::Instagram => None,
        }
    }

    /// The hosts of the site's own pictures and video, besides the fixer's.
    fn media_hosts(self) -> &'static [&'static str] {
        match self {
            Self::Reddit => &[
                "i.redd.it",
                "preview.redd.it",
                "external-preview.redd.it",
                "v.redd.it",
            ],
            Self::Instagram => &["cdninstagram.com", "fbcdn.net"],
        }
    }

    fn links(self, text: &str) -> Vec<PostLink> {
        match self {
            Self::Reddit => links::scan(&REDDIT_LINK, text, reddit_link),
            Self::Instagram => links::scan(&INSTAGRAM_LINK, text, instagram_link),
        }
    }

    /// The address of the post on the site itself, which the answer links back to.
    fn canonical(self, path: &str) -> String {
        match self {
            Self::Reddit if path.starts_with("/r/") => format!("https://www.reddit.com{path}"),
            Self::Reddit => format!("https://redd.it{path}"),
            Self::Instagram => format!("https://www.instagram.com{path}"),
        }
    }
}

fn by_path(path: String) -> PostLink {
    PostLink {
        user: String::new(),
        id: 0,
        path,
    }
}

fn reddit_link(captures: &Captures<'_>) -> Option<PostLink> {
    let id = captures
        .name("id")
        .or_else(|| captures.name("short"))?
        .as_str();
    if !REDDIT_ID_LENGTH.contains(&id.len()) {
        return None;
    }
    let path = match captures.name("path") {
        Some(path) => path.as_str().to_owned(),
        None => format!("/{id}"),
    };
    Some(by_path(path))
}

fn instagram_link(captures: &Captures<'_>) -> Option<PostLink> {
    let code = captures.name("code")?.as_str();
    if !INSTAGRAM_CODE_LENGTH.contains(&code.len()) {
        return None;
    }
    let kind = captures.name("kind")?.as_str();
    Some(by_path(format!("/{kind}/{code}/")))
}

/// A site answered from its embed fixer: the fixer serves the page that the link's path names.
#[derive(Debug, Clone)]
pub(crate) struct OpenGraphProvider {
    site: Site,
    proxy: String,
}

impl Provider for OpenGraphProvider {
    fn enabled(&self, config: &Config) -> bool {
        match self.site {
            Site::Reddit => config.reddit_links,
            Site::Instagram => config.instagram_links,
        }
    }

    fn detect(&self, text: &str) -> Vec<PostLink> {
        self.site.links(text)
    }

    fn api_url(&self, link: &PostLink) -> String {
        format!("https://{}{}", self.proxy, link.path)
    }

    fn parse(&self, bytes: &[u8]) -> Result<Post> {
        parse_page(self.site, bytes)
    }

    fn post_url(&self, link: &PostLink) -> Option<String> {
        Some(self.site.canonical(&link.path))
    }

    fn request_headers(&self, _url: &str) -> Vec<(&'static str, String)> {
        vec![("user-agent", CRAWLER_AGENT.to_owned())]
    }

    fn translatable(&self) -> bool {
        false
    }

    fn media_hosts(&self) -> &'static [&'static str] {
        self.site.media_hosts()
    }

    fn is_media(&self, url: &str) -> bool {
        let mut hosts = vec![self.proxy.as_str()];
        hosts.extend_from_slice(self.site.media_hosts());
        is_at_hosts(url, &hosts)
    }
}

/// The sites answered from an embed fixer, each with the host the operator gives it, or its default.
/// A host that is not a host name is logged without its value, and that site is left out.
#[must_use]
pub(crate) fn providers(proxies: &BTreeMap<String, String>) -> Vec<Arc<dyn Provider>> {
    [Site::Reddit, Site::Instagram]
        .into_iter()
        .filter_map(|site| {
            let proxy = proxy_of(site, proxies)?;
            let provider: Arc<dyn Provider> = Arc::new(OpenGraphProvider { site, proxy });
            Some(provider)
        })
        .collect()
}

fn proxy_of(site: Site, proxies: &BTreeMap<String, String>) -> Option<String> {
    let host = proxies
        .get(site.key())
        .map(String::as_str)
        .or(site.default_proxy())?;
    if is_host_name(host) {
        Some(host.to_owned())
    } else {
        tracing::warn!(
            site = site.key(),
            "the host of the embed fixer is not a host name; the site is not expanded"
        );
        None
    }
}

/// A host name such as `rxddit.com`: lowercase letters, digits, dots and hyphens, at least one dot,
/// and not an IP address. A port, a path, a user name or an upper-case letter is not one.
#[must_use]
fn is_host_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let allowed = host.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
    });
    let well_formed = labels
        .iter()
        .all(|label| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-'));
    let address = labels
        .iter()
        .all(|label| label.bytes().all(|byte| byte.is_ascii_digit()));
    host.len() <= MAX_HOST_LEN && allowed && well_formed && labels.len() >= 2 && !address
}

/// The post of a page: its title and description as the text, its picture and video, and the author
/// it names. The address is set from the link by [`Provider::post_url`]. Every post is marked for
/// adults, since a page does not say.
fn parse_page(site: Site, bytes: &[u8]) -> Result<Post> {
    let html = String::from_utf8_lossy(bytes);
    let tags = meta_tags(&html);
    let title = tag(&tags, "og:title");
    let description = tag(&tags, "og:description");
    if title.is_none() && description.is_none() {
        bail!("the page has no post in it");
    }
    let text = match (title, description) {
        (Some(title), Some(description)) if !description.trim().is_empty() => {
            format!("{title}\n{description}")
        }
        (Some(title), _) => title.to_owned(),
        (None, description) => description.unwrap_or_default().to_owned(),
    };
    let creator = tag(&tags, "twitter:creator").map(|creator| creator.trim_start_matches('@'));
    let name = creator
        .filter(|creator| !creator.is_empty())
        .or_else(|| tag(&tags, "og:site_name"))
        .unwrap_or(site.name());
    let video = ["og:video:url", "og:video:secure_url", "og:video"]
        .into_iter()
        .find_map(|key| tag(&tags, key));
    Ok(Post {
        id: 0,
        url: String::new(),
        author: Author {
            name: name.to_owned(),
            handle: name.to_owned(),
            avatar: None,
            profile: String::new(),
        },
        text,
        images: tag(&tags, "og:image")
            .map(|image| vec![image.to_owned()])
            .unwrap_or_default(),
        video: video.map(str::to_owned),
        counts: Vec::new(),
        created: tag(&tags, "article:published_time").and_then(iso_seconds),
        sensitive: true,
        color: site.color(),
        reply_to: None,
        quote: None,
        reposted_by: None,
        media_referer: None,
    })
}

/// The content of the first tag of `tags` with `key`.
fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(name, _)| name.as_str() == key)
        .map(|(_, content)| content.as_str())
}

/// The `og:`-style tags of `html` that are read, as (key, content), in the order of the page.
fn meta_tags(html: &str) -> Vec<(String, String)> {
    let bytes = html.as_bytes();
    let lower = html.to_ascii_lowercase();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = lower.get(from..).and_then(|rest| rest.find("<meta")) {
        let body = from + at + "<meta".len();
        let Some(end) = tag_end(bytes, body) else {
            break;
        };
        let starts_tag = bytes
            .get(body)
            .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'/');
        if starts_tag && let Some(pair) = html.get(body..end).and_then(meta_pair) {
            found.push(pair);
        }
        from = end + 1;
    }
    found
}

/// The index of the `>` that ends the tag whose attributes start at `from`; a `>` in quotes does not.
fn tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote = None;
    for (at, &byte) in bytes.iter().enumerate().skip(from) {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return Some(at),
            None => {}
        }
    }
    None
}

/// The (key, content) of a `<meta>` tag whose key is one that is read.
fn meta_pair(body: &str) -> Option<(String, String)> {
    let attrs = attributes(body);
    let value = |wanted: &str| {
        attrs
            .iter()
            .find(|(name, _)| name.as_str() == wanted)
            .map(|(_, value)| value.as_str())
    };
    let key = value("property")
        .or_else(|| value("name"))?
        .trim()
        .to_ascii_lowercase();
    if !TAGS.contains(&key.as_str()) {
        return None;
    }
    Some((key, value("content")?.to_owned()))
}

/// The attributes of a tag as (lower-case name, decoded value). Quotes may be `"` or `'`, or none.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let bytes = tag.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    loop {
        at = advance(bytes, at, |byte| byte.is_ascii_whitespace() || byte == b'/');
        if at >= bytes.len() {
            break;
        }
        let name_start = at;
        at = advance(bytes, at, |byte| {
            !(byte.is_ascii_whitespace() || byte == b'=' || byte == b'/')
        });
        let name = tag
            .get(name_start..at)
            .unwrap_or_default()
            .to_ascii_lowercase();
        at = advance(bytes, at, |byte| byte.is_ascii_whitespace());
        if bytes.get(at) != Some(&b'=') {
            continue;
        }
        at = advance(bytes, at + 1, |byte| byte.is_ascii_whitespace());
        let value = match bytes.get(at) {
            Some(&quote @ (b'"' | b'\'')) => {
                let start = at + 1;
                at = advance(bytes, start, |byte| byte != quote);
                let value = tag.get(start..at).unwrap_or_default();
                at = (at + 1).min(bytes.len());
                value
            }
            _ => {
                let start = at;
                at = advance(bytes, start, |byte| !byte.is_ascii_whitespace());
                tag.get(start..at).unwrap_or_default()
            }
        };
        found.push((name, decode(value)));
    }
    found
}

/// The index of the first byte from `from` that `keep` does not hold for, or the end.
fn advance(bytes: &[u8], from: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut at = from;
    while bytes.get(at).is_some_and(|&byte| keep(byte)) {
        at += 1;
    }
    at
}

/// The text with its character references decoded; one that is not known is kept as it is.
fn decode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        rest = rest.get(at..).unwrap_or_default();
        match entity(rest) {
            Some((decoded, used)) => {
                out.push(decoded);
                rest = rest.get(used..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = rest.get(1..).unwrap_or_default();
            }
        }
    }
    out.push_str(rest);
    out
}

/// The character that the reference at the start of `text` (which begins with `&`) stands for, and
/// how many bytes it takes.
fn entity(text: &str) -> Option<(char, usize)> {
    let window = text.as_bytes().get(..text.len().min(MAX_ENTITY))?;
    let end = window.iter().position(|&byte| byte == b';')?;
    let name = text.get(1..end)?;
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let number = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => name.strip_prefix('#')?.parse::<u32>().ok()?,
            };
            char::from_u32(number)?
        }
    };
    Some((decoded, end + 1))
}

/// Unix seconds of an ISO 8601 time such as `2024-05-01T12:00:00+09:00`. A fraction of a second is
/// dropped, and a time without a zone is UTC.
fn iso_seconds(text: &str) -> Option<i64> {
    let (date, time) = text.trim().split_once('T')?;
    let mut parts = date.split('-').map(|part| part.parse::<i64>().ok());
    let year = parts.next()??;
    let month = parts.next()??;
    let day = parts.next()??;
    let zone_at = time.find(['Z', '+', '-']).unwrap_or(time.len());
    let clock = time.get(..zone_at)?.split('.').next()?;
    let mut fields = clock.split(':').map(|part| part.parse::<i64>().ok());
    let hour = fields.next()??;
    let minute = fields.next()??;
    let second = fields.next()??;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let offset = zone_offset(time.get(zone_at..)?)?;
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// The seconds a zone designator is ahead of UTC: empty and `Z` are UTC, `+09:00` and `-05:00` are not.
fn zone_offset(zone: &str) -> Option<i64> {
    if zone.is_empty() || zone == "Z" {
        return Some(0);
    }
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let (hours, minutes) = zone.get(1..)?.split_once(':')?;
    let hours = hours.parse::<i64>().ok()?;
    let minutes = minutes.parse::<i64>().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;

    fn found(site: Site, text: &str) -> Vec<String> {
        site.links(text).into_iter().map(|link| link.path).collect()
    }

    fn provider(site: Site, proxy: &str) -> OpenGraphProvider {
        OpenGraphProvider {
            site,
            proxy: proxy.to_owned(),
        }
    }

    const PAGE: &str = r#"<html><head>
        <meta name="description" content="ignored">
        <meta content="A harbor at night &amp; the pier" property='og:description'>
        <meta property="og:title" content="Harbor &#x2F; Lanterns">
        <meta property="og:image" content="https://i.redd.it/a.jpg">
        <meta property="og:site_name" content="Reddit">
        <meta name="twitter:creator" content="@sample_artist">
        <meta property="article:published_time" content="2024-05-01T12:00:00+09:00">
        <meta property="og:video:url" content="https://v.redd.it/b/DASH.mp4">
        </head></html>"#;

    #[test]
    fn reddit_posts_are_found_in_each_form_with_their_paths_kept() {
        assert_eq!(
            found(
                Site::Reddit,
                "https://www.reddit.com/r/rust/comments/abc123/a_title/ https://old.reddit.com/r/rust/comments/abc123 https://redd.it/abc123",
            ),
            [
                "/r/rust/comments/abc123/a_title/",
                "/r/rust/comments/abc123",
                "/abc123",
            ]
        );
        assert_eq!(
            found(
                Site::Reddit,
                "https://reddit.com/r/rust/comments/abc1234/?utm=1"
            ),
            ["/r/rust/comments/abc1234/"]
        );
    }

    #[test]
    fn a_reddit_id_of_the_wrong_length_and_other_pages_are_not_posts() {
        assert!(found(
            Site::Reddit,
            "https://reddit.com/r/rust/comments/abc1 https://reddit.com/r/rust/comments/abc123456 https://reddit.com/r/rust https://reddit.com/user/someone https://notreddit.com/r/rust/comments/abc123 https://reddit.com.example/r/rust/comments/abc123 https://redd.it/abc1",
        )
        .is_empty());
    }

    #[test]
    fn instagram_posts_reels_and_tv_are_found_and_other_pages_are_not() {
        assert_eq!(
            found(
                Site::Instagram,
                "https://www.instagram.com/p/CODE12/ https://instagram.com/reel/CODE12?igsh=x https://m.instagram.com/tv/CODE12",
            ),
            ["/p/CODE12/", "/reel/CODE12/", "/tv/CODE12/"]
        );
        assert!(found(
            Site::Instagram,
            "https://www.instagram.com/s/CODE12 https://www.instagram.com/someone/ https://instagram.com/p/abc",
        )
        .is_empty());
    }

    #[test]
    fn a_link_in_angle_brackets_is_not_expanded() {
        assert!(found(Site::Reddit, "<https://reddit.com/r/rust/comments/abc123/>").is_empty());
        assert!(found(Site::Instagram, "<https://instagram.com/p/CODE12>").is_empty());
    }

    #[test]
    fn the_same_link_is_taken_once_and_at_most_three_of_a_site() {
        assert_eq!(
            found(
                Site::Reddit,
                "https://reddit.com/r/a/comments/abc123 https://reddit.com/r/a/comments/abc123 https://reddit.com/r/a/comments/abc124 https://reddit.com/r/a/comments/abc125 https://reddit.com/r/a/comments/abc126",
            )
            .len(),
            3
        );
    }

    #[test]
    fn the_fixer_host_replaces_the_link_host_and_the_crawler_agent_is_sent() {
        let provider = provider(Site::Reddit, "rxddit.com");
        let link = by_path("/r/rust/comments/abc123/a_title/".to_owned());
        assert_eq!(
            provider.api_url(&link),
            "https://rxddit.com/r/rust/comments/abc123/a_title/"
        );
        assert_eq!(
            provider.post_url(&link).as_deref(),
            Some("https://www.reddit.com/r/rust/comments/abc123/a_title/")
        );
        assert_eq!(
            provider.request_headers("https://rxddit.com/r/rust/comments/abc123/"),
            [("user-agent", CRAWLER_AGENT.to_owned())]
        );
        assert!(!provider.translatable());
    }

    #[test]
    fn only_the_fixer_and_the_sites_own_media_hosts_are_taken() {
        let reddit = provider(Site::Reddit, "rxddit.com");
        assert!(reddit.is_media("https://rxddit.com/a.jpg"));
        assert!(reddit.is_media("https://i.redd.it/a.jpg"));
        assert!(!reddit.is_media("https://evilrxddit.com/a.jpg"));
        assert!(!reddit.is_media("https://i.redd.it.evil.example/a.jpg"));
        assert!(!reddit.is_media("http://i.redd.it/a.jpg"));
        let instagram = provider(Site::Instagram, "ddinstagram.example.org");
        assert!(instagram.is_media("https://scontent.cdninstagram.com/a.jpg"));
        assert!(instagram.is_media("https://scontent.xx.fbcdn.net/a.jpg"));
        assert!(!instagram.is_media("https://evilcdninstagram.com/a.jpg"));
        assert!(!instagram.is_media("https://fbcdn.net.evil.example/a.jpg"));
    }

    #[test]
    fn a_host_that_is_not_a_plain_host_name_is_refused() {
        for bad in [
            "",
            "localhost",
            "Rxddit.com",
            "rxddit.com:8080",
            "user@rxddit.com",
            "rxddit.com/x",
            "127.0.0.1",
            "a..b.com",
            "-a.com",
            "rx ddit.com",
            "[::1]",
        ] {
            assert!(!is_host_name(bad), "{bad}");
        }
        assert!(is_host_name("rxddit.com"));
        assert!(is_host_name("ddinstagram.example.org"));
    }

    #[test]
    fn reddit_has_its_default_fixer_and_instagram_only_when_one_is_given() {
        let none = providers(&BTreeMap::new());
        assert_eq!(none.len(), 1);
        assert!(
            none[0]
                .api_url(&by_path("/abc12".to_owned()))
                .starts_with("https://rxddit.com/")
        );
        let mut given = BTreeMap::new();
        given.insert("instagram".to_owned(), "ddinstagram.example.org".to_owned());
        assert_eq!(providers(&given).len(), 2);
        given.insert("reddit".to_owned(), "Bad Host".to_owned());
        assert_eq!(providers(&given).len(), 1);
    }

    #[test]
    fn a_page_is_read_for_its_tags_in_any_order_and_quoting_with_entities_decoded() {
        let post = parse_page(Site::Reddit, PAGE.as_bytes()).unwrap();
        assert_eq!(post.text, "Harbor / Lanterns\nA harbor at night & the pier");
        assert_eq!(post.images, ["https://i.redd.it/a.jpg"]);
        assert_eq!(post.video.as_deref(), Some("https://v.redd.it/b/DASH.mp4"));
        assert_eq!(post.author.name, "sample_artist");
        assert_eq!(post.created, Some(1_714_532_400));
        assert!(post.sensitive);
        assert_eq!(post.color, 0xFF_45_00);
        assert!(post.counts.is_empty());
    }

    #[test]
    fn a_page_without_its_tags_is_an_error_and_a_broken_one_never_panics() {
        assert!(parse_page(Site::Reddit, b"").is_err());
        for junk in [
            "<meta",
            "<meta content=",
            "<meta property=\"og:title\" content='",
            "&&&;;",
            "<meta/",
            "<meta a b c d=\"",
            "é<meta é>",
        ] {
            assert!(parse_page(Site::Reddit, junk.as_bytes()).is_err(), "{junk}");
        }
        assert!(parse_page(Site::Reddit, &[0xff, 0xfe, b'<', b'm']).is_err());
    }

    #[test]
    fn many_ampersands_are_read_without_quadratic_work() {
        let html = format!(
            "{}<meta property=\"og:title\" content=\"t\">",
            "&".repeat(200_000)
        );
        assert!(parse_page(Site::Reddit, html.as_bytes()).is_ok());
    }

    #[test]
    fn multibyte_characters_next_to_references_are_kept_whole() {
        let html = "<meta property=\"og:title\" content=\"日本語&amp;é&#x1F600;&tail\">";
        let post = parse_page(Site::Instagram, html.as_bytes()).unwrap();
        assert_eq!(post.text, "日本語&é😀&tail");
    }

    #[test]
    fn references_are_decoded_and_unknown_ones_are_kept() {
        assert_eq!(
            decode("a&amp;b &lt;c&gt; &quot;d&quot; &#39;e&#x27; &nope"),
            "a&b <c> \"d\" 'e' &nope"
        );
    }

    #[test]
    fn an_iso_time_is_unix_seconds_with_its_zone() {
        assert_eq!(
            iso_seconds("2024-05-01T12:00:00+09:00"),
            Some(1_714_532_400)
        );
        assert_eq!(iso_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_seconds("2024-05-01T03:00:00.250Z"), Some(1_714_532_400));
        assert_eq!(iso_seconds("2024-13-01T00:00:00Z"), None);
        assert_eq!(iso_seconds("yesterday"), None);
    }
}
