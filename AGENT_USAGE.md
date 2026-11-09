# iterm: usage for agents

`iterm guide` is the start-here text and `iterm help NOUN` the reference; both always match the binary. This page
is the short version. The v2 commands (`node`, `ledger`, `org`, `transcript`, `exit`, budgets, lends as a gate) ended
with v3; v2 itself is the tag `iterm-v2-final`.

**Writes run at once.** There is no apply gate: `--plan` is the dry run, and `--apply` is accepted and ignored. A
command you only meant to look at must carry `--plan`. Output is one line per item, `key=value`; `--json` anywhere.
Exit 0 ok or planned, 1 failed, 2 refused, 3 timeout.

Three hard rails, and only three: nothing answers a permission or folder-trust prompt (input to such a pane is held,
`queued reason=prompt`); closing a pane whose agent is live needs `--force`; nothing forges the caller's identity.

```sh
iterm tree                                  # everything, one line per node
iterm pane get @me                          # you
iterm agent list                            # agents: provider, model, state
iterm pane read X --tail 40
iterm pane wait X --until idle --timeout 60
iterm pane new --agent --at @me --side right --task T [--name N] [--model M] [--effort E] [--worktree] [--read-only]
iterm tab new --cwd P1 --cwd P2 --agent     # one tab and one agent per folder, in the current window
iterm agent run 'task one' 'task two' --worktree   # one agent per task, wait, one result line each
iterm children --wait                       # block until your children are through
iterm agent msg X --text T                  # also kept in X's mailbox
iterm agent inbox --mark-read
iterm agent merge X
iterm pane close X --force
```

`tab new --agent` starts the agent alone in its tab: it adds no second pane. Agents are named `w1`, `w2`, … in spawn
order, not after their folder; pass `--name {i}`-style names to choose. Add a pane beside one with
`iterm pane new --at X --side right --cmd C`.

Old command names still work as aliases of these (`tab-create` is `tab new`, `spawn` is `pane new --agent`, `close` is
`pane close`), with the same open behavior: they write at once as well.
