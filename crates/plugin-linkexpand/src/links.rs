//! The links in a message that are expanded. A provider names its own pattern; what the providers share
//! is here: each link once, at most `MAX_PER_MESSAGE` of them, and none written as `<…>`, which is how a
//! writer asks for no preview.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// The most links of one kind that are expanded in a single message.
pub const MAX_PER_MESSAGE: usize = 3;

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static MESSAGE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:(?:ptb|canary)\.)?discord(?:app)?\.com/channels/(?P<guild>\d{1,20})/(?P<channel>\d{1,20})/(?P<message>\d{1,20})(?P<close>>)?",
    )
    .expect("a fixed pattern")
});

/// A link to a message of a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageLink {
    pub guild: u64,
    pub channel: u64,
    pub message: u64,
}

/// A link to a post of a site: the name of its author in the link, and the id of the post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostLink {
    pub user: String,
    pub id: u64,
}

fn wrapped(captures: &Captures<'_>) -> bool {
    captures.name("open").is_some() && captures.name("close").is_some()
}

/// The links `parse` reads from the matches of `pattern` in `text`: each once, in order, at most
/// `MAX_PER_MESSAGE`, and none written as `<…>`.
#[must_use]
pub fn scan<T: PartialEq>(
    pattern: &Regex,
    text: &str,
    parse: impl Fn(&Captures<'_>) -> Option<T>,
) -> Vec<T> {
    let mut found: Vec<T> = Vec::new();
    for captures in pattern.captures_iter(text) {
        if wrapped(&captures) {
            continue;
        }
        let Some(link) = parse(&captures) else {
            continue;
        };
        if !found.contains(&link) {
            found.push(link);
        }
        if found.len() == MAX_PER_MESSAGE {
            break;
        }
    }
    found
}

/// The message links in `text`, each once, in order, at most [`MAX_PER_MESSAGE`].
#[must_use]
pub fn message_links(text: &str) -> Vec<MessageLink> {
    scan(&MESSAGE_LINK, text, |captures| {
        // An id is never 0: a link that says so names nothing, and must not reach an `Id::new`.
        let number = |name: &str| captures[name].parse::<u64>().ok().filter(|id| *id > 0);
        Some(MessageLink {
            guild: number("guild")?,
            channel: number("channel")?,
            message: number("message")?,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x::post_links;

    #[test]
    fn a_link_with_an_id_of_zero_names_nothing() {
        assert!(message_links("https://discord.com/channels/1/0/3").is_empty());
        assert!(message_links("https://discord.com/channels/0/2/3").is_empty());
        assert!(post_links("https://x.com/jack/status/0").is_empty());
    }

    #[test]
    fn message_links_of_every_discord_host_are_found_once_each() {
        let text = "see https://discord.com/channels/1/2/3 and https://ptb.discord.com/channels/1/2/4 \
            and https://canary.discordapp.com/channels/1/2/5 and again https://discord.com/channels/1/2/3";
        let links = message_links(text);
        assert_eq!(
            links,
            [
                MessageLink {
                    guild: 1,
                    channel: 2,
                    message: 3
                },
                MessageLink {
                    guild: 1,
                    channel: 2,
                    message: 4
                },
                MessageLink {
                    guild: 1,
                    channel: 2,
                    message: 5
                },
            ]
        );
    }

    #[test]
    fn a_link_in_angle_brackets_is_a_request_for_no_preview() {
        assert!(message_links("<https://discord.com/channels/1/2/3>").is_empty());
        assert!(post_links("<https://x.com/user/status/9>").is_empty());
        assert_eq!(
            message_links("<https://discord.com/channels/1/2/3").len(),
            1
        );
    }

    #[test]
    fn direct_message_links_and_other_sites_are_not_message_links() {
        assert!(message_links("https://discord.com/channels/@me/2/3").is_empty());
        assert!(message_links("https://example.com/channels/1/2/3").is_empty());
        assert!(message_links("https://discord.com/channels/1/2").is_empty());
    }

    #[test]
    fn posts_on_x_and_the_old_name_are_found_with_their_user() {
        let links = post_links(
            "https://x.com/jack/status/20 https://twitter.com/jack/status/21?s=20 \
             https://mobile.twitter.com/jack/status/22 https://www.x.com/jack/status/20",
        );
        let ids: Vec<u64> = links.iter().map(|l| l.id).collect();
        assert_eq!(ids, [20, 21, 22]);
        assert!(links.iter().all(|l| l.user == "jack"));
        assert!(post_links("https://x.com/jack").is_empty());
        assert!(post_links("https://notx.com/jack/status/1").is_empty());
    }

    #[test]
    fn at_most_three_links_of_a_kind_are_taken() {
        let text = (1..=6)
            .map(|n| format!("https://x.com/a/status/{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(post_links(&text).len(), MAX_PER_MESSAGE);
    }
}
