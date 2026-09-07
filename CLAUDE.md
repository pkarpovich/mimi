# CLAUDE.md

Conventions this crate is built under. Everything here was established by measurement on this machine during planning, or is a rule that keeps the measured behaviour from being undone.

## Module layout

```
src/main.rs             CLI entry: run | install | uninstall | --check-config
src/plist.rs            Info.plist template rendering (reached by build.rs through include!)
src/config.rs           TOML config, defaults, validation
src/activity/mod.rs     ActivitySource and DeviceSource traits, AudioProcess, ActivityEvent
src/activity/poller.rs  the polling loop, device-change sampling
src/activity/diff.rs    pure snapshot diffing
src/session/mod.rs      session lifecycle, orchestration, the run loop
src/session/decide.rs   pure allow-list matching, label, state machine
src/capture/mod.rs      Capture trait, TrackKind, CaptureConfig, ring producer handle, Formats
src/capture/tap.rs      tap + aggregate lifecycle, IOProc, rebuild                   (unsafe)
src/capture/layout.rs   pure AudioBufferList -> tracks interpretation
src/capture/ring.rs     lock-free hand-off from the IOProc to the writer thread
src/capture/silence.rs  pure all-zero detection over a window
src/capture/devices.rs  pure rebuild decision over devices and rate
src/writer.rs           ExtAudioFile ADTS AAC writer, stereo fold, resampling        (unsafe)
src/remux.rs            ADTS -> m4a packet copy at completion
src/sink.rs             Sink trait, local-folder implementation, sidecar JSON
src/hook.rs             completion-hook thread: sidecar ledger, shell run, retry
src/executable.rs       the running binary's identity, and the watch that retires the daemon when it is swapped
src/service.rs          launchd agent install/uninstall
src/instance.rs         the advisory single-instance lock
src/macos/mod.rs        raw Core Audio property helpers                              (unsafe)
src/macos/audiofile.rs  AudioFile open/create/packet copy for the remux              (unsafe)
```

## Unsafe containment

`unsafe` lives in exactly three places: `src/macos/`, `src/capture/tap.rs` and `src/writer.rs` - the code that touches Core Audio and AudioToolbox directly. Nothing above them sees a raw pointer. The check is a grep, and it is part of the definition of done:

```sh
! grep -rn 'unsafe' src --include='*.rs' | grep -vE '^src/(macos/|capture/tap\.rs|writer\.rs)'
```

Consequences worth knowing before reaching for a fourth location: `libc::getuid` is wrapped as `macos::user_id()` so `src/service.rs` stays safe, and `src/capture/ring.rs` is a lock-free SPSC ring written in *safe* Rust over atomic indices with samples held as `AtomicU32` bit patterns, rather than the obvious unsafe implementation.

## Each buffer is a track; channels live inside it

The IOProc's `AudioBufferList` is interpreted by `capture::layout::interpret`, and the rule is measured, not assumed:

- Two buffers means buffer 0 is the microphone (1 channel) and buffer 1 is the tap (2 channels),
with the same frame count.
- One buffer means no input device is in the composition: system audio only, no microphone.
- A buffer with more channels than the track needs is mixed down by averaging.
- Frame counts that disagree between buffers are truncated to the shorter one.
- A track is identified by its **position** in the list, never by which entries happen to carry
samples. A buffer that arrives with no channels or no data leaves its own track empty; it does not promote the other buffer into its place. Ranking the non-empty entries instead puts the microphone on the system track whenever the tap delivers an empty buffer, and `writer::fold` then swaps the channels for those blocks.

Reading channels *within* a buffer as if they were the two tracks produces a file where the sources are concatenated rather than separated. That mistake was made once during the spike; there is a test named for it. Do not "simplify" the layout code back into channel indexing.

## The delivered sample rate comes from the aggregate, never from the tap

Read `kAudioDevicePropertyNominalSampleRate` **on the aggregate device**. `kAudioTapPropertyFormat` reports 48000 Hz regardless of the truth - on AirPods Pro, where the stream is really 24000 Hz, it still says 48000, and a file written from that plays at double speed.

The rate is not a constant and must not be read once. It changes when the default device changes, and it changes when a device switches mode without changing identity: opening the microphone forces AirPods into headset mode, same UID, different rate.

