# Seirei architecture

Seirei is a terminal front end over T3 Code's orchestration. Each project gets one pane. The pane shows a coordinator conversation and the implementation jobs the coordinator spawned, with live status. The coordinator and every job are ordinary T3 threads. Seirei runs no agents itself.

A background daemon owns the connection to T3 and the synced state. TUI clients attach and detach over a Unix socket. Both ship in one binary, `seirei`.

## Using it

```
$ seirei login               # once: pair with T3, save a 30-day credential
$ seirei machine add macbook https://macbook.tail1234.ts.net:4043   # a T3 on another machine, over Tailscale
$ seirei login macbook       # pair with it, using a code made on that machine
$ seirei machine list        # every machine, its address, and whether the daemon reaches it
$ seirei                     # attach (starting the daemon if needed) and open the project picker
$ seirei seirei-code         # straight to one project's pane; `<machine>/seirei-code` when the title is on several machines
$ seirei status seirei-code  # the same pane as JSON, for scripts and debugging
$ seirei daemon              # the daemon in the foreground
$ seirei daemon --stop
```

In a project pane, `c` creates the coordinator when the project has none. `i` opens the composer, and `Enter` sends to the coordinator. `Tab` moves to the jobs list, `Enter` opens a job's timeline, and `Esc` returns. `a` answers the oldest open question.

## What T3 gives us

Seirei talks to T3's MCP server over streamable HTTP. The local origin comes from `<T3CODE_HOME or ~/.t3>/userdata/server-runtime.json`, and other machines' origins from `machines.toml`. A few behaviors of that server shape everything below. They were read out of the running server's source, not guessed.

- **T3 pushes nothing.** The server has tools only. No resources, no subscriptions. Seirei polls and diffs.
- **`t3_thread_list` is ordered by `updatedAt`, newest first, and every timeline event bumps `updatedAt`.** One page per tick shows everything that changed since the last tick, even in a project with 1,900 threads.
- **Timeline positions are not stable.** T3 recomputes them on every read. Rows get inserted before existing ones, rolled-back rows vanish, and finished items can reopen.
- **Titles set at launch are never regenerated,** so a tag in a title is a permanent handle.
- **A token from `t3 auth session issue` is capped at `approval-required`.** It cannot message or answer a thread running with more autonomy. The pairing-code OAuth flow can grant up to `full-access`, which is why `seirei login` exists.
- **Tool approvals are invisible to MCP.** The pending-request tools return user-input questions only. A thread waiting on an approval has to be approved in T3's UI.
- **A launch needs a model.** `t3_thread_launch` without `modelSelection` uses the project's default model, and rejects the launch when the project has none. `orchestrator_capabilities` lists every provider with its models, and a provider that cannot run lists the reasons as `constraints`.

## Crates

| Crate | Owns | Why it is its own crate |
| --- | --- | --- |
| `seirei-proto` | Domain types, the timeline window, socket frames, the socket path | The only code both sides share. No tokio, no reqwest, no ratatui. |
| `seirei-tui` | The app state machine, keymap, render, terminal and socket runtime | `seirei` depends on it, so it cannot depend on `seirei`. T3's wire format is unreachable from terminal code. |
| `seirei` | The daemon (T3 boundary, project actors, server), `login`, and the CLI | The daemon has one consumer, the binary. A separate crate would hide nothing. |

Module map:

