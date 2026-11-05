# Contributing to Helide

Helide is a fork of [Helix](https://github.com/helix-editor/helix) that adds a file tree, a git panel, terminals and AI agent panels around the editor. Bug reports, ideas and pull requests are all welcome on the [issue tracker](https://github.com/ujjwalvivek/helide-rs/issues).

## Where to report what

- Something wrong with the tree, git panel, terminals, agents, tabs, history, preview or workspaces: open an issue here.
- Something that also happens in plain Helix (editing, keymaps, language servers, a grammar): report it to [Helix](https://github.com/helix-editor/helix/issues) instead, since Helide keeps Helix's editing untouched and picks up its fixes when it merges upstream.

When you report a bug, say which build you run (helide or otari, and the version from the release name), your terminal, and how to make it happen.

## Branches

- **helide** is the main branch. Pull requests go here.
- **helide-rs-mozilla** is helide plus the Otari integration built for the Mozilla hackathon. General fixes are copied over to it; Otari-only work stays there.
- **master** mirrors upstream Helix and is only used to merge it in.

## Building

Helide builds like Helix. You need the Rust version in rust-toolchain.toml (rustup picks it up on its own).

```sh
cargo run              # a debug build, quickest to compile
cargo install --path helix-term --locked   # installs hx
```

The agent panel needs `claude` or `opencode` on your PATH, the code stats need [scc](https://github.com/boyter/scc), and a Nerd Font makes the file icons show.

To print-debug, log with log::info!, warn! or error!, start with `cargo run -- --log foo.log -v`, and follow the file with `tail -f foo.log` in another terminal.

## Where the code is

Almost everything Helide adds lives in helix-term:

| File | What it holds |
| --- | --- |
| ui/editor.rs | The frame around the editor: panel layout, tabs per split, drag and drop, workspaces |
| ui/file_tree.rs, ui/code_stats.rs | The file tree and the scc stats under it |
| ui/git_panel.rs | Changes, staging, commits, branches, stashes and the commit graph |
| ui/terminal.rs, ui/agents.rs | Terminal and agent tabs, and how an agent's turn is detected |
| ui/frame.rs, ui/panel_style.rs | Tab strips and the shared panel look |
| ui/preview.rs, history.rs | Markdown preview and :history |
| ide.rs, mcp.rs, ipc.rs | The bridge Claude Code talks to, and hx msg |
| model.rs | :rewrite, :ask and commit messages (otari.rs on the mozilla branch) |
| watch.rs | Watching the disk so the tree and git panel stay current |

Smaller hooks are spread through helix-view (documents, the split tree) and helix-tui. Helix's own [architecture.md](./architecture.md) still describes everything underneath.

## Testing

- `cargo test --workspace` runs the unit tests.
- `cargo integration-test` runs the integration tests; Helide's are in helix-term/tests/test/helide.rs.
- `cargo clippy --workspace` should come out clean.

Most of Helide is UI, so for those changes the real test is running hx and using it. Say in your pull request what you tried on screen.

## Pull requests

- Keep one change per pull request, and keep Helix's own code as close to upstream as you can, so merging new Helix releases stays easy.
- Run rustfmt on the files you touched (`cargo fmt` reformats the whole workspace).
- Commit messages are one line, starting with "helide: " and saying what changed for the user, like "helide: right click pastes in terminals and agents".
- Releases are built by a workflow when a release is published, so you don't need to attach builds.

By contributing you agree that your work is licensed under the [MPL-2.0](../LICENSE), like the rest of Helix.
