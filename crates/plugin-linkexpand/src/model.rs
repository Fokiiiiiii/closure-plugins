//! The post of any site, as the shared sending shows it. A site only says how its links are found,
//! how its service answers for a post, and which files may be fetched; the rest is the same for all.

/// Who wrote a post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    pub name: String,
    /// The name the site knows the author by, such as `jack` for `@jack`.
    pub handle: String,
    pub avatar: Option<String>,
    /// The page of the author on the site.
    pub profile: String,
}

/// A post of a site.
#[derive(Debug, Clone)]
pub struct Post {
    /// The id the site gives the post, which its link names.
    pub id: u64,
    /// The link to the post, which the answer links back to.
    pub url: String,
    pub author: Author,
    pub text: String,
    /// The pictures, at most the site's limit, each at one of the site's media hosts.
    pub images: Vec<String>,
    /// The video, at one of the site's media hosts.
    pub video: Option<String>,
    /// The counts as labels, such as `💬 32`; the ones the site did not give are not here.
    pub counts: Vec<String>,
    /// Unix seconds.
    pub created: Option<i64>,
    /// Whether the site marks the post for adults.
    pub sensitive: bool,
    /// The colour of the post's bar.
    pub color: u32,
    /// The id of the post it answers.
    pub reply_to: Option<u64>,
    /// The post it quotes. Its id is what the whole of it is fetched by.
    pub quote: Option<Box<Post>>,
    /// Who reposted it, when the link is to a repost.
    pub reposted_by: Option<Author>,
    /// The `Referer` the file server of the pictures needs, when it needs one.
    pub media_referer: Option<&'static str>,
}