- `seirei_proto::ids` holds opaque T3 ids, `MachineId`, `ProjectRef`, `SendId`, and `T3Time`.
- `seirei_proto::model` holds machines and their status, projects, thread summaries, model selections, runtime modes and the credential's `Access`, lineage, jobs, the coordinator view, questions, and health.
- `seirei_proto::timeline` holds timeline items, the open-set `ItemKind`, and the window.
- `seirei_proto::wire` holds the socket protocol.
- `seirei::config` decides which machines exist and where their configuration and credentials come from.
- `seirei::t3` is the `T3Api` seam, page sizes, positions, and the error taxonomy. `t3::mcp` speaks MCP over HTTP. `t3::wire` turns T3's JSON into domain types. `t3::login` runs the pairing flow.
- `seirei::coordinator` decides attribution, the coordinator key and tags, the coordinator's model, the brief, and the ensure state machine.
- `seirei::project` is one actor per project. It owns the scan, timeline refresh, commands, and publishing.
- `seirei::server` owns the machine registry and every change to it, sessions, the handshake, routing, and the actor registry.
- `seirei_tui::app` is the TUI state machine, `keymap` every binding, and `render` the drawing.

## The daemon's state

Each watched project has one actor task, the only writer of that project's state and durable record. Sync ticks and user commands for a project run on that task. So "ensure coordinator" cannot race the scan that would adopt the coordinator it is creating. There is no lock around project state. Two structures are shared. The machine registry is guarded so that its changes and the files behind them happen together. The registry that maps a `ProjectRef` to its actor is guarded for get-or-spawn.

Actors publish through `watch` channels. Each project has one `Arc<ProjectView>`, and each open timeline has one `Arc<Timeline>`. A slow reader skips to the latest value. Interest is channel liveness. A timeline stays hot exactly as long as some session holds its receiver, and the view's receiver count picks the polling cadence.

The durable state is the coordinator binding in `<state_dir>/machines/<machine>/projects/<project>.json` and each machine's T3 identity in `<state_dir>/machines/<machine>/identity.json`. Everything else is rebuilt from T3 after a restart.

## Machines

A machine is one T3 server. `local` is the T3 on this machine, found through `server-runtime.json`. T3 ids are unique only within one T3, so the daemon keys every project by `ProjectRef`, its machine and its T3 id. Thread ids stay plain, because every thread reference travels inside a project's scope.

Each machine has its own `T3Api`, credential, and state folder. Nothing is shared between machines. A login's credential is saved to `<config_dir>/credentials/<machine>.json`. A token from `$SEIREI_T3_TOKEN` or `t3_token` applies to `local` only.

`ListProjects` asks every machine at once and waits at most 5 s for each. A machine that fails lists no projects and reports why: `NeedsLogin` for a missing, lapsed, or rejected credential, `Unreachable` when T3 does not answer, and `Failed` when it answers with something other than its projects. One machine never fails the whole call. A call goes out even without a credential, so a T3 that is down reads as unreachable rather than as a missing login.

Other machines live in `<config_dir>/machines.toml`, one table per machine with its `address` and an optional `coordinator_model`. `local` is implicit and cannot be added or removed. Its `coordinator_model` stays in `config.toml`. A user may edit the file by hand, and the daemon reads such an edit when it starts.

The daemon owns machine changes. `AddMachine`, `RemoveMachine`, and `Login` are socket commands. `seirei machine add`, `seirei machine remove`, and `seirei login` are clients that send them, and they start the daemon when none runs. The TUI's picker sends them too. So while the daemon runs, it is the only writer of `machines.toml` and the credential files. A change takes one lock over the machine registry, writes the file, and updates the registry before it lets go. Two changes at once cannot lose each other's writes.

A change takes effect at once, and connected TUIs stay connected. `AddMachine` parses the address in the daemon, so a bad address comes back as the parser's message and nothing is written. The daemon then opens the machine's connection with any credential saved for that address. `Login` pairs without holding the lock, because pairing waits on T3. It then saves the credential and swaps it into the machine's existing connection. The machine's project actors keep running, and their next call carries the new token over a new MCP session. `RemoveMachine` drops the connection, stops the machine's project actors, and closes the feed of every session that watches one of its projects. It deletes the machine's saved credential, so no working token stays on disk. The state folder stays, with the machine's identity and coordinator records, so adding the machine back keeps the duplicate rules and its coordinators. It needs a new login.

