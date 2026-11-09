# iterm-cli

`iterm` is one small Rust binary that drives [iTerm2](https://iterm2.com) from the command line: every window,
tab, split pane, profile, setting and menu, for humans and for AI agents. It also starts, names, messages and
waits on coding agents (Claude Code, Codex, OMP) in panes, so one agent can orchestrate others.

## Install

```sh
brew install nikhi1g/tap/iterm-cli      # Homebrew
cargo install --git https://github.com/nikhi1g/iterm-cli   # from source
```

Both install the command `iterm`. Prebuilt binaries for Apple silicon and Intel are attached to each
[release](https://github.com/nikhi1g/iterm-cli/releases).

Requirements: macOS, iTerm2 running, and **Settings → General → Magic → Enable Python API** switched on.
`iterm` talks to iTerm2's official API over its local socket; iTerm2 asks you once to allow it. It starts no
daemon and sends nothing over the network.

## Use

```sh
iterm guide                                   # start here
iterm tree                                    # every window, tab, pane and agent, one line each
iterm pane new --at @me --side right          # split beside the pane you are in
iterm pane read X --tail 40                   # a pane's last lines
iterm window set W --tile grid
iterm agent run 'task one' 'task two' --worktree    # one agent per task, wait, one result line each
iterm agent msg X --text 'status?'
iterm help                                    # the grammar; iterm help NOUN for one noun
```

The grammar is `iterm <noun> <verb> [SELECTOR] [PLACEMENT] [GEOMETRY] …`. Output is one `key=value` line per
item, or `--json`. Writes run at once; `--plan` shows what a command would do without doing it.

Three things it never does: answer a permission or folder-trust prompt for you, close a pane with a live agent
without `--force`, or forge the caller's identity.

## Build from source

```sh
cargo build --release      # target/release/iterm
cargo test                 # runs against an in-memory fake; iTerm2 is not needed
```

## License

MIT © Nikhil Gupta. See [LICENSE](LICENSE).
