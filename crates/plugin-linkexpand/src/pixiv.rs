//! Pixiv: how its links are found, and how an artwork is read from the public JSON the site serves
//! for it. The JSON shape is written from the site's own format, not yet checked against a live
//! answer: an answer that does not fit it is an error, and the link is then left alone.

use std::sync::LazyLock;

use anyhow::{Result, bail};
use plugin_kit::grouped;
use regex::Regex;
use serde::Deserialize;

use crate::Config;
use crate::links::{self, PostLink};
use crate::model::{Author, Post};
use crate::provider::{Provider, is_at_hosts};

const PIXIV_COLOR: u32 = 0x00_96_FA;
/// The most pictures of one artwork that are taken, whatever its page count says.
const MAX_PAGES: usize = 20;
const MEDIA_HOSTS: &[&str] = &["pximg.net"];
/// The file servers of Pixiv refuse a picture that is not asked for from its own pages.
const REFERER: &str = "https://www.pixiv.net/";
/// Pixiv's animations are not expanded; their pages are not still pictures.
const ANIMATION: u8 = 2;

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static ARTWORK_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:www\.|mobile\.)?(?:pixiv|phixiv)\.net/(?:[A-Za-z]{2}(?:-[A-Za-z]+)?/)?artworks/(?P<id>\d{1,20})(?P<close>>)?",
    )
    .expect("a fixed pattern")
});

/// Pixiv: the links to its artworks, and its public JSON for them.
#[derive(Debug, Clone, Copy)]
pub struct PixivProvider;

impl Provider for PixivProvider {
    fn enabled(&self, config: &Config) -> bool {
        config.pixiv_links
    }

    fn detect(&self, text: &str) -> Vec<PostLink> {
        post_links(text)
    }

    fn api_url(&self, id: u64) -> String {
        format!("https://www.pixiv.net/ajax/illust/{id}?lang=ja")
    }

    fn pages_url(&self, id: u64) -> Option<String> {
        Some(format!(
            "https://www.pixiv.net/ajax/illust/{id}/pages?lang=ja"
        ))
    }

    fn parse(&self, bytes: &[u8]) -> Result<Post> {
        parse_artwork(bytes)
    }

    fn parse_pages(&self, bytes: &[u8]) -> Result<Vec<String>> {
        parse_pages(bytes)
    }

    fn media_hosts(&self) -> &'static [&'static str] {
        MEDIA_HOSTS
    }

    fn media_referer(&self, url: &str) -> Option<&'static str> {
        is_at_hosts(url, MEDIA_HOSTS).then_some(REFERER)
    }
}

/// The links to artworks in `text`, each once, in order, at most `MAX_PER_MESSAGE`.
#[must_use]
pub fn post_links(text: &str) -> Vec<PostLink> {
    links::scan(&ARTWORK_LINK, text, |captures| {
        let id = captures["id"].parse::<u64>().ok().filter(|id| *id > 0)?;
        Some(PostLink {
            user: String::new(),
            id,
        })
    })
}

