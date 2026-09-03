# Completion hook: run a configured command for every finished recording

## Overview

mimi writes a finished meeting into `output_dir` and stops there. The transcription pipeline on this machine is fed from a NATS stream, and today a mimi recording only reaches it when someone enqueues it by hand. This plan gives mimi one generic seam: a configured command that runs once for every recording that completed while the hook was configured, with the recording and its sidecar handed over in the environment, retried until it exits 0, with the state of that delivery kept in the sidecar on disk.

mimi learns nothing about NATS, envelopes or the transcriber. The command is opaque; what it does with the recording is the command's business and lives outside this repository. What mimi guarantees is delivery: a recording completed under a configured hook is handed to the command until one run exits 0, across hook failures, daemon restarts and reboots, without ever blocking the next recording.

Key properties:
- The hook never delays recording. Completion hands the sidecar path to a dedicated hook thread and the run loop goes back to watching the microphone.
- One run at a time, in order of completion. A hook that fails is retried with backoff while the daemon runs; a hook that never succeeded before the daemon stopped is found again at the next start from the sidecar alone.
- The sidecar is the only state. A recording completed under a hook carries `"on_complete": {"state": "pending"}` from the moment it is written and `{"state": "done", "at": "..."}` after the first successful run. Sidecars written before the hook existed carry no field and are never touched, so enabling the hook publishes nothing retroactively.
- With `on_complete` unset every file mimi writes is byte-for-byte what it writes today.

### Non-goals

- No NATS, HTTP or any network client in mimi. The hook is a process.
- No backfill of recordings that completed before the hook was configured. A sidecar without an `on_complete` field is not the hook's business.
- No concurrency between hook runs, no per-recording priority.
- No hook on session start, device change or failure - completion only.
- No capture of the hook's stdout and stderr into structured fields; the child inherits mimi's streams and its output lands in the daemon log as-is.
- No attempt counter, last error or failure reason in the sidecar. Failures are logged; the sidecar records only pending or done.
- No graceful SIGTERM phase before the timeout kill. The command is expected to finish well inside the timeout; on expiry its process group is killed outright.
- No `mimi` subcommand to re-run a hook by hand. The contract is plain enough to run from a shell: set the two environment variables and run the command.
- No per-hook environment or working-directory configuration beyond what the contract fixes.

### Rejected alternatives

- **A NATS client inside mimi (`async-nats`)** - the only maintained Rust client is async and brings tokio plus roughly eighty transitive crates into a crate that is deliberately std-only, and every change to the consumer's message shape would need a mimi release. The synchronous `nats` crate is deprecated and receives security fixes only. A hand-written wire client and a shell-out to the `nats` CLI were considered and dropped for the same reason: the recorder should not carry the queue.
- **Retry inside the hook command** - every command would have to reinvent an outbox to survive restarts, while mimi already owns a durable per-recording file. Delivery state belongs next to the recording, so retry lives in mimi and the command only has to be idempotent.
- **A marker file per recording (`<stem>.published`)** - a third file beside the audio and the sidecar for one bit of state. The sidecar already exists, is written by mimi alone, and `ls` keeps showing two entries per recording.
- **Piping the hook's output** - a pipe the hook thread must drain while also enforcing a timeout is a deadlock waiting to happen; inheriting the daemon's streams keeps the thread free of reading.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - every file under `src/` follows it: `for` loops over iterator chains, `let ... else`, explicit destructuring, no wildcard matches, enums over bools, newtypes over strings.
- `rustdoc` - for the `///` lines on items another module calls; `CLAUDE.md`'s narrower rule wins where they differ (one sentence, starts with the item's name, no `# Examples`).

## Context (from discovery)

