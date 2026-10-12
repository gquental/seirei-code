<img src="assets/icon.svg" width="64" height="64" alt="">

# seirei

Seirei is a terminal front end for [T3 Code](https://t3.codes). It gives each project one pane. The pane holds a coordinator thread you talk to and the implementation threads it launches, each with live status. The coordinator and its jobs are ordinary T3 threads, so seirei runs no agents of its own.

A background daemon holds the connection to T3 and keeps each project in sync. The terminal UI attaches to the daemon over a Unix socket. Both ship in one binary, `seirei`.

## Install from source

You need [mise](https://mise.jdx.dev) and a running T3 Code.

```sh
mise install
cargo build --release
```

The binary is `target/release/seirei`. Put it on your `PATH`. After a rebuild, seirei finds the older daemon and offers to restart it. Press `r` to accept.

## First run

Pair seirei with T3 once.

```sh
seirei login
```

This runs `t3 auth pairing create` for a pairing code, so the `t3` CLI must be on your `PATH`. To use a code from T3's UI instead, pass `--code`. The credential lasts 30 days. Run `seirei login` again when it lapses.

Then start seirei.

```sh
seirei
```

It starts the daemon if needed and opens the project picker.

## Commands

| Command | What it does |
| --- | --- |
| `seirei` | Opens the project picker. Starts the daemon if needed. |
| `seirei <project>` | Opens one project's pane, by title or id. Write `<machine>/<project>` when the title is on more than one machine. |
| `seirei status [project]` | Prints every machine with its status and projects, or one project's pane, as JSON. |
| `seirei login [machine] [--access <level>] [--code <code>]` | Pairs with a machine's T3 and saves a credential. The machine defaults to `local`. |
| `seirei machine add <name> <address>` | Adds the T3 on another machine. |
| `seirei machine list` | Lists every machine, with its status when a daemon is running. |
| `seirei machine remove <name>` | Stops reaching a machine. |
| `seirei daemon [--stop]` | Runs the daemon in the foreground, or stops the running one. |

`--access` sets the most autonomy seirei may act with. The levels are `read-only`, `approval-required`, `auto-accept-edits`, `auto`, and `full-access`, the default. Seirei cannot message or answer a thread that runs above your level. The pane tells you when that happens and which login would fix it.

## Other machines

Seirei can also manage the projects of T3 servers on your other machines. For now it reaches them only over [Tailscale](https://tailscale.com). Serve T3 on your tailnet on the other machine, then add it under a name of your choice.

```sh
seirei machine add macbook https://macbook.tail1234.ts.net:4043
```

The address must use `https` with a `.ts.net` name or a Tailscale IP. Give the port when it is not 443. Then pair with the machine.

```sh
seirei login macbook
```

Seirei asks for a pairing code. The code has to come from that machine, so create one in its T3, or run `t3 auth pairing create --ttl 10m` there. In a script, pass the code with `--code` instead. The credential lasts 30 days, like the local one.

`seirei machine list` shows each machine and whether the daemon reaches it. A project title that is on two machines needs its machine, as in `seirei macbook/seirei-code`. When two names reach the same T3, seirei lists its projects once. They go under `local`, or else under the name that sorts first, even while that machine is down.

## Keys

In the picker, `j` and `k` move, `Enter` opens a project, and `q` quits.

In a project pane:

| Key | Action |
| --- | --- |
| `c` | Create the coordinator when the project has none. |
| `i` | Write to the coordinator. `Enter` sends, `Alt-Enter` adds a line, and `Esc` keeps the draft. |
| `Tab` | Switch between the conversation and the jobs list. |
| `j` / `k` | Scroll, or move through the jobs list. |
| `Enter` | Open the selected job's timeline. |
| `a` | Answer the oldest open question. |
| `Esc` | Go back. |
| `?` | Show every key. |
| `Ctrl-C` | Quit. |

In the answer form, `j` and `k` move between options, `Space` toggles one, `Enter` answers, and `Esc` leaves the question for later.

## Configuration

Seirei reads `$XDG_CONFIG_HOME/seirei/config.toml`, usually `~/.config/seirei/config.toml`. Every key is optional, and the file may be absent.

| Key | What it does |
| --- | --- |
| `coordinator_model` | The model every new coordinator runs on, written `"<providerInstanceId>/<model>"`, like `"claudeAgent/claude-opus-5-5"`. |
| `t3_token` | A token for the local T3, used instead of the `seirei login` credential. `$SEIREI_T3_TOKEN` overrides it. |
| `t3_home` | Where T3 keeps its data. The default is `~/.t3`. `$T3CODE_HOME` overrides it. |

Other machines live in `machines.toml` next to `config.toml`. `seirei machine add` writes it, and you may edit it by hand. Each table is one machine, with its `address` and, optionally, its own `coordinator_model`.

```toml
[macbook]
address = "https://macbook.tail1234.ts.net:4043"
coordinator_model = "claudeAgent/claude-opus-5-5"
```

Without `coordinator_model`, a new coordinator runs on the project's default model in T3. When the project has no default, the coordinator runs on the model of the project's most recent thread, or else on the first model T3 can run. `seirei status <project>` shows the provider and model of the coordinator and each job. Restart the daemon after you change the file.

## How it works

[docs/architecture.md](docs/architecture.md) covers the daemon, the socket protocol, sync, and how seirei finds a coordinator's jobs.
