# closure-plugins

Optional modules for [closure](https://github.com/Fokiiiiiii/closure). Each plugin is a crate that
declares a module with closure's SDK, so it gets what every module of closure gets: its settings in
the dashboard, a switch per server, permissions and capabilities checked by the host.

| Plugin | What it does |
| --- | --- |
| `plugin-linkexpand` | Answers a link to a message of the server (a quote) and a link to a post on X (text, pictures, counts, video) with a preview. A link in `<...>` is left alone, and a message is quoted only for someone who can read its channel. |
| `plugin-starboard` | Posts a message that gets enough of an emoji (⭐ by default) to a channel, and keeps its count up to date. Age-restricted and non-public channels are never posted. |

`plugin-kit` holds what the plugins share, and `closure-plugins` lists them all in one function:
`closure_plugins::modules()`.

## Building them into closure

closure has a Cargo feature for this, off by default:

```sh
cargo build --release --features plugins      # in closure
```

The feature adds the git dependency on this repository and puts the modules of
`closure_plugins::modules()` after closure's own. A server turns each one on in the dashboard.
closure's workspace patches `closure-core`, `closure-sdk` and `closure-i18n` to its own copies, so
the plugins and closure share one set of types.

Because the dependency is in closure's manifest, building closure needs to be able to fetch this
repository (even without the feature).

## Adding a plugin

1. A crate in `crates/` that depends on `closure-sdk` and `closure-core`, and exposes
   `module() -> Result<SdkModule, CoreError>`.
2. One line in `crates/closure-plugins/src/lib.rs`.

Tests use closure's `Harness` (`closure_sdk::testing`), which plays Discord.

## Developing

The plugins depend on closure through git. To build against a checkout next to this one, put this
in `.cargo/config.toml` (it is git-ignored):

```toml
[patch."https://github.com/Fokiiiiiii/closure"]
closure-core = { path = "../closure/crates/closure-core" }
closure-sdk = { path = "../closure/crates/closure-sdk" }
closure-i18n = { path = "../closure/crates/closure-i18n" }
```

```sh
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
