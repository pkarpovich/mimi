<img src="assets/icon.svg" width="96" align="right" alt="">

# mimi

A macOS daemon that records meetings without being asked.

mimi watches which processes hold the microphone. When one of the configured meeting applications takes it, mimi records two tracks - your microphone and the audio the other participants produce (captured with a Core Audio process tap) - and writes them into a local folder as one stereo file: **left channel is you, right channel is everyone else**. When the meeting application releases the microphone, mimi closes the file.

It never opens the microphone speculatively. Nothing is recorded until a process whose bundle id matches the allow-list is already holding the input.

## The name

**mimi** is 耳, Japanese for "ears".

It is the companion of [nikki](https://github.com/pkarpovich/nikki) - 日記, "diary" - the daemon that records what was on screen and what was being done. nikki watches, mimi listens.

## What it records

- One ADTS AAC file per session, stereo, at a fixed sample rate (24000 Hz by default).
- Left channel: your microphone. Right channel: the system audio mixdown of the other participants.
- One JSON sidecar per session, carrying what the recording was.

The container is deliberate. ADTS frames are self-synchronising and carry no index, so a file left behind by a crash or a `kill -9` is still playable up to the point it stopped. That guarantee only holds because recording writes directly into `output_dir` - never into a temporary directory that a later move would depend on.

## When it triggers

A session **starts** when a process whose bundle id starts with one of `meeting_bundle_prefixes` takes the microphone, and **ends** when no such process holds it any more, after `stop_grace_seconds` have passed. The grace period is what keeps a meeting application that briefly drops and re-takes the input from splitting one meeting into two files.

Matching is by prefix because meeting applications hold the microphone from a helper process, not from the main one: Google Meet in Dia holds it as `company.thebrowser.browser.helper`, Teams appears as five processes and Slack as three. mimi excludes its own process, so it never reacts to its own microphone use.

Two other behaviours follow from how the daemon is meant to be run:

- Started **mid-meeting**, mimi sees a process already holding the input and starts recording. It
does not wait for the next take.
- If a meeting application **crashes**, its process disappears while holding the input and the
session ends normally instead of hanging.

A device change - AirPods dying and AirPods Max taking over, or AirPods switching into headset mode and changing sample rate - rebuilds the capture **without closing the file**. The recording continues into the same artifact; the gap is the hardware settling time.

## Where the files land

Everything goes into `output_dir` (default `~/Recordings/mimi`). For a session that started at 2026-08-30 14:32:05 local time in Dia:

| file | when |
|---|---|
| `2026-08-30T14-32-05-thebrowser.aac.partial` | while recording |
| `2026-08-30T14-32-05-thebrowser.aac` | renamed in place at completion |
| `2026-08-30T14-32-05-thebrowser.json` | written at completion |

The label comes from the allow-list prefix that matched: lowercased, trailing dot removed, reduced to its last dotted component. `company.thebrowser.` becomes `thebrowser`, `us.zoom.` becomes `zoom`, `com.google.Chrome` becomes `chrome`. A name that is already taken gets a numeric suffix rather than overwriting anything: the in-progress file is created at the moment the name is chosen, so a second recorder walking the same names in the same second is handed the next suffix rather than the file this one is about to write.

`output_dir` is created `0700` when it does not exist, and recordings and sidecars are created `0600` - a meeting stays readable by the user who recorded it and nobody else. A directory that already exists keeps the mode it was given.

The sidecar:

```json
{
  "started_at": "2026-08-30T14:32:05+02:00",
  "ended_at": "2026-08-30T15:04:11+02:00",
  "duration_seconds": 1926,
  "bundle_id": "company.thebrowser.browser.helper",
  "sample_rate": 24000,
  "channels": 2,
  "device_changes": 1,
  "failed_device_changes": 0,
  "silent": false,
  "write_failed": false
}
```

`device_changes` counts the rebuilds that succeeded, `failed_device_changes` the ones that exhausted their retries - a recording whose capture never came back says so, once per outage rather than once per retry. `silent` is the verdict of the silence check, which watches the opening seconds of the recording and restarts on every rebuild; an opening that was judged silent stays reported for the rest of the session, whatever a later rebuild heard. The check exists because a capture that turns into digital silence is otherwise invisible: callbacks keep firing, counters stay healthy, and the file is empty. `write_failed` says the writer gave up before the session ended - the file holds everything up to that point and nothing after it.

## Configuration

`~/.config/mimi/config.toml`, read once at startup. A missing file is not an error - the defaults apply. A malformed file, an unknown key or an out-of-range value is a startup error, and the daemon exits rather than recording with a silently wrong allow-list.

| key | type | default | meaning |
|---|---|---|---|
| `output_dir` | string | `~/Recordings/mimi` | where recordings are written, including while in progress. `~`, `~/x` and relative values resolve against `$HOME` |
| `meeting_bundle_prefixes` | array of strings | `company.thebrowser.`, `us.zoom.`, `com.microsoft.teams2`, `com.tinyspeck.slackmacgap`, `com.google.Chrome` | a process matches when its bundle id starts with any entry |
| `sample_rate` | integer | `24000` | the fixed rate of the written file; one of the twelve rates AAC carries - 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000 |
| `bit_rate` | integer | `96000` | AAC bit rate in bits per second, between 8000 and 320000 |
| `stop_grace_seconds` | integer | `15` | how long the microphone must stay released before a session closes |
| `poll_interval_ms` | integer | `1000` | how often the microphone holders are sampled |

Print the effective configuration and exit:

```sh
mimi --check-config
```

It exits non-zero and names the reason when the file cannot be used.

## Install

mimi needs macOS 14.2 or newer: that is where the Core Audio process-tap API (`CATapDescription`, `AudioHardwareCreateProcessTap`) arrives. On anything older every session fails to create the tap.

The binary needs a stable code signature so that TCC keeps recognising it across rebuilds:

```sh
security find-identity -v -p codesigning
./scripts/build-signed.sh "Developer ID Application: Your Name (TEAMID)"
```

The script signs with the hardened runtime, which denies the microphone outright unless the binary carries `com.apple.security.device.audio-input` - `mimi.entitlements` is what grants it, and the script prints the entitlements back after signing so you can see it took.

For distribution there is `scripts/bundle.sh <binary> <out-dir> [identity]`, which assembles `Mimi.app` around the same binary, gives it the icon and `LSUIElement`, and signs the bundle with the same entitlements. The bundle exists for one reason: TCC identifies a bundle by its identifier at a path that does not move, and a loose binary by its absolute path - which any package manager changes on every version, taking the microphone grant with it. The icon comes from `assets/AppIcon.icns`, which is committed; `scripts/icon.sh` regenerates it from `assets/icon.svg` through a headless browser and `iconutil`, and only needs running when the artwork changes.

The bundle-only keys (`CFBundleExecutable`, `CFBundlePackageType`, `LSUIElement`, `CFBundleIconFile`) are deliberately absent from `Info.plist.template` and added by `bundle.sh`, because `build.rs` embeds that template into the bare binary and declaring a loose daemon an `APPL` bundle makes macOS treat it as a UI application.

Then install the LaunchAgent, which points at the executable you run `install` from:

```sh
./target/release/mimi install
```

That writes `~/Library/LaunchAgents/dev.pkarpovich.mimi.plist` with `RunAtLoad` and `KeepAlive`, and loads it with `launchctl bootstrap gui/<uid>`. Re-installing over a loaded agent replaces it.

```sh
mimi uninstall
```

unloads the agent and removes the plist. Recordings and logs are left alone.

You can also run it in the foreground - `mimi run`, or just `mimi` - and stop it with Ctrl-C. SIGINT and SIGTERM close any recording in progress through the normal session-end path, so the file is renamed and its sidecar is written.

Only one mimi runs at a time. A second one exits immediately with `another instance is already running`, holding an advisory lock on `~/Library/Application Support/mimi/instance.lock` for the life of the process. Without that refusal the second daemon looks alive but records nothing: its aggregate device carries the same UID as the first one's, Core Audio refuses to create it, and the failure repeats once per poll while the first daemon keeps writing files - so everything appears to work while the instance you actually installed is dead. The lock lives on the open file, so a `kill -9` releases it too.

## Permissions

mimi carries `NSMicrophoneUsageDescription` and `NSAudioCaptureUsageDescription` in an `__info_plist` section embedded in the binary itself, so it needs no app bundle. It needs:

- **Microphone** access, to record your side of the meeting.
- **Audio capture** for the Core Audio process tap, to record the other participants. This is the
audio-only API; mimi deliberately does not use ScreenCaptureKit, which would demand the broader screen-recording permission.

Grant whatever macOS prompts for on the first recorded session. If no prompt appears and the recordings come out silent, that is the silence check firing and the permission state is what to look at first.

## Logs

Every event goes to stderr, so under the LaunchAgent the file to read is `~/Library/Logs/mimi.err.log`. `~/Library/Logs/mimi.log` is the agent's `StandardOutPath` and stays empty. mimi logs its version at startup, then one event per session start, session end, device rebuild, rebuild failure, silence verdict and dropped ring blocks. `mimi --version` (or `-V`) prints the same version, which is what tells you whether an upgrade actually took.

## Development

A Rust toolchain of 1.85 or newer (the crate is edition 2024) and the Xcode command line tools have to be there already; `.mise.toml` carries the tasks, not a toolchain pin.

```sh
mise run check   # cargo fmt --check, clippy -D warnings, cargo test
mise run build
mise run test
mise run lint
mise run fmt
```

The Core Audio surface cannot be unit tested, so the decisions are pushed out of the unsafe code into pure functions that can be: buffer-layout interpretation, event diffing, allow-list matching, silence detection, rebuild decisions, file naming and plist rendering each have their own tests. `CLAUDE.md` carries the conventions the crate is built under.