Two rates exist and they are not interchangeable. The rate the **writer** works from is the aggregate's, read after every build and published per generation through `Formats`. The rate the **rebuild decision** works from is the default *input* device's, sampled by `activity::poller` - there is no aggregate at poll time, and it is the input device that changes rate when AirPods enter headset mode. `rebuild_needed` therefore compares a poller sample against a poller sample: `Tap`'s baseline is the `Devices` its last successful build was handed, never `Tap::sample_rate()`. Seeding one side of that comparison from the aggregate makes it meaningless.

Every device sample the run loop holds comes from the poller, including the first one. The poller announces its own baseline as a `DevicesChanged` before its first pass, and `main` starts the loop from an unset `Devices` rather than sampling Core Audio itself. A sample taken on another thread before the poller existed can already be stale by the time the poller sets its baseline, and nothing would ever correct it: the poller emits a change, not a state, so a change inside that window is one the loop never hears about, and the first session would be built on devices that are gone.

## `tapautostart` is 0

`kAudioAggregateDeviceTapAutoStartKey` is a start gate, not a convenience. Measured twice: with the key set to 1 and an aggregate whose only source is the tap, no buffer ever arrives while nothing is playing - `AudioDeviceStart` returns 0 and the IOProc never fires. With the key set to 0 and the same composition, the first buffer arrives after 55 ms. Setting it to 1 makes capture depend on somebody else producing audio first, which is exactly the wrong dependency for a recorder.

The rest of the aggregate description matters too: sub-device and tap entries are **dictionaries** (`{"uid": ...}`, plus `{"drift": 1}` on the microphone), not bare UID strings. An array of bare UUID strings is accepted by the API and yields a tap that contributes nothing.

The teardown sequence is `AudioDeviceStop`, `AudioDeviceDestroyIOProcID`, `AudioHardwareDestroyAggregateDevice`, `AudioHardwareDestroyProcessTap`, in that order, in one helper that the stop path, the rebuild path and `Drop` all call.

## A device change rebuilds capture without closing the file

Measured: when the default device changes mid-recording, the IOProc keeps firing - its rate even doubles - but the data becomes first duplicated across both tracks and then entirely zero. Callbacks alive, counters healthy, file silent. So a device change is not something to tolerate; it must rebuild the tap and the aggregate.

The file and the writer thread stay open across that rebuild. The recording is one artifact, and the gap is the settle delay. The captured-device baseline is updated only after a successful start, so a failed attempt is retried (three attempts, 250 ms apart) rather than suppressed. On exhaustion the session keeps its file and the sidecar records the failure.

`devices::rebuild_needed` takes the rate as an input alongside the two device UIDs, because AirPods change rate without changing UID. It also takes an `Io`, because a rebuild that exhausted its attempts leaves the baseline pointing at devices no capture is running on any more: without that input, devices that returned to what the session started on would be judged unchanged and the recording would stay silent for the rest of the meeting.

That `Io` input is only reachable if somebody asks again, and the poller emits `DevicesChanged` on a change, not on a state. So the run loop owns a `Recovery`: capture it wanted but does not have, retried every `RECOVERY` seconds regardless of which event woke the loop. It covers both directions the same way - a rebuild that exhausted its attempts, and a session start that failed while the meeting app keeps holding the microphone and therefore produces no second `InputTaken` to trigger another attempt. Recovery is throttled rather than run on every tick because a permanently failing rebuild sleeps 750 ms per round, and a permanently failing start would otherwise log five times a second.

## Format generations, and why the writer resamples

The ring's blocks carry a **format generation**, a counter incremented on every capture build. The writer decides what to do from the generation of the block it is about to write, so blocks captured at the old rate are handled at the old rate. Without that, blocks queued at 24000 Hz would be converted as if they were 48000 Hz at the exact moment the default device changes.

`ExtAudioFile` refuses a *different* client format once writing has started - probed directly: the first `kExtAudioFileProperty_ClientDataFormat` succeeds, re-applying the same rate succeeds, but a different rate returns `-66565` (`kExtAudioFileError_InvalidOperationOrder`) and every later write fails too. So the client format is established by the first block and stands for the life of the file; a later generation captured at another rate is **resampled onto it** instead. The silence detector and the resampler both reset on a generation change, which is the rebuild boundary the writer can see.

Two formats, not one. The **file** format is AAC at `config.sample_rate`, set once at creation - that is the rate the sidecar reports and the rate the file plays at. The **client** format is float PCM at the rate the aggregate delivered for the first block written, and `ExtAudioFile` converts between the two. Only the client format is bound to a generation.

The `ExtAudioFileRef` is created, used and closed only on the writer thread. The rebuild path reaches the writer solely by pushing blocks with a new generation.