For now seirei reaches other machines only over Tailscale. A `config::TailnetOrigin` is built only by its parse function, which accepts `https` with a host under `.ts.net` or a Tailscale IP, an optional port, and no path but `/mcp`. It keeps the bare origin. Tailscale Serve uses real certificates, so reqwest's rustls verifies them against the system's roots. The local T3 stays plain HTTP on loopback.

The daemon runs the OAuth flow against the machine's origin. For `local` without a code, it makes one with the local `t3`. A pairing code for a remote T3 has to be made on that machine, because seirei can only run the local `t3`. Without `--code`, `seirei login <machine>` asks for one on a terminal and sends it with `Login`. A `Login` for a remote without a code fails and says where to get one.

Neither an address nor a host identifies a T3. Two addresses can reach one T3, and one host can run two. A T3's identity is its `environmentId`. Every listing that reaches a machine reads it with `t3_environment_read`, and the daemon records it in the machine's `identity.json`. It reloads those files at start. Of the machines that share an `environmentId`, the first in `MachineId` order (`local`, then by name) owns that T3. Every other one is `Duplicate` of the owner, whether or not the owner answers right now. A duplicate lists no projects, starts no project actors, and refuses commands with a reply that names the owner. So one T3 never gets a second coordinator for a project under another name.

When a machine reads a different `environmentId` than its record, its address now reaches another T3. The daemon replaces the record and drops that machine's project records and actors, because their coordinator bindings name the old server's threads. The machine reports `Replaced` with the old and new T3 until the daemon restarts.

A bare project title that more than one machine lists is ambiguous. `seirei` and `seirei status` then ask for `<machine>/<project>`.

## The protocol

- **Framing.** One JSON object per line over a Unix socket at `$SEIREI_SOCKET`, else `$XDG_RUNTIME_DIR/seirei/daemon.sock`. You can debug it with `socat`.
- **Versioning.** The TUI and daemon are one executable, so the protocol needs build detection, not compatibility rules. `Hello`, `Welcome`, `WrongBuild`, and `Yielding` are frozen. Every other shape may change in any build. A TUI that finds an older daemon offers to restart it.
- **What crosses.** Whole domain values. `Project(ProjectView)` carries the pane, and `Timeline` carries one thread's window. Nothing is a delta, a sequence number, or a view model.
- **Convergence.** `Subscribe { project, job }` is declarative and replaces the last one. `project` is a `ProjectRef`, and every `Project` frame names its machine. A reconnecting client sends `Hello` and its current subscription, and its first frames are the full current values.

## Sync

Each tick of a project's actor runs these steps:

1. Drop timelines nobody holds.
2. **Adopt a lost launch.** When the key has no thread and no lookup has run since the last launch attempt, look up the key's tag and adopt the thread it finds. A tick never launches.
3. **Scan,** once a coordinator is bound. Page `t3_thread_list` until a page reaches back past the horizon. Pages overlap by five rows, because a thread archived mid-scan shifts later rows up. Every row lands in a map, newest `updatedAt` wins. The horizon starts at the binding's `minted_after` and then tracks the newest `updatedAt` seen. A thread bumped mid-scan jumps above the horizon and is caught next tick.
4. **Refresh timelines.** Refresh each hot timeline whose list row shows a new `updatedAt`. A refresh reads the newest 100 items and replaces the window whole. Positions shift, so nothing is merged. A second read happens only when the thread grew between the list call and the read.
5. **Questions.** For `Waiting` threads, list pending question ids and read the new ones.
6. Publish if the view changed. A failed tick publishes too, with its problem in `health`. A project T3 does not list still gets a view, named by its id alone, so the pane says why it is empty.

The cadence is 1.5 s while a client watches the project and 20 s otherwise. Each consecutive failure doubles it, up to 60 s. Any user command triggers an immediate tick.