- `src/sink.rs` - `Sink` trait (`accept(&self, recording: Recording) -> Result<(), SinkError>`), `LocalFolder` (rename `.partial` away, remux to m4a, write the sidecar with `write_private` at mode 0600), the `Sidecar` struct and `sidecar()` builder, `file_stem`/`reserve`. The plan for v1 named this trait as the seam for exactly this kind of feature.
- `src/session/mod.rs` - `run` is the loop; `finish_session` builds a `Recording` and calls `sink.accept` synchronously, so anything slow in the sink delays the run loop. `Session` holds `partial`, `started_at`, `bundle_id`, `writer`; `SessionStart` from `decide.rs` also carries `label: String`, which the sidecar does not receive today. `Shutdown` is the `Arc<AtomicBool>` the signal handlers raise; the loop checks `requested()` between events and calls `finish_session` once more after it.
- `src/config.rs` - flat TOML keys into `ConfigFile` with `deny_unknown_fields`, defaults as constants, `positive()` for integer validation, `Display` for `--check-config`, tests through a `TempHome` helper.
- `src/main.rs` - `run()` destructures `Config`, spawns the poller thread, calls `session::run` with `&LocalFolder`, drops the poller's stop sender afterwards. The composition root for the new thread.
- `src/writer.rs` - the existing pattern for a worker thread: `spawn` returns a handle holding a `JoinHandle`, errors travel in a shared slot, the thread is joined on stop.
- `src/macos/mod.rs` - `user_id()` is the sanctioned shape for a libc call: a one-line safe wrapper so callers stay free of `unsafe`. The killpg wrapper this plan adds follows it.
- Dependencies already present and sufficient: `serde`, `serde_json`, `chrono`, `libc`, `tracing`, `thiserror`. Nothing is added to `Cargo.toml`.
- Definition of done from `CLAUDE.md`: `mise run check` green, the unsafe grep passes, every new module declared with `mod`.

## Development Approach

- **Testing approach**: regular - implement, then tests, within the same task.
- Complete each task fully before moving to the next.
- Make small, focused changes.
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task; tests are listed as separate checklist items and cover success and failure paths.
- **CRITICAL: all tests must pass before starting the next task** - `mise run check` is the gate, not `cargo test` alone.
- **CRITICAL: update this plan file when scope changes during implementation.**
- Maintain backward compatibility: with `on_complete` unset, no file mimi writes changes by a byte.

## Code-Quality Rules (verify before marking each task complete)

The `rust-style` skill has no separate hard-rules block; its rules and this crate's own definition of done are materialized here. A task is not complete while any of these is violated.

**rust-style:**
- `for` loops with mutable accumulators, not iterator combinator chains.
- `let ... else` for early exits; `if let` only when the branch is short and there is no else.
- Shadow through transformations; no `raw_`, `parsed_`, `trimmed_` prefixes.
- Newtypes over meaningful strings, enums over `bool` parameters.
- Match every variant explicitly - no `_ =>` arms, no `matches!`. Destructure structs and tuples explicitly.
- No comments. A comment is justified only when the WHY cannot be recovered from the code; this plan and `CLAUDE.md` carry the reasoning.
- `///` only on an item another module calls, one sentence, starting with the item's name.

**This crate (`CLAUDE.md`):**
- `unsafe` stays in `src/macos/`, `src/capture/tap.rs` and `src/writer.rs` only. The killpg wrapper goes into `src/macos/mod.rs`; `src/hook.rs` is safe code.
- Tests live inline in `#[cfg(test)] mod tests` at the bottom of the file they cover.
- No `#[allow(dead_code)]`. An item with no caller is unfinished work.
- Every new module file is declared with `mod <name>;` in its parent.

**Per-task gate:**
1. `mise run check` (fmt check, clippy `-D warnings` on all targets, tests) is green.
2. `! grep -rn 'unsafe' src --include='*.rs' | grep -vE '^src/(macos/|capture/tap\.rs|writer\.rs)'` passes.
3. Only after 1 and 2: mark the task's checkboxes.

## Testing Strategy

- **Unit tests**: required for every task. The thread loop is driven with a fake runner and a fake clock so no test sleeps for a real backoff; process spawning is tested against the real `/bin/sh` with commands that exit immediately or sleep past a sub-second timeout.
- **No e2e suite** in this project. The acceptance run against a real meeting is Post-Completion.

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with a `+` prefix.
- Document issues and blockers with a `!` prefix.
- Update the plan if implementation deviates from the original scope.