## The AAC bit rate is a three-step sequence

Order matters: set the client format first (that is what instantiates the codec), read the converter out with `kExtAudioFileProperty_AudioConverter` and set `kAudioConverterEncodeBitRate` on it, then commit by setting `kExtAudioFileProperty_ConverterConfig` to a **pointer-sized NULL**. Skipping the commit makes the setting silently do nothing; passing a `UInt32` zero instead of a null pointer crashes the writer thread.

## Real-time discipline

The IOProc runs on a real-time thread and must not allocate, lock, block or touch the filesystem. It interprets the buffer list into a scratch the block owns, copies both tracks into the preallocated ring, and returns. The writer thread drains the ring and does the fold, the encode and the write. Writer errors go into a shared slot, read after the thread joins - never a panic on that thread.

## The two-track seam

Microphone and system audio stay separate concepts from the IOProc all the way to the writer. The ring carries them apart. They are folded into interleaved stereo in exactly one place, `writer::fold`: left is the microphone, right is the system mixdown, and when the microphone track is absent its channel is silence rather than a copy of the system track.

## The completion hook delivers from the sidecar, not from memory

`on_complete` is one shell command run for every recording that completes while it is configured, until a run exits 0. The command is opaque; what mimi owes it is delivery, and the reasoning behind how that delivery is arranged is here rather than in comments.

**The sidecar is the only state.** There is no queue file, no marker file, no in-memory list that has to survive anything. `LocalFolder::accept` writes `"on_complete": {"state": "pending"}` in the same write that creates the sidecar - not a second write afterwards - so no completed recording ever exists on disk without its pending mark, and a `kill -9` between the two writes is a window that does not exist. The hook thread rewrites the field to `done` with an `at` after the run that succeeded, through a `<sidecar>.tmp` at mode 0600 and a rename, so a crash mid-rewrite leaves the sidecar as it was: still pending, delivered again later. `write_private` fsyncs before it returns, so the rename cannot publish a name whose contents never reached the disk. That is why the command has to be idempotent - a hook that exited 0 and then lost the rewrite is run again at the next start. It is also why a rewrite that fails because the sidecar is *gone* ends the job rather than retrying it: the sidecar is the state, so a command that moved or deleted its own `MIMI_SIDECAR` after exiting 0 has left nothing pending, and the next start's scan would not find it either. Retrying that one on a timer is the one way this design can run a command forever - every 30 minutes at the cap, over a recording no ledger entry names any more.

**The startup scan looks at `pending` only.** A sidecar with no `on_complete` field is not the hook's business: it was written before the hook was configured, and enabling a hook must not publish the archive retroactively. `Ledger.file` is `Option<String>` for the same reason - a sidecar from before this feature carries no `file` either, and it has to be passed over silently rather than warned about as unparsable. The cases that do warn are a sidecar that cannot be read or parsed at all, and a pending one that names no recording or whose audio is gone. An `output_dir` that does not exist yet is not one of them: `session::start_session` creates it on the first meeting, and the hook thread is spawned before that, so `scan` treats `NotFound` as an empty directory rather than warning on every start until the first recording exists.

**A sidecar names its own recording, and nothing else.** `file` is read back off the disk, so `job_for` accepts it only when it is exactly one normal path component and joins it beside the sidecar itself; `../`, an absolute path and an empty name are a warn and no job. It also has to be the recording *this* sidecar was written for - `<stem>.m4a` or `<stem>.aac` for the sidecar's own `<stem>.json`, which is the only pair `LocalFolder::accept` ever writes. Without that binding a sidecar is a pointer at any regular file beside it, and a planted `evil.json` naming a recording whose own sidecar carries no `on_complete` hands the command a recording the hook was never meant to see - the retroactive publishing the pending-only scan exists to prevent. Bound to the stem, the name is unforgeable where it matters: mimi's sidecar already occupies the one name a planted one would need. The recording also has to *be* a recording where it stands, which is why the check is `symlink_metadata` and a regular file rather than `exists`: `exists` follows the link, so a symlink planted beside the sidecar is a path component that passes the first test and still hands the command a file outside `output_dir`. That check is shape, not a lock - mimi never opens the recording, the command does, so nothing mimi stats can bind what the command later resolves. `write_private` guards the same seam from the other side: `mode` and `truncate` describe a file the call *creates*, so a name that is already taken - a stale `<sidecar>.tmp` left by a crash - is opened `O_NOFOLLOW` and has its mode set on the descriptor rather than inherited. Neither is reachable while `output_dir` is the 0700 directory mimi creates; both are what keeps an `output_dir` that already existed, with a mode mimi never chose, from turning the hook into somebody else's errand boy.