A tick that fails halfway leaves correct, partly stale state. Every step is an upsert or a whole-value replace, and the next tick redoes it. Writes are classified by `T3Error::may_have_applied`. A `Send` carries the client's `SendId` as T3's `clientRequestId`, so resending cannot duplicate. A coordinator launch whose response was lost is found by its tag before any relaunch.

Only the user's `c` launches a coordinator. A launch T3 refuses fails that command, and the reply carries T3's reason. The sync stays healthy, and nothing launches again until the user presses `c` again.

## The coordinator and its jobs

The coordinator key is the single durable fact per project. `Binding { key, minted_after, thread }` is written atomically before anything is launched under the key.

- The coordinator's title carries `seirei:<KEY>`.
- The brief tells the coordinator to launch each job with `t3_thread_launch` into its own worktree, with a title ending in `[job:<KEY>]`.
- A job is a direct `delegate_task` child of the coordinator, or a top-level MCP-created thread whose title carries the job tag. Anything descended from a job through subagent links is that job's subtask.

`coordinator::jobs` derives roles from the scanned rows on every publish. It is a pure function, and roles are never stored. Rows arrive newest first, so a child is often seen before its parent.

`ensure_step` is the whole lifecycle. The user's `c` runs every step, and a tick runs only `Lookup`:

| Binding | Looked up since last launch | Step | Run by |
| --- | --- | --- | --- |
| none | (any) | `Mint`: persist a fresh key | `c` |
| key, no thread | no | `Lookup`: `t3_thread_list { titleContains: seirei:<KEY> }`, adopt the oldest match | a tick or `c` |
| key, no thread | yes | `Launch`, then persist the thread id | `c` |
| key, thread | (any) | `Ready` | |

`c` sets "looked up" to no before it starts, so it always looks before it launches. That adopts a launch T3 applied after its response was lost, even when a tick already looked. "Looked up" lives in memory, so a restarted daemon looks once on its first tick. A crash between `t3_thread_launch` and recording its result resolves on the next tick. The lookup finds the tagged thread and binds it.

A key with no thread shows as no coordinator, so `c` stays available and reuses the key. The pane shows the coordinator as starting only while a launch the user asked for is in flight, or after the launch until the scan sees its thread.

The coordinator launches at the credential's ceiling. A coordinator above the ceiling would be read-only to seirei.

`coordinator::launch_model` picks the coordinator's model, first match wins:

1. The machine's `coordinator_model`, from `config.toml` for `local` and from `machines.toml` for the others, written `<providerInstanceId>/<model>`.
2. The project's default model. Seirei passes no `modelSelection`, and T3 applies the default.
3. The model of the project's newest thread, the first row of `t3_thread_list`.
4. The first model `orchestrator_capabilities` offers from a provider without constraints.

With none of these, the launch fails before it reaches T3. A launch re-reads the project, so a default set in T3 after the daemon started applies.

## The TUI

`App::update(Event) -> Vec<Effect>` is pure. It does no IO and reads no terminal or clock. `Screen` is `Picker` or `Project`. The picker may hold one machine form, to add a machine, log in to one, or remove one. Inside a project, `Focus` (`Conversation`, `Jobs`, `Job(id)`) and `Mode` (`Browse`, `Compose`, `ConfirmCreate`, `Answer`) are separate enums, so modes exclude each other by construction. The keymap context is derived, never stored. The subscription is derived from the screen and resent whenever it changes or the link comes back.

The picker groups projects under one heading per machine. A heading shows the machine's name, T3's label when it differs, and the machine's status. A machine that needs a login names the `seirei login <machine>` that fixes it. An unreachable one shows why, and a duplicate names the machine that lists its projects. The cursor moves over headings and projects. `m` opens the form that adds a machine. On a heading, `l` opens the login form for any machine but a duplicate, and `x` asks to remove any machine but `local`. While a form's command is in flight, the form says so and ignores another `Enter`. A failure keeps the form open with its input and shows the daemon's message. Success closes the form and lists the projects again. In a project, the breadcrumb reads `seirei › <machine> › <project>`, and it leaves out the machine while `local` is the only one.

