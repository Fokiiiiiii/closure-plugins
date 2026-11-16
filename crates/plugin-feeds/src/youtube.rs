//! `YouTube`: the videos of a channel, read from the feed the service publishes for it.

use anyhow::{Context as _, Result, bail};
use roxmltree::{Document, Node, ParsingOptions};

use crate::source::Item;

/// A feed is a few kilobytes; more than this is not one.
pub const MAX_FEED_BYTES: usize = 1024 * 1024;
const ATOM: &str = "http://www.w3.org/2005/Atom";
const YOUTUBE: &str = "http://www.youtube.com/xml/schemas/2015";
const CHANNEL_LINK: &str = "https://www.youtube.com/channel/";
const CHANNEL_ID_TAIL_LEN: usize = 22;

/// The channel id in `text`: the id itself, or the id in a link to the channel.
#[must_use]
pub fn channel_id(text: &str) -> Option<String> {
    let text = text.trim();
    let id = match text.strip_prefix(CHANNEL_LINK) {
        Some(rest) => rest.split(['/', '?', '#']).next().unwrap_or_default(),
        None => text,
    };
    is_channel_id(id).then(|| id.to_owned())
}

fn is_channel_id(id: &str) -> bool {
    id.strip_prefix("UC").is_some_and(|tail| {
        tail.len() == CHANNEL_ID_TAIL_LEN
            && tail
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// The address of the feed of a channel; the id has been checked by [`channel_id`].
#[must_use]
pub fn feed_url(channel: &str) -> String {
    format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel}")
}

/// The videos of an Atom feed, in the order the feed gives them (newest first). An entry without
/// a video id or a link is left out.
///
/// # Errors
/// The body is not XML, is not an Atom feed, declares a DTD, or is larger than [`MAX_FEED_BYTES`].
pub fn parse_feed(body: &[u8]) -> Result<Vec<Item>> {
    if body.len() > MAX_FEED_BYTES {
        bail!("the feed is larger than the limit");
    }
    let text = std::str::from_utf8(body).context("the feed is not UTF-8")?;
    let options = ParsingOptions {
        allow_dtd: false,
        ..ParsingOptions::default()
    };
    let document = Document::parse_with_options(text, options).context("the feed is not XML")?;
    let root = document.root_element();
    if !root.has_tag_name((ATOM, "feed")) {
        bail!("the feed is not an Atom feed");
    }
    Ok(root
        .children()
        .filter(|node| node.has_tag_name((ATOM, "entry")))
        .filter_map(video)
        .collect())
}

fn video(entry: Node) -> Option<Item> {
    let id = text_of(entry, YOUTUBE, "videoId").filter(|id| !id.is_empty())?;
    let url = entry
        .children()
        .find(|node| {
            node.has_tag_name((ATOM, "link")) && node.attribute("rel") == Some("alternate")
        })?
        .attribute("href")?
        .to_owned();
    let title = text_of(entry, ATOM, "title").unwrap_or_default();
    Some(Item { id, title, url })
}

fn text_of(node: Node, namespace: &str, name: &str) -> Option<String> {
    let child = node
        .children()
        .find(|child| child.has_tag_name((namespace, name)))?;
    Some(
        child
            .children()
            .filter_map(|part| part.text())
            .collect::<String>()
            .trim()
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_id_is_read_from_the_id_or_a_channel_link_and_nothing_else() {
        let id = "UC_x5XG1OV2P6uZZ5FSM9Ttw";
        assert_eq!(channel_id(id).as_deref(), Some(id));
        assert_eq!(
            channel_id(&format!("https://www.youtube.com/channel/{id}")).as_deref(),
            Some(id)
        );
        assert_eq!(
            channel_id(&format!("https://www.youtube.com/channel/{id}/videos")).as_deref(),
            Some(id)
        );
        assert_eq!(channel_id("@somebody"), None);
        assert_eq!(channel_id("UC_short"), None);
        assert_eq!(channel_id("XX_x5XG1OV2P6uZZ5FSM9Ttw"), None);
        assert_eq!(channel_id("UC_x5XG1OV2P6uZZ5FSM9Tt!"), None);
    }

    #[test]
    fn the_real_feed_is_read_to_its_videos() {
        let videos = parse_feed(include_bytes!("../fixtures/youtube_channel.xml")).unwrap();
        assert!(videos.len() > 1, "{videos:?}");
        assert_eq!(videos[0].id, "vLAegkGGWrM");
        assert_eq!(videos[0].url, "https://www.youtube.com/watch?v=vLAegkGGWrM");
        assert!(videos[0].title.starts_with("Building with Gemini"));
    }

    #[test]
    fn a_feed_that_is_not_xml_is_an_error_and_so_is_one_that_declares_a_dtd() {
        assert!(parse_feed(b"<feed><entry>").is_err());
        assert!(parse_feed(b"not xml at all").is_err());
        let dtd = br#"<?xml version="1.0"?><!DOCTYPE feed [<!ENTITY a "b">]><feed>&a;</feed>"#;
        assert!(parse_feed(dtd).is_err());
    }

    #[test]
    fn a_feed_larger_than_the_limit_is_an_error_not_a_read() {
        let mut body = b"<feed>".to_vec();
        body.resize(MAX_FEED_BYTES + 1, b' ');
        assert!(parse_feed(&body).is_err());
    }

    #[test]
    fn an_entry_without_a_video_id_is_left_out() {
        let body = br#"<feed xmlns="http://www.w3.org/2005/Atom" xmlns:yt="http://www.youtube.com/xml/schemas/2015"><entry><title>x</title><link rel="alternate" href="https://www.youtube.com/watch?v=a"/></entry></feed>"#;
        assert_eq!(parse_feed(body).unwrap(), Vec::new());
    }
}