**And none of that can make a shared directory safe, so `on_complete` refuses one.** Every check above is about the *shape* of what a sidecar names, and shape is exactly what somebody who can write into `output_dir` gets right for free: a planted `<stem>.json` marked pending beside a planted `<stem>.m4a` is a well-formed pair, and the file the command opens is resolved after mimi stopped looking at it. So `hook::claim_private` is called in `main` before the thread is spawned - `output_dir` must be owned by the daemon user, carry no group or other write bit, and hand nothing out through an extended ACL, or `on_complete` is a startup failure rather than a hook. It **creates** the directory at `RECORDINGS_DIR_MODE` first, and that is what makes the rest of it mean anything: a directory that is merely absent is one another local user can create in the window before the first meeting, and `session::start_session`'s recursive create accepts a directory that already exists rather than making its own. Creating it here leaves two cases and no third - either mimi made it, or it was already there and is judged. The ACL is checked because the mode bits do not show one: `chmod +a "everyone allow write"` leaves `mode() & 0o022 == 0` and still lets anybody plant a pair, so `macos::acl_grants_write` walks the extended ACL and refuses any *allow* entry carrying `add_file`, `add_subdirectory`, `delete_child`, `writesecurity` or `chown`. Deny entries pass, which is what keeps the `group:everyone deny delete` that every macOS home directory carries from being read as an exposure. It is a gate on the hook alone, not on recording: mimi's own files are 0600 either way, and the hook is the one thing that hands whatever is in that directory to a command. What it accepts it **answers with**, canonicalized, and `main` runs both the hook thread and the session off that answer rather than off the configured path: every check here follows the links on the way to the directory, so judging through a link and then following it a second time judges one directory and delivers from another. A configured `output_dir` under a directory somebody else can write is a link they can repoint between the two, and pointing it at a private directory of the daemon's own long enough to pass is free. Resolving once removes the second lookup, and with it that window.

The resolved path is then walked to the root, because judging the leaf alone judges a name somebody else can move out from under it. Replacing `output_dir` with a directory of one's own takes no access to `output_dir` at all - it takes write access to whatever holds it, and a rename. So `stable` requires every ancestor to be owned by the daemon user or by root and closed to everyone else, and `private` stays the stricter check for the leaf: the leaf is where a pair is planted, so a mode others can write is fatal there even under a sticky bit, while an ancestor that is sticky is safe however open it is - sticky is exactly the rule that only the owner of an entry may rename it, and every entry on this chain is one this walk already proved is the daemon's or root's. Root is trusted because a root that plants recordings has no need of this hook. The ACL check applies to the ancestors too and for the same reason it applies to the leaf: `chmod +a "everyone allow delete_child"` leaves `mode() & 0o022 == 0` and still hands out the rename. The gate is a startup judgement, not a lock held over the run - a directory whose mode changes afterwards is beyond what any check in this process can promise.

**The child inherits stdout and stderr; it is never piped.** A pipe would have to be drained by the same thread that is enforcing the timeout, and a child that fills the pipe buffer while that thread is sleeping between `try_wait` polls is a deadlock. Inheriting the daemon's streams keeps the thread free of reading, and under launchd the command's stderr lands in the same log as mimi's own events while its stdout goes to `mimi.log`, which is otherwise empty.

**The child is its own process group and is killed as one.** `Command::process_group(0)` makes it the group leader, and on the timeout - or on shutdown - `macos::kill_process_group` sends `SIGKILL` to the group, then the child is reaped. Killing the `sh` alone would leave whatever it spawned running, so the test that covers the timeout runs `sleep 31337 & sleep 31337` and asserts through `pgrep` that neither process is left behind. The wrapper lives in `src/macos/mod.rs` next to `user_id()` so `src/hook.rs` stays free of `unsafe`.

**Shutdown drops the queue, not the work.** `ShellRunner` watches the same `Shutdown` flag inside its poll loop, so the join in `main` is bounded by one 200 ms tick rather than by `on_complete_timeout_seconds`. Nothing is flushed on the way out: every job that was not marked done is still `pending` on disk, and the next start's scan finds it.

## The agent runs the executable inside the bundle