The keymap is one table of `(context, key, action, help)`. Dispatch, the help overlay, and the footer all read it. Tests fail the build when a key is bound twice in one context, or when a text context binds a printable key. The text contexts are the composer, the answer form's free text, and the machine forms' fields. A binding shows in the footer only when its action can do something now. "Message the coordinator" needs a coordinator, and a login whose grant allows the coordinator's runtime mode. The view carries the grant of the credential `seirei login` saved. A token from `$SEIREI_T3_TOKEN` or the config file has no known grant, so T3 alone decides. When the grant is all that keeps a binding dead, the pane names the `seirei login` that would make it live.

Losing the daemon mid-send turns the draft into a resendable failure. Resending the same text reuses the draft's `SendId`, so it cannot duplicate. Editing the text makes a new message with a fresh `SendId`, because T3 would drop the edit as a duplicate if the lost send had landed. A confirmed send also starts a fresh `SendId`.

## Synthesis decision

Three designs were built in parallel, each with a compiling sketch. A pushed daemon-rendered view models with keyed patches. B replicated domain state through a sequenced change log and a shared reducer. C used per-project actors with whole-value frames. A cross-judge scored them C 25, B 24, A 23 of 30, and picked C on the smallest surface and invariants enforced by structure.

C is the base. These parts came from elsewhere or replaced C's:

- **Timeline window replace,** replacing C's position-keyed merge. All three candidates merged by position. Running their own merge code against a row inserted mid-thread showed duplicated rows and a missing row. C's version also stalled behind a long-running item.
- **Title tags, from A and B,** replacing C's first-message marker and its durable verdict cache. C rejected title tags because titles might be regenerated, and T3's source shows they never are. Classification became a pure function of list rows.
- **Subtask rollup, from A.** C attributed only the coordinator's direct children, so a launched job's own helpers disappeared.
- **The "may have applied" error taxonomy, from B.** C's errors could not tell "never reached T3" from "response lost".
- **Choices keep `value`, from A,** because T3 expects `value ?? label` in answers.
- **Scan overlap, from B,** because offset cursors shift mid-scan.
- **`seirei status`, from B's headless clients,** as the lever for verifying the daemon without a terminal.
- **`seirei login`, new.** All three designs minted tokens with `t3 auth session issue`, which caps seirei at `approval-required` and is rejected outright without `--subject mcp-client`.

Rejected: A's patch protocol and B's replica machinery (sequence numbers, epochs, digests, a source-hash fingerprint). Whole values on a local socket make both dead weight. Also rejected were B's separate T3 crate, which has one consumer, and B's strict enums that fail a whole page on one unknown value. C parses per record instead.

## Tradeoffs accepted

- We resend a whole timeline window on every change, in exchange for no client reducer and no resync path.
- We restart the daemon on every upgrade, one keypress, in exchange for zero protocol compatibility code.
- We accept about 1.5 s of latency while a project is watched, because T3 offers no push.
- A job launched without the tag is invisible. In exchange, attribution never parses T3's tool-call blobs.
- Sync logic lives in async actor methods rather than a pure reducer. Its tests need a fake `T3Api`. In exchange, the code reads top to bottom.
- `seirei login` lasts 30 days and T3 has no refresh grant. Seirei says so when the token lapses, and the user pairs again.
- An archived job keeps its last status in the pane until the daemon restarts. Nothing removes rows the scan stops seeing.

## Open questions

- Should attribution also read the coordinator's `t3_thread_launch` tool calls? T3 includes the tool output in `dynamic_tool` items, so a job the coordinator forgot to tag could still be found. The output's exact JSON needs a real coordinator transcript first.
- What ceiling should `seirei login` request by default? It requests `full-access` because a coordinator usually runs without approval prompts.
- Should the coordinator brief be user-editable, for example a file in the config directory?