## Solution Overview

Three pieces, one new module:

1. **Configuration** - `on_complete` (string, optional; absent means no hook) and `on_complete_timeout_seconds` (integer, default 120, must be positive). Both flat keys in `config.toml`, both shown by `--check-config`.

2. **The sidecar as the delivery ledger** - the sidecar gains `file` (the basename of the completed audio) and `label` (the allow-list label), and, only when a hook is configured, `on_complete`. `LocalFolder` writes `{"state": "pending"}` in the same write that creates the sidecar, so there is no window in which a completed recording exists without its pending mark, then hands the sidecar path to the hook thread. The hook thread rewrites the field to `{"state": "done", "at": "<rfc3339>"}` after the first run that exits 0.

3. **The hook thread (`src/hook.rs`)** - spawned from `main` only when `on_complete` is set, before `session::run`, joined after it. It keeps a queue of sidecar paths: the startup scan of `output_dir/*.json` for `pending` entries seeds it, the sink feeds it live. For each due job it builds `MIMI_RECORDING` and `MIMI_SIDECAR` from the sidecar, runs `/bin/sh -c <command>` in `output_dir` in its own process group, polls the child until it exits or the timeout expires, kills the whole group on expiry, and classifies the result. Exit 0 marks the sidecar done; anything else schedules a retry at 60 s doubling to a 30 min cap. When `Shutdown` is raised the thread kills any in-flight child and exits without touching the queue - every unfinished job is still `pending` on disk.

The run loop is untouched except for carrying `label` into `Recording`. `Sink::accept` keeps its signature; `LocalFolder` gains the sender and does the enqueue itself, so the session tests and `FakeSink` do not change.

## Technical Details

### Hook contract (what a command can rely on)

