# CLAUDE.md

Conventions this crate is built under. Everything here was established by measurement on this
machine during planning, or is a rule that keeps the measured behaviour from being undone.

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
src/sink.rs             Sink trait, local-folder implementation, sidecar JSON
src/service.rs          launchd agent install/uninstall
src/macos/mod.rs        raw Core Audio property helpers                              (unsafe)
```

## Unsafe containment

`unsafe` lives in exactly three places: `src/macos/`, `src/capture/tap.rs` and `src/writer.rs` -
the code that touches Core Audio and AudioToolbox directly. Nothing above them sees a raw pointer.
The check is a grep, and it is part of the definition of done:

```sh
! grep -rn 'unsafe' src --include='*.rs' | grep -vE '^src/(macos/|capture/tap\.rs|writer\.rs)'
```

Consequences worth knowing before reaching for a fourth location: `libc::getuid` is wrapped as
`macos::user_id()` so `src/service.rs` stays safe, and `src/capture/ring.rs` is a lock-free SPSC ring
written in *safe* Rust over atomic indices with samples held as `AtomicU32` bit patterns, rather than
the obvious unsafe implementation.

## Each buffer is a track; channels live inside it

The IOProc's `AudioBufferList` is interpreted by `capture::layout::interpret`, and the rule is
measured, not assumed:

- Two buffers means buffer 0 is the microphone (1 channel) and buffer 1 is the tap (2 channels),
  with the same frame count.
- One buffer means no input device is in the composition: system audio only, no microphone.
- A buffer with more channels than the track needs is mixed down by averaging.
- Frame counts that disagree between buffers are truncated to the shorter one.
- A track is identified by its **position** in the list, never by which entries happen to carry
  samples. A buffer that arrives with no channels or no data leaves its own track empty; it does not
  promote the other buffer into its place. Ranking the non-empty entries instead puts the microphone
  on the system track whenever the tap delivers an empty buffer, and `writer::fold` then swaps the
  channels for those blocks.

Reading channels *within* a buffer as if they were the two tracks produces a file where the sources
are concatenated rather than separated. That mistake was made once during the spike; there is a test
named for it. Do not "simplify" the layout code back into channel indexing.

## The delivered sample rate comes from the aggregate, never from the tap

Read `kAudioDevicePropertyNominalSampleRate` **on the aggregate device**.
`kAudioTapPropertyFormat` reports 48000 Hz regardless of the truth - on AirPods Pro, where the
stream is really 24000 Hz, it still says 48000, and a file written from that plays at double speed.

The rate is not a constant and must not be read once. It changes when the default device changes,
and it changes when a device switches mode without changing identity: opening the microphone forces
AirPods into headset mode, same UID, different rate.

Two rates exist and they are not interchangeable. The rate the **writer** works from is the
aggregate's, read after every build and published per generation through `Formats`. The rate the
**rebuild decision** works from is the default *input* device's, sampled by `activity::poller` -
there is no aggregate at poll time, and it is the input device that changes rate when AirPods enter
headset mode. `rebuild_needed` therefore compares a poller sample against a poller sample: `Tap`'s
baseline is the `Devices` its last successful build was handed, never `Tap::sample_rate()`. Seeding
one side of that comparison from the aggregate makes it meaningless.

## `tapautostart` is 0

`kAudioAggregateDeviceTapAutoStartKey` is a start gate, not a convenience. Measured twice: with the
key set to 1 and an aggregate whose only source is the tap, no buffer ever arrives while nothing is
playing - `AudioDeviceStart` returns 0 and the IOProc never fires. With the key set to 0 and the
same composition, the first buffer arrives after 55 ms. Setting it to 1 makes capture depend on
somebody else producing audio first, which is exactly the wrong dependency for a recorder.

The rest of the aggregate description matters too: sub-device and tap entries are **dictionaries**
(`{"uid": ...}`, plus `{"drift": 1}` on the microphone), not bare UID strings. An array of bare UUID
strings is accepted by the API and yields a tap that contributes nothing.

The teardown sequence is `AudioDeviceStop`, `AudioDeviceDestroyIOProcID`,
`AudioHardwareDestroyAggregateDevice`, `AudioHardwareDestroyProcessTap`, in that order, in one
helper that the stop path, the rebuild path and `Drop` all call.

## A device change rebuilds capture without closing the file

Measured: when the default device changes mid-recording, the IOProc keeps firing - its rate even
doubles - but the data becomes first duplicated across both tracks and then entirely zero. Callbacks
alive, counters healthy, file silent. So a device change is not something to tolerate; it must
rebuild the tap and the aggregate.

The file and the writer thread stay open across that rebuild. The recording is one artifact, and the
gap is the settle delay. The captured-device baseline is updated only after a successful start, so a
failed attempt is retried (three attempts, 250 ms apart) rather than suppressed. On exhaustion the
session keeps its file and the sidecar records the failure.

`devices::rebuild_needed` takes the rate as an input alongside the two device UIDs, because AirPods
change rate without changing UID. It also takes an `Io`, because a rebuild that exhausted its
attempts leaves the baseline pointing at devices no capture is running on any more: without that
input, devices that returned to what the session started on would be judged unchanged and the
recording would stay silent for the rest of the meeting.

That `Io` input is only reachable if somebody asks again, and the poller emits `DevicesChanged` on a
change, not on a state. So the run loop owns a `Recovery`: capture it wanted but does not have,
retried every `RECOVERY` seconds regardless of which event woke the loop. It covers both directions
the same way - a rebuild that exhausted its attempts, and a session start that failed while the
meeting app keeps holding the microphone and therefore produces no second `InputTaken` to trigger
another attempt. Recovery is throttled rather than run on every tick because a permanently failing
rebuild sleeps 750 ms per round, and a permanently failing start would otherwise log five times a
second.

## Format generations, and why the writer resamples

The ring's blocks carry a **format generation**, a counter incremented on every capture build. The
writer decides what to do from the generation of the block it is about to write, so blocks captured
at the old rate are handled at the old rate. Without that, blocks queued at 24000 Hz would be
converted as if they were 48000 Hz at the exact moment the default device changes.

`ExtAudioFile` refuses a *different* client format once writing has started - probed directly: the
first `kExtAudioFileProperty_ClientDataFormat` succeeds, re-applying the same rate succeeds, but a
different rate returns `-66565` (`kExtAudioFileError_InvalidOperationOrder`) and every later write
fails too. So the client format is established by the first block and stands for the life of the
file; a later generation captured at another rate is **resampled onto it** instead. The silence
detector and the resampler both reset on a generation change, which is the rebuild boundary the
writer can see.

Two formats, not one. The **file** format is AAC at `config.sample_rate`, set once at creation - that
is the rate the sidecar reports and the rate the file plays at. The **client** format is float PCM at
the rate the aggregate delivered for the first block written, and `ExtAudioFile` converts between the
two. Only the client format is bound to a generation.

The `ExtAudioFileRef` is created, used and closed only on the writer thread. The rebuild path
reaches the writer solely by pushing blocks with a new generation.

## The AAC bit rate is a three-step sequence

Order matters: set the client format first (that is what instantiates the codec), read the converter
out with `kExtAudioFileProperty_AudioConverter` and set `kAudioConverterEncodeBitRate` on it, then
commit by setting `kExtAudioFileProperty_ConverterConfig` to a **pointer-sized NULL**. Skipping the
commit makes the setting silently do nothing; passing a `UInt32` zero instead of a null pointer
crashes the writer thread.

## Real-time discipline

The IOProc runs on a real-time thread and must not allocate, lock, block or touch the filesystem. It
interprets the buffer list into a scratch the block owns, copies both tracks into the preallocated
ring, and returns. The writer thread drains the ring and does the fold, the encode and the write.
Writer errors go into a shared slot, read after the thread joins - never a panic on that thread.

## The two-track seam

Microphone and system audio stay separate concepts from the IOProc all the way to the writer. The
ring carries them apart. They are folded into interleaved stereo in exactly one place,
`writer::fold`: left is the microphone, right is the system mixdown, and when the microphone track
is absent its channel is silence rather than a copy of the system track.

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

The unsafe Core Audio surface cannot be unit tested. The answer is not "no tests" but pushing the
decisions out of the unsafe code into pure functions that can be: buffer-layout interpretation,
event diffing, allow-list matching, silence detection, rebuild decisions, file naming, plist
rendering. The unsafe blocks stay thin and are covered by running the daemon against a real meeting.

`session::run` is driven in tests with a fake `ActivitySource`, a fake `Capture` and a fake `Sink`.
No test creates a real tap.

Before calling anything done: `mise run check` (fmt, clippy `-D warnings`, tests) is green, the
unsafe grep above passes, and every module file is declared with `mod <name>;` in its parent - an
undeclared module is not compiled, and neither are its tests.