/// The answer of Pixiv's JSON: `error` is set when the artwork is not there or not for this client.
#[derive(Debug, Deserialize)]
struct Answer<T> {
    error: bool,
    body: T,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Artwork {
    illust_id: String,
    illust_title: String,
    #[serde(default)]
    illust_comment: String,
    illust_type: u8,
    user_id: String,
    user_name: String,
    #[serde(default)]
    x_restrict: u8,
    like_count: Option<u64>,
    bookmark_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Page {
    urls: PageUrls,
}

#[derive(Debug, Deserialize)]
struct PageUrls {
    original: String,
}

/// # Errors
/// The answer is not JSON of the shape Pixiv gives, is an error, or is an animation.
pub fn parse_artwork(bytes: &[u8]) -> Result<Post> {
    let answer: Answer<Artwork> = serde_json::from_slice(bytes)?;
    if answer.error {
        bail!("pixiv says the artwork is not there");
    }
    let artwork = answer.body;
    if artwork.illust_type == ANIMATION {
        bail!("an animation is not expanded");
    }
    let id: u64 = artwork.illust_id.parse()?;
    let comment = plain_text(&artwork.illust_comment);
    let text = if comment.trim().is_empty() {
        artwork.illust_title
    } else {
        format!("{}\n{comment}", artwork.illust_title)
    };
    let counts = [("❤", artwork.like_count), ("🔖", artwork.bookmark_count)]
        .into_iter()
        .filter_map(|(mark, count)| Some(format!("{mark} {}", grouped(count?))))
        .collect();
    Ok(Post {
        id,
        url: format!("https://www.pixiv.net/artworks/{id}"),
        author: Author {
            name: artwork.user_name,
            handle: artwork.user_id.clone(),
            avatar: None,
            profile: format!("https://www.pixiv.net/users/{}", artwork.user_id),
        },
        text,
        images: Vec::new(),
        video: None,
        counts,
        created: None,
        sensitive: artwork.x_restrict > 0,
        color: PIXIV_COLOR,
        reply_to: None,
        quote: None,
        reposted_by: None,
        media_referer: None,
    })
}

/// The original of each page of an artwork, at most `MAX_PAGES`.
///
/// # Errors
/// The answer is not JSON of the shape Pixiv gives, or is an error.
pub fn parse_pages(bytes: &[u8]) -> Result<Vec<String>> {
    let answer: Answer<Vec<Page>> = serde_json::from_slice(bytes)?;
    if answer.error {
        bail!("pixiv says the pages are not there");
    }
    Ok(answer
        .body
        .into_iter()
        .take(MAX_PAGES)
        .map(|page| page.urls.original)
        .collect())
}

/// The text of Pixiv's description: its markup is dropped and its line breaks are kept.
#[must_use]
pub fn plain_text(html: &str) -> String {
    let html = html.replace("<br />", "\n").replace("<br>", "\n");
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
pub(crate) mod fixtures {
    pub const INFO_101: &str = r#"{"error":false,"message":"","body":{"illustId":"101","illustTitle":"Harbor Lanterns","illustComment":"<p>A night harbor.<br />Lanterns on the pier.</p>","userId":"5001","userName":"Sample Artist","createDate":"2024-05-01T12:00:00+09:00","pageCount":2,"xRestrict":0,"illustType":0,"likeCount":42,"bookmarkCount":7,"viewCount":900,"tags":{"tags":[{"tag":"harbor"}]},"urls":{"original":"https://i.pximg.net/img-original/img/2024/05/01/12/00/00/101_p0.png","regular":"https://i.pximg.net/img-master/img/2024/05/01/12/00/00/101_p0_master1200.jpg"}}}"#;
    pub const PAGES_101: &str = r#"{"error":false,"message":"","body":[{"urls":{"original":"https://i.pximg.net/img-original/img/2024/05/01/12/00/00/101_p0.png","regular":"https://i.pximg.net/img-master/img/2024/05/01/12/00/00/101_p0_master1200.jpg"},"width":1200,"height":800},{"urls":{"original":"https://i.pximg.net/img-original/img/2024/05/01/12/00/00/101_p1.png","regular":"https://i.pximg.net/img-master/img/2024/05/01/12/00/00/101_p1_master1200.jpg"},"width":1200,"height":800}]}"#;
    pub const INFO_102: &str = r#"{"error":false,"message":"","body":{"illustId":"102","illustTitle":"Night Market","illustComment":"","userId":"5002","userName":"Sample Painter","createDate":"2024-05-02T12:00:00+09:00","pageCount":1,"xRestrict":1,"illustType":0,"likeCount":3,"bookmarkCount":0,"viewCount":50,"tags":{"tags":[]},"urls":{"original":"https://i.pximg.net/img-original/img/2024/05/02/12/00/00/102_p0.png","regular":"https://i.pximg.net/img-master/img/2024/05/02/12/00/00/102_p0_master1200.jpg"}}}"#;
    pub const PAGES_102: &str = r#"{"error":false,"message":"","body":[{"urls":{"original":"https://i.pximg.net/img-original/img/2024/05/02/12/00/00/102_p0.png","regular":"https://i.pximg.net/img-master/img/2024/05/02/12/00/00/102_p0_master1200.jpg"},"width":800,"height":800}]}"#;
    pub const ERROR_103: &str = r#"{"error":true,"message":"Not found","body":[]}"#;
    pub const INFO_104: &str = r#"{"error":false,"message":"","body":{"illustId":"104","illustTitle":"Two Pictures","illustComment":"","userId":"5003","userName":"Sample Sketcher","createDate":"2024-05-03T12:00:00+09:00","pageCount":2,"xRestrict":0,"illustType":0,"likeCount":1,"bookmarkCount":0,"viewCount":10,"tags":{"tags":[]},"urls":{"original":"https://evil.example/img/104_p0.png","regular":"https://evil.example/img/104_p0_master.jpg"}}}"#;
    pub const PAGES_104: &str = r#"{"error":false,"message":"","body":[{"urls":{"original":"https://evil.example/img/104_p0.png","regular":"https://evil.example/img/104_p0_master.jpg"}},{"urls":{"original":"https://i.pximg.net/img-original/img/2024/05/03/12/00/00/104_p1.png","regular":"https://i.pximg.net/img-master/img/2024/05/03/12/00/00/104_p1_master1200.jpg"}}]}"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::provider::Provider;

    fn ids(text: &str) -> Vec<u64> {
        post_links(text).iter().map(|link| link.id).collect()
    }

    #[test]
    fn a_link_in_each_form_is_found_once_and_one_in_angle_brackets_is_not() {
        assert_eq!(
            ids(
                "https://www.pixiv.net/artworks/101 https://pixiv.net/en/artworks/102 \
                 https://phixiv.net/artworks/103 https://www.pixiv.net/artworks/101 \
                 <https://pixiv.net/artworks/104>"
            ),
            [101, 102, 103]
        );
        assert!(ids("https://www.pixiv.net/users/5001").is_empty());
    }

    #[test]
    fn the_description_keeps_its_line_breaks_and_loses_its_markup() {
        assert_eq!(plain_text("<p>A &amp; B<br />C</p>"), "A & B\nC");
    }

    #[test]
    fn an_artwork_is_read_as_a_post_with_its_author_counts_and_colour() {
        let post = PixivProvider.parse(INFO_101.as_bytes()).unwrap();
        assert_eq!(post.id, 101);
        assert_eq!(post.url, "https://www.pixiv.net/artworks/101");
        assert_eq!(post.author.name, "Sample Artist");
        assert_eq!(post.author.handle, "5001");
        assert_eq!(
            post.text,
            "Harbor Lanterns\nA night harbor.\nLanterns on the pier."
        );
        assert_eq!(post.counts, ["❤ 42", "🔖 7"]);
        assert!(!post.sensitive);
        assert_eq!(post.color, PIXIV_COLOR);
        assert!(post.images.is_empty(), "the pages give the pictures");
    }

    #[test]
    fn an_artwork_for_adults_is_marked_so() {
        assert!(PixivProvider.parse(INFO_102.as_bytes()).unwrap().sensitive);
    }

    #[test]
    fn an_error_answer_and_an_animation_are_refused() {
        assert!(PixivProvider.parse(ERROR_103.as_bytes()).is_err());
        let animation = INFO_101.replace("\"illustType\":0", "\"illustType\":2");
        assert!(PixivProvider.parse(animation.as_bytes()).is_err());
        assert!(PixivProvider.parse(b"not json").is_err());
    }

    #[test]
    fn the_pages_are_their_originals_and_at_most_twenty() {
        assert_eq!(
            PixivProvider.parse_pages(PAGES_101.as_bytes()).unwrap(),
            [
                "https://i.pximg.net/img-original/img/2024/05/01/12/00/00/101_p0.png",
                "https://i.pximg.net/img-original/img/2024/05/01/12/00/00/101_p1.png",
            ]
        );
        let many: Vec<String> = (0..25)
            .map(|n| {
                format!(r#"{{"urls":{{"original":"https://i.pximg.net/img-original/{n}.png"}}}}"#)
            })
            .collect();
        let json = format!(
            r#"{{"error":false,"message":"","body":[{}]}}"#,
            many.join(",")
        );
        assert_eq!(
            PixivProvider.parse_pages(json.as_bytes()).unwrap().len(),
            MAX_PAGES
        );
    }

    #[test]
    fn only_the_files_of_pixiv_get_its_referer() {
        assert_eq!(
            PixivProvider.media_referer("https://i.pximg.net/img-original/a.png"),
            Some("https://www.pixiv.net/")
        );
        assert_eq!(
            PixivProvider.media_referer("https://evil.example/a.png"),
            None
        );
        assert_eq!(
            PixivProvider.media_referer("http://i.pximg.net/a.png"),
            None
        );
    }
}