- Invocation: `/bin/sh -c <on_complete>`, current directory `output_dir`, the daemon's own environment plus `MIMI_RECORDING` (absolute path of the completed audio: the `.m4a`, or the `.aac` when the remux failed and the ADTS file was kept) and `MIMI_SIDECAR` (absolute path of the sidecar). stdin is `/dev/null`; stdout and stderr are inherited from the daemon, so under the LaunchAgent they land in `~/Library/Logs/mimi.err.log` and `mimi.log`.
- Exit 0 means delivered: the sidecar is marked done and the command is never run again for that recording. Any other exit, a signal, a spawn failure or the timeout means retry later.
- The command must be idempotent: a run that succeeded after mimi lost track of it (a `kill -9` between the child's exit and the sidecar rewrite) is run again at the next start.
- The environment under launchd is minimal (`/usr/bin:/bin:/usr/sbin:/sbin`); a command that needs anything else names it by absolute path.

### Sidecar

Two fields added unconditionally, one conditionally:

```json
"file": "2026-09-02T16-20-54-teams2.m4a",
"label": "teams2",
"on_complete": {"state": "pending"}
```

`on_complete` is present only when the sink was built with a hook sender. `state` is `pending` or `done`; `done` carries `at`, RFC3339 with seconds like `started_at`. In `src/sink.rs`:

```rust
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum OnComplete { Pending, Done { at: String } }
```

`Sidecar` gets `file: String`, `label: String`, `on_complete: Option<OnComplete>` with `skip_serializing_if = "Option::is_none"`. The existing test that pins the sidecar at exactly ten fields becomes twelve without a hook and thirteen with one.

### Configuration

`Config` gains `on_complete: Option<String>` and `on_complete_timeout_seconds: u32` (`DEFAULT_ON_COMPLETE_TIMEOUT_SECONDS = 120`, validated through the existing `positive`). An `on_complete` that is present but blank after trimming is a `ConfigError::BlankOnComplete` startup error, matching how `output_dir` refuses blanks. `Display` prints `on_complete_timeout_seconds` always and `on_complete = "<command>"` only when set.

### `src/hook.rs`

Public surface (everything else private):

```rust
pub struct HookSettings { pub command: String, pub timeout: Duration, pub output_dir: PathBuf, pub retry_base: Duration, pub retry_cap: Duration }
pub struct Job { pub sidecar: PathBuf, pub recording: PathBuf }
pub enum Outcome { Done, Failed(Failure) }
pub enum Failure { Exited(i32), Signaled, TimedOut, NotStarted(io::Error) }
pub trait Runner { fn run(&self, job: &Job) -> Outcome; }
pub struct ShellRunner { /* command, timeout, cwd */ }
pub fn scan(dir: &Path) -> Vec<Job>
pub fn job_for(sidecar: &Path) -> Option<Job>
pub fn mark_done(sidecar: &Path, at: DateTime<Local>) -> Result<(), HookError>
pub fn retry_after(attempts: u32, base: Duration, cap: Duration) -> Duration
pub fn spawn(settings: HookSettings, jobs: Receiver<PathBuf>, shutdown: Shutdown) -> Hook
impl Hook { pub fn join(self) }
```

Decisions the module implements:

- `job_for` reads the sidecar, deserializes only `file` and `on_complete` (unknown fields ignored), and yields a `Job` when `on_complete` is `Pending` and `dir/<file>` exists. A sidecar without the field, a `done` one, an unparsable one, or a pending one whose audio is missing yields `None`; the last two are logged at warn with the path. `scan` applies `job_for` to every `*.json` in `dir`, sorted by file name so older recordings go first, and never fails: an unreadable directory is a warn and an empty result.
- `ShellRunner::run` spawns `sh -c` with `Command::process_group(0)` (std, stable) so the child is its own group leader; polls `try_wait` every 200 ms; on the deadline calls `macos::kill_process_group(child.id())` then `wait` to reap, and returns `TimedOut`. A child that exits with a signal is `Signaled`; a spawn error is `NotStarted`.
- `retry_after(attempts, base, cap)` is `min(base * 2^(attempts - 1), cap)` with `attempts >= 1`; `main` passes 60 s and 30 min, tests pass milliseconds.
- The loop (`fn run(settings, jobs, shutdown, runner, clock)`, private, tested directly): keeps a `Vec` of `(Job, attempts, due)`; each iteration drains new sidecar paths from the channel without blocking, runs the earliest due job, applies the outcome (`Done` -> `mark_done` and drop; `mark_done` failing is logged and the job is retried like a hook failure, since a done that is not on disk is not done), otherwise reschedules with `retry_after`; with nothing due it waits on the channel until the next due time or 200 ms, whichever is sooner; it exits when `shutdown.requested()` or when the channel is disconnected. Time comes from an injected clock (`Fn() -> Instant`) so the tests control it. The in-flight child is killed on shutdown by `ShellRunner` observing the same `Shutdown` flag inside its poll loop, so the join in `main` is bounded by one poll tick, not by the hook timeout.
- `mark_done` rewrites the sidecar by reading it into a `serde_json::Value` object, setting `on_complete`, serializing pretty like the sink does, writing to `<sidecar>.tmp` at mode 0600 and renaming over the original. Every other field survives untouched. A sidecar that is not a JSON object is a `HookError`.

### `src/macos/mod.rs`

`pub fn kill_process_group(pid: u32)` - the one-line safe wrapper around `libc::killpg(pid, SIGKILL)` in the file that already holds the crate's libc calls. The return value is ignored: the only failure is a group that is already gone.

### Composition (`src/main.rs`)

`run()` destructures the two new keys. When `on_complete` is `Some`, it creates the channel, calls `hook::spawn` with `HookSettings { command, timeout: Duration::from_secs(on_complete_timeout_seconds), output_dir: output_dir.clone(), retry_base: 60 s, retry_cap: 30 min }` and a clone of `shutdown`, and builds `LocalFolder::new(Some(sender))`; otherwise `LocalFolder::new(None)`. After `session::run` returns the sender goes out of scope with the sink and `Hook::join` is called before the poller's stop sender is dropped.

### Processing flow

```
session ends -> finish_session -> LocalFolder::accept
  rename .partial -> remux -> sidecar (on_complete: pending when hooked) -> send sidecar path
hook thread: scan(output_dir) seeds the queue at start; channel feeds it live
  due job -> sh -c <command> (MIMI_RECORDING, MIMI_SIDECAR, cwd output_dir, own process group)
    exit 0   -> mark_done (tmp + rename) -> job dropped
    anything -> log, due = now + retry_after(attempts)
  shutdown -> kill in-flight group, exit; pending jobs stay pending on disk
```

## Implementation Steps

### Task 1: Configuration keys for the hook

**Files:**
- Modify: `src/config.rs`

- [x] add `on_complete: Option<String>` and `on_complete_timeout_seconds: u32` to `Config`, the two keys to `ConfigFile`, `DEFAULT_ON_COMPLETE_TIMEOUT_SECONDS = 120`, and `ConfigError::BlankOnComplete`
- [x] parse them in `from_toml`: trim the command and refuse a blank one, run the timeout through `positive`
- [x] extend `Display` (timeout always, command only when set) and the destructuring in `missing_file_yields_defaults`
- [x] write tests: defaults with no file (`None`, 120), both keys parsed, blank `on_complete` rejected, zero and negative timeout rejected, `Display` with and without a command, an unknown key still rejected
- [x] run `mise run check` - must pass before task 2
- + `src/main.rs` destructures the two new keys as `_` so the crate keeps compiling; task 6 replaces that with the real wiring

### Task 2: Sidecar carries file, label and the pending mark

**Files:**
- Modify: `src/sink.rs`
- Modify: `src/session/mod.rs`
- Modify: `src/main.rs`

- [x] add `label: String` to `Recording` and to `Session`, filled from `SessionStart` in `start_session` and passed on in `finish_session`; update the session tests' `Recording` destructuring and `FakeSink` usage
- [x] add `OnComplete` (tagged enum, `Serialize` and `Deserialize`) and the `file`, `label`, `on_complete` fields to `Sidecar`; `sidecar()` takes the completed audio path to fill `file`
- [x] turn `LocalFolder` into a struct holding `Option<Sender<PathBuf>>` with `LocalFolder::new`; when the sender is present the sidecar is written with `Pending` and its path is sent afterwards; a send error is a warn, not an `accept` failure
- [x] switch `main.rs` to `LocalFolder::new(None)` so the crate compiles and behaves as before
- [x] write tests: a sidecar without a hook has exactly twelve fields and no `on_complete`; with a hook it has thirteen and `{"state": "pending"}`; `file` is the m4a name after a successful remux and the aac name when the remux is skipped; `label` is carried; `accept` delivers the sidecar path on the channel; a dropped receiver does not fail `accept`; `OnComplete` round-trips both variants through serde_json
- [x] run `mise run check` - must pass before task 3
- + the m4a-name test encodes a real ADTS file through `writer::spawn` first, because `remux::to_m4a` only succeeds on audio Core Audio can read
- + `on_complete` is built with `hook.as_ref().map(...)` rather than a `match`: clippy's `manual_map` is denied by `-D warnings`

### Task 3: Running one hook: `ShellRunner` and the process-group kill

**Files:**
- Create: `src/hook.rs`
- Modify: `src/main.rs` (declare `mod hook;`)
- Modify: `src/macos/mod.rs`

- [x] add `macos::kill_process_group(pid: u32)` beside `user_id`, keeping `src/hook.rs` free of `unsafe`
- [x] create `src/hook.rs` with `Job`, `Outcome`, `Failure`, `Runner`, `ShellRunner` (command, timeout, cwd, a `Shutdown` clone) and `ShellRunner::run`: `sh -c`, the two `MIMI_*` variables, `Stdio::null` for stdin and inherited stdout and stderr, `process_group(0)`, a 200 ms `try_wait` poll bounded by the timeout, group kill and reap on expiry or on shutdown
- [x] write tests against the real `/bin/sh`: `exit 0` is `Done`; `exit 3` is `Failed(Exited(3))`; `sleep 30` with a 300 ms timeout is `Failed(TimedOut)` and returns within about one poll tick after the deadline; a nonexistent shell path is `NotStarted`; the child sees `MIMI_RECORDING` and `MIMI_SIDECAR` and runs in the configured cwd (probe with a command that writes those into a file in the temp dir); a raised `Shutdown` ends a sleeping child early
- [x] run `mise run check` and the unsafe grep - must pass before task 4
- + `mod hook;` is declared `#[cfg(test)]` and `macos::kill_process_group` carries the same attribute, because clippy `-D warnings` denies `dead_code` on the non-test bin target and nothing in `main` reaches the module until task 6, where both attributes go away. `#[allow(dead_code)]` is forbidden, so the module is simply not compiled into the binary yet - the same shape `src/plist.rs` already has
- + `NotStarted` is reached with a nonexistent working directory rather than a nonexistent shell path: the shell is the fixed `/bin/sh` const the contract names, and a `shell` field would be configurability the plan does not ask for
- + a raised `Shutdown` returns `Failed(Signaled)`, not `Failed(TimedOut)`: the child was killed, its deadline was not reached, and the loop that raised the flag discards the outcome anyway
- + the timeout test runs `sleep 31337 & sleep 31337` and asserts through `pgrep` that neither survives, so the process-group kill is measured rather than assumed
- + `Shutdown::request` (already `#[cfg(test)]`) became `pub` so `src/hook.rs`'s tests can raise the flag

### Task 4: The sidecar ledger: `job_for`, `scan`, `mark_done`, `retry_after`

**Files:**
- Modify: `src/hook.rs`

- [x] implement `job_for` (deserialize `file` and `on_complete` only, require `Pending` and an existing audio file, warn on the two skip cases named in Technical Details) and `scan` (every `*.json` in the directory, sorted by name, unreadable directory is a warn and an empty vector)
- [x] implement `mark_done` as read into `serde_json::Value`, set `on_complete`, pretty-serialize, write `<sidecar>.tmp` at mode 0600, rename over the original; non-object JSON is `HookError`
- [x] implement `retry_after`
- [x] write tests: `scan` returns only pending sidecars with audio present, in name order, and ignores a sidecar with no `on_complete`, a done one, a pending one whose audio is missing, and a file that is not JSON; `mark_done` flips the state, sets `at`, preserves every other field byte-equal after re-parsing, keeps mode 0600 and leaves no `.tmp` behind; `mark_done` on a JSON array is an error; `retry_after` for attempts 1, 2, 3 and past the cap
- [x] run `mise run check` - must pass before task 5
- + `sink::write_private` and `sink::SIDECAR_EXTENSION` became `pub` rather than being duplicated in `src/hook.rs`: the 0600 mode of a sidecar and the extension that identifies one are the sink's policy, and a second copy would be a second place to get them wrong
- + `HookError` also carries `Read` and `Describe`: `mark_done` reads and re-serializes before it writes, and a sidecar that vanished between the run and the rewrite must reach the loop as a failure rather than a panic
- + `mark_done` removes its `<sidecar>.tmp` when the rename fails, so a failed rewrite leaves the directory as it found it

### Task 5: The hook thread: queue, backoff, shutdown

**Files:**
- Modify: `src/hook.rs`

- [x] implement the private loop `run(settings, jobs, shutdown, runner, clock)` per Technical Details, plus `spawn` (seeds the queue with `scan(output_dir)`, builds `ShellRunner`, spawns the thread, returns `Hook`) and `Hook::join`
- [x] write tests with a fake `Runner` that records the jobs it was given and returns scripted outcomes, a fake clock the test advances, and a temp `output_dir` holding real sidecar files: a sidecar sent on the channel is run once and marked done; a failing run is retried after `retry_base`, then `2 * retry_base`, and is marked done on the run that succeeds; two pending sidecars present at start are run in name order before a live one; a raised `Shutdown` ends the loop with an unmarked job still `pending` on disk; a disconnected channel ends the loop; a `mark_done` failure (make the sidecar unwritable or replace it with an array) leads to a retry rather than a silent drop
- [x] run `mise run check` - must pass before task 6
- + the `scan` seeding happens inside `run`, not in `spawn`: the loop is the tested surface, and a test that drives it with a fake runner and a fake clock must be able to seed the queue from its own temp `output_dir` without going through the real `ShellRunner`. `spawn` still owns the composition (settings -> `ShellRunner` -> thread)
- + `enqueue` ignores a sidecar path already in the queue, so the startup scan and a live send of the same recording cannot deliver it twice
- + the `mark_done` failure is provoked by sealing the output directory at mode 0500 rather than replacing the sidecar with an array: an array never becomes a job in the first place (`job_for` cannot read it), so it never reaches the loop
- + `spawn` and `Hook::join` are covered by their own test against the real `/bin/sh`; with `mod hook;` still `#[cfg(test)]` (task 6 removes it), an uncalled `spawn` would be dead code under clippy `-D warnings`

### Task 6: Wire the hook into the daemon

**Files:**
- Modify: `src/main.rs`

- [ ] drop the `#[cfg(test)]` from `mod hook;` in `src/main.rs` and from `macos::kill_process_group`
- [ ] in `run()`, when `on_complete` is set: create the channel, `hook::spawn` with the settings from Technical Details and a clone of `shutdown`, `LocalFolder::new(Some(sender))`; otherwise `LocalFolder::new(None)`
- [ ] join the hook after `session::run` returns; the sink (and with it the sender) must be dropped before the join so a loop waiting on the channel also ends when shutdown was not raised
- [ ] confirm `mimi --check-config` prints the two keys through the existing `Display`
- [ ] this task has no unit-testable surface of its own; run the full suite and the unsafe grep, and confirm `cargo build --release` produces a binary whose `--check-config` output matches a config file with and without `on_complete`
- [ ] run `mise run check` - must pass before task 7

### Task 7: Verify acceptance criteria

- [ ] verify every property in Overview is implemented and every non-goal is absent: no network crate in `Cargo.toml`, no sidecar without a hook carries `on_complete`, no scan result for a sidecar without the field
- [ ] verify the edge cases: remux failure keeps the aac and `file` names it; a sidecar whose audio was deleted is skipped with one warn per scan; a hook that exits 0 after mimi was killed is run again at the next start (documented as the idempotency requirement)
- [ ] run the full test suite: `mise run check`
- [ ] run the unsafe grep from `CLAUDE.md`
- [ ] confirm every new module is declared with `mod` and no `#[allow(dead_code)]` was added

### Task 8: Update documentation

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

- [ ] README: add `on_complete` and `on_complete_timeout_seconds` to the configuration table; add a "Completion hook" section stating the contract from Technical Details (invocation, environment, exit codes, retry and backoff, the `on_complete` sidecar field, the startup scan, the idempotency requirement, the launchd environment note); extend the sidecar example with `file`, `label` and `on_complete`
- [ ] CLAUDE.md: add `src/hook.rs` to the module layout; add a "Completion hook" section carrying the reasoning that must not become comments: why stdio is inherited rather than piped, why the child runs in its own process group and is killed through `macos::kill_process_group`, why the pending mark is written in the same write as the sidecar, and why the scan looks at `pending` only
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

**Manual verification:**
- Set `on_complete` to a command that records its environment to a file and exits 0, run `mimi run` in the foreground, hold a short meeting: the sidecar shows `pending` immediately and `done` with `at` after the command ran; `MIMI_RECORDING` and `MIMI_SIDECAR` point at the right files.
- Repeat with a command that exits 1: the log shows the retry schedule; stop mimi with Ctrl-C; the sidecar is still `pending`; start mimi again and watch the scan pick it up.
- Repeat with `sleep 600` and a short timeout: the group is killed on expiry and no `sleep` survives in `ps`.
- Install the release through the usual signed build and `mimi install`; confirm the LaunchAgent inherits the minimal PATH and the configured command still resolves everything it needs by absolute path.

**External system updates:**
- The command that turns a sidecar into a message for the transcription queue lives outside this repository and is delivered separately. Its contract is the one in Technical Details; nothing here changes when it does.
- The cask's `postflight` already re-runs `mimi install` on upgrade, so the daemon picks up the new binary and the new config keys without further steps.