`service::install` writes the plist with the running executable's path **after `fs::canonicalize`**. The cask links `/opt/homebrew/bin/mimi` to the binary inside `Mimi.app`, and `mimi install` typed at a shell arrives through that link; without the resolution the plist ran the link, and macOS derives what a process is from the path it was executed by - `NSRunningApplication` for the daemon was nil, Control Center and Privacy & Security showed a generic icon beside "mimi", and the microphone grant was keyed to the link's path rather than to the bundle that the whole app-bundle design exists to make durable. Measured on 26.6.2 before the fix; the same daemon started from the bundle path is the app.

`install` over a loaded agent used to race launchd: `bootout` returns before the service is gone, and a `bootstrap` issued right behind it is refused with `Bootstrap failed: 5: Input/output error`, leaving nothing loaded. `wait_unloaded` polls `launchctl print` on the service until launchd no longer knows it (capped at 5 s) before the bootstrap.

## Upgrades are mimi's own business

The cask never loads or unloads the agent. `mimi install` writes the plist once, with `KeepAlive` as `PathState` on the bundle's own binary rather than `true`: launchd keeps the daemon alive only while `/Applications/Mimi.app/Contents/MacOS/mimi` exists, which is what makes `brew uninstall` end in silence instead of a respawn loop over a missing file. `executable::watch` (a thread started in `main`, polling every 2 s) compares the device and inode of that path with what it found at startup; a swap or a removal - what Homebrew does on upgrade and uninstall - raises `Shutdown::retire`, and the run loop ends as soon as no session is open, so a recording in progress is finished rather than cut. launchd then starts whatever binary is at the path, which after an upgrade is the new version.

Why not the cask: an `uninstall launchctl:` stanza runs on every upgrade too, which is how 0.1.1 lost its daemon on each `brew upgrade` (#6 answered that with a `postflight` calling `mimi install`, now deprecated by Homebrew 6). `postflight_steps`, the replacement, runs in a sandbox that substitutes `HOME` and refuses launchd: measured on brew 6.0.22 twice - once with the fake home, once with the real home declared writable and `HOME` restored through `env:` - `launchctl bootstrap` answers `Bootstrap failed: 5` and the failed step rolls the whole install back. No cask in Homebrew/homebrew-cask loads a launch agent from steps; the ones that ship daemons let a `.pkg` or the app itself do it.

## Code style

- No comments. A comment is justified only when the *why* cannot be recovered from the code - a
hidden invariant, a workaround, surprising platform behaviour. This file carries the reasoning.
- `///` on an item another module calls, and only when the name does not already say it. It starts
with the item's name and is one sentence. No `# Examples` sections.
- `for` loops with mutable accumulators over iterator combinator chains.
- `let ... else` to exit early, keeping the happy path unindented. `if let` only when the branch is
short and there is no else.
- Shadow variables through transformations; no `raw_`, `parsed_`, `trimmed_` prefixes.
- Newtypes over meaningful strings (`BundleId`, `BundlePrefix`, device UIDs), enums over `bool`
parameters.
- Match all variants explicitly - no `_ =>` arms, no `matches!` - and destructure structs and tuples
explicitly, so adding a variant or a field is a compiler error.
- Tests live inline in a `#[cfg(test)] mod tests` block at the bottom of the file they cover.
- No `#[allow(dead_code)]`, blanket or otherwise. Dead code means a module was written and never
wired up; if an item has no caller yet, the work that created it is not finished.

## Testing

The unsafe Core Audio surface cannot be unit tested. The answer is not "no tests" but pushing the decisions out of the unsafe code into pure functions that can be: buffer-layout interpretation, event diffing, allow-list matching, silence detection, rebuild decisions, file naming, plist rendering. The unsafe blocks stay thin and are covered by running the daemon against a real meeting.

`session::run` is driven in tests with a fake `ActivitySource`, a fake `Capture` and a fake `Sink`. No test creates a real tap.

`hook::run` is driven the same way, with one addition: it takes its clock as an `impl Fn() -> Instant` and its work as a `Runner`, so a test advances a fake clock instead of sleeping a real backoff. What has no pure core is tested against the real thing - `ShellRunner` runs actual sub-second `/bin/sh` commands and proves the group kill through `pgrep`, and one `sink` test encodes a fixture through `writer::spawn` because `remux::to_m4a` only succeeds on audio Core Audio can read. The seam between the sink that writes a sidecar and the hook that reads it is covered end to end rather than from both sides separately, so the two cannot drift.

Before calling anything done: `mise run check` (fmt, clippy `-D warnings`, tests) is green, the unsafe grep above passes, and every module file is declared with `mod <name>;` in its parent - an undeclared module is not compiled, and neither are its tests.
