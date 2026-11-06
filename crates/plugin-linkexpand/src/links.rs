//! The links in a message that the bot expands: links to messages of the same server, and posts
//! on X. A link written as `<https://...>` is left alone: that is how a writer asks for no preview.

use std::sync::LazyLock;

use regex::Regex;

/// The most links of one kind that are expanded in a single message.
pub const MAX_PER_MESSAGE: usize = 3;

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static MESSAGE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:(?:ptb|canary)\.)?discord(?:app)?\.com/channels/(?P<guild>\d{1,20})/(?P<channel>\d{1,20})/(?P<message>\d{1,20})(?P<close>>)?",
    )
    .expect("a fixed pattern")
});

#[allow(clippy::expect_used, reason = "a fixed pattern, compiled by the tests")]
static X_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?P<open><)?https?://(?:www\.|mobile\.)?(?:x|twitter)\.com/(?P<user>[A-Za-z0-9_]{1,15})/status/(?P<id>\d{1,20})(?P<close>>)?",
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

/// A link to a post on X.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostLink {
    pub user: String,
    pub id: u64,
}

fn wrapped(captures: &regex::Captures<'_>) -> bool {
    captures.name("open").is_some() && captures.name("close").is_some()
}

/// The message links in `text`, each once, in order, at most [`MAX_PER_MESSAGE`].
#[must_use]
pub fn message_links(text: &str) -> Vec<MessageLink> {
    let mut found: Vec<MessageLink> = Vec::new();
    for captures in MESSAGE_LINK.captures_iter(text) {
        if wrapped(&captures) {
            continue;
        }
        let number = |name: &str| captures[name].parse::<u64>().ok();
        let (Some(guild), Some(channel), Some(message)) =
            (number("guild"), number("channel"), number("message"))
        else {
            continue;
        };
        let link = MessageLink {
            guild,
            channel,
            message,
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

/// The links to posts on X in `text`, each once, in order, at most [`MAX_PER_MESSAGE`].
#[must_use]
pub fn post_links(text: &str) -> Vec<PostLink> {
    let mut found: Vec<PostLink> = Vec::new();
    for captures in X_LINK.captures_iter(text) {
        if wrapped(&captures) {
            continue;
        }
        let Ok(id) = captures["id"].parse::<u64>() else {
            continue;
        };
        let link = PostLink {
            user: captures["user"].to_owned(),
            id,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        // Only one bracket is not a pair.
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
