//! A thread of posts as the messages of an answer: what a post answers, the post that was linked, and
//! what it quotes. Each post is an embed, whichever site it comes from.

use plugin_kit::{image, truncate};
use twilight_model::channel::message::Embed;
use twilight_model::util::Timestamp;
use twilight_util::builder::embed::{EmbedAuthorBuilder, EmbedBuilder, EmbedFooterBuilder};

use crate::model::{Author, Post};

/// The bar of a post shown for the sake of another, so that a reader can tell it from the one linked.
pub const REPLIED_COLOR: u32 = 0x8E_95_9E;
pub const QUOTED_COLOR: u32 = 0xA3_6C_F5;
pub const REPOST_COLOR: u32 = 0x00_BA_7C;

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
    /// The pictures, each to be attached as a file.
    pub photos: Vec<String>,
    /// The video, or the animation.
    pub video: Option<String>,
    pub link: String,
    /// Whether this is the post that was linked, and not the one it answers or quotes.
    pub main: bool,
    /// The `Referer` the pictures need, see [`Post::media_referer`].
    pub referer: Option<&'static str>,
}

/// A post with the posts around it: every post it answers up to the first one, and the posts it
/// quotes, one inside the other.
#[derive(Debug, Clone)]
pub struct Thread {
    /// What the post answers: the first post of the conversation first, the post it answers last.
    pub ancestors: Vec<Post>,
    pub post: Post,
    /// The post it quotes, the post that one quotes, and so on.
    pub quotes: Vec<Post>,
}

impl Thread {
    #[must_use]
    pub fn new(post: Post) -> Self {
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

    /// Whether one of the posts is marked for adults.
    #[must_use]
    pub fn any_sensitive(&self) -> bool {
        self.ancestors
            .iter()
            .chain(std::iter::once(&self.post))
            .chain(&self.quotes)
            .any(|post| post.sensitive)
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
    let post = &thread.post;
    all.push(match &post.reposted_by {
        Some(who) => piece(
            post,
            Some(format!("{} {}", labels.reposted_by, who.name)),
            REPOST_COLOR,
            limit,
            true,
        ),
        None => piece(post, None, post.color, limit, true),
    });
    let quoted = thread.quotes.len();
    for (at, quote) in thread.quotes.iter().enumerate() {
        let title = numbered(&labels.quoted, at + 1, quoted);
        all.push(piece(quote, Some(title), QUOTED_COLOR, limit, false));
    }
    all
}

fn author_line(author: &Author) -> EmbedAuthorBuilder {
    let mut line = EmbedAuthorBuilder::new(truncate(
        &format!("{} (@{})", author.name, author.handle),
        250,
    ))
    .url(author.profile.clone());
    if let Some(icon) = author.avatar.as_deref().and_then(image) {
        line = line.icon_url(icon);
    }
    line
}

/// One post as a message. `title` says what the post is to the one that was linked (the post it
/// answers, the post it quotes, or a repost) and `color` is its bar.
fn piece(post: &Post, title: Option<String>, color: u32, limit: usize, main: bool) -> Piece {
    let mut embed = EmbedBuilder::new()
        .color(color)
        .author(author_line(&post.author))
        .url(&post.url);
    if !post.counts.is_empty() {
        embed = embed.footer(EmbedFooterBuilder::new(post.counts.join("  ")));
    }
    if let Some(title) = title {
        embed = embed.title(title);
    }
    if !post.text.trim().is_empty() {
        embed = embed.description(truncate(&post.text, limit));
    }
    if let Some(at) = post.created.and_then(|s| Timestamp::from_secs(s).ok()) {
        embed = embed.timestamp(at);
    }
    Piece {
        embed: embed.build(),
        photos: post.images.clone(),
        video: post.video.clone(),
        link: post.url.clone(),
        main,
        referer: post.media_referer,
    }
}
