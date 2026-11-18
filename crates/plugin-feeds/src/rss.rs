//! RSS 2.0 and Atom feeds, as the sites and `RSSHub` write them.

use anyhow::{Context as _, Result, bail};
use roxmltree::{Document, Node, ParsingOptions};

use crate::source::Item;
use crate::youtube::MAX_FEED_BYTES;

/// The items of a feed, in the order the feed gives them (newest first). An item without an http or
/// https link is left out; its id is its guid or id, or else its link.
///
/// # Errors
/// The body is not XML, is neither RSS nor Atom, declares a DTD, or is larger than [`MAX_FEED_BYTES`].
pub fn parse(body: &[u8]) -> Result<Vec<Item>> {
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
    let entries: Vec<Node> = match root.tag_name().name() {
        "rss" => root
            .children()
            .filter(|node| named(*node, "channel"))
            .flat_map(|channel| channel.children())
            .filter(|node| named(*node, "item"))
            .collect(),
        "feed" => root
            .children()
            .filter(|node| named(*node, "entry"))
            .collect(),
        _ => bail!("the feed is neither RSS nor Atom"),
    };
    Ok(entries.into_iter().filter_map(item).collect())
}

/// Matches by local name, so the namespace prefix a feed uses does not matter.
fn named(node: Node, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn item(node: Node) -> Option<Item> {
    let url = link(node)?;
    let id = child_text(node, "guid")
        .or_else(|| child_text(node, "id"))
        .unwrap_or_else(|| url.clone());
    let title = child_text(node, "title").unwrap_or_default();
    Some(Item { id, title, url })
}

/// The text of an RSS `link`, or the `href` of an Atom `link` that is the alternate one or has no
/// `rel`; the first that is a web address.
fn link(node: Node) -> Option<String> {
    node.children()
        .filter(|child| named(*child, "link"))
        .find_map(|child| match child.attribute("href") {
            Some(href) => child
                .attribute("rel")
                .is_none_or(|rel| rel == "alternate")
                .then(|| href.to_owned()),
            None => text_of(child),
        })
        .and_then(|url| web_link(&url))
}

fn web_link(url: &str) -> Option<String> {
    let url = url.trim();
    let web = ["http://", "https://"].into_iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    });
    web.then(|| url.to_owned())
}

fn child_text(node: Node, name: &str) -> Option<String> {
    node.children()
        .find(|child| named(*child, name))
        .and_then(text_of)
}

fn text_of(node: Node) -> Option<String> {
    let text = node
        .children()
        .filter_map(|part| part.text())
        .collect::<String>();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reddit_atom_feed_gives_its_posts_newest_first() {
        let items = parse(include_bytes!("../fixtures/reddit_rust.xml")).unwrap();
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].id, "t3_abc123");
        assert_eq!(items[0].title, "This Week in Rust");
        assert_eq!(
            items[0].url,
            "https://www.reddit.com/r/rust/comments/abc123/this_week_in_rust/"
        );
        assert_eq!(items[1].id, "t3_def456");
    }

    #[test]
    fn an_rsshub_rss_feed_gives_its_items_by_guid() {
        let items = parse(include_bytes!("../fixtures/rsshub_twitter.xml")).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, "https://twitter.com/someone/status/1001");
        assert_eq!(items[0].url, "https://twitter.com/someone/status/1001");
    }

    #[test]
    fn an_item_without_a_web_link_is_left_out_and_an_id_falls_back_to_the_link() {
        let body = b"<rss><channel><item><title>a</title></item><item><title>b</title><link>javascript:alert(1)</link></item><item><title>c</title><link>ftp://example.org/c</link></item><item><title>d</title><link>https://example.org/d</link></item></channel></rss>";
        let items = parse(body).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].id, "https://example.org/d");
    }

    #[test]
    fn a_feed_that_is_not_xml_declares_a_dtd_or_is_neither_rss_nor_atom_is_an_error() {
        assert!(parse(b"not xml at all").is_err());
        let dtd = br#"<?xml version="1.0"?><!DOCTYPE rss [<!ENTITY a "b">]><rss>&a;</rss>"#;
        assert!(parse(dtd).is_err());
        assert!(parse(b"<html><body/></html>").is_err());
    }

    #[test]
    fn a_feed_larger_than_the_limit_is_an_error() {
        let mut body = b"<rss>".to_vec();
        body.resize(MAX_FEED_BYTES + 1, b' ');
        assert!(parse(&body).is_err());
    }
}
