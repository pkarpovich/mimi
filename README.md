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
| `2026-08-30T14-32-05-thebrowser.m4a` | remuxed from it at completion; the `.aac` is removed once it exists, and kept instead when the remux fails |
| `2026-08-30T14-32-05-thebrowser.json` | written at completion |

The label comes from the allow-list prefix that matched: lowercased, trailing dot removed, reduced to its last dotted component. `company.thebrowser.` becomes `thebrowser`, `us.zoom.` becomes `zoom`, `com.google.Chrome` becomes `chrome`. A name that is already taken gets a numeric suffix rather than overwriting anything: the in-progress file is created at the moment the name is chosen, so a second recorder walking the same names in the same second is handed the next suffix rather than the file this one is about to write.

`output_dir` is created `0700` when it does not exist, and recordings and sidecars are created `0600` - a meeting stays readable by the user who recorded it and nobody else. A directory that already exists keeps the mode it was given.

The sidecar:

```json
{
  "file": "2026-08-30T14-32-05-thebrowser.m4a",
  "started_at": "2026-08-30T14:32:05+02:00",
  "ended_at": "2026-08-30T15:04:11+02:00",
  "duration_seconds": 1926,
  "bundle_id": "company.thebrowser.browser.helper",
  "label": "thebrowser",
  "sample_rate": 24000,
  "channels": 2,
  "device_changes": 1,
  "failed_device_changes": 0,
  "silent": false,
  "write_failed": false,
  "on_complete": {"state": "pending"}
}
```

`file` names the audio the sidecar describes, `label` is the allow-list label the name was built from, and `on_complete` is the completion hook's delivery state - present only when a hook was configured while the recording completed.

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
| `on_complete` | string | unset | a shell command run once for every recording that completes while it is configured; absent means no hook, blank is a startup error |
| `on_complete_timeout_seconds` | integer | `120` | how long one run of that command may take before its process group is killed |

Print the effective configuration and exit:

```sh
mimi --check-config
```

It exits non-zero and names the reason when the file cannot be used.

## Completion hook

With `on_complete` set, every recording that completes is handed to that command until one run of it exits 0. mimi knows nothing about what the command does - uploading, transcribing, enqueuing - it only guarantees the hand-over.

```toml
on_complete = "/usr/local/bin/publish-recording"
on_complete_timeout_seconds = 120
```

The contract:

- The command runs as `/bin/sh -c <on_complete>`, with `output_dir` as the working directory and in its own process group. stdin is `/dev/null`; stdout and stderr are the daemon's own, so under the LaunchAgent the command's stderr lands in `~/Library/Logs/mimi.err.log` next to mimi's own events, and its stdout in `~/Library/Logs/mimi.log`.
- It gets the daemon's environment plus `MIMI_RECORDING`, the absolute path of the completed audio (the `.m4a`, or the `.aac` when the remux failed and the ADTS file was kept), and `MIMI_SIDECAR`, the absolute path of its sidecar. Everything else about the recording is in that sidecar.
- **Exit 0 means delivered.** The sidecar is marked done and the command is never run again for that recording. Any other exit, a signal, a failure to start, or outliving `on_complete_timeout_seconds` means the recording is still owed and is retried; on the timeout the whole process group is killed, so a command that spawned children does not leave them behind.
- Retries back off: 60 seconds after the first failure, doubling, capped at 30 minutes. One run at a time, in order of completion. A hook that keeps failing never delays a recording - completion hands the sidecar over and the run loop goes straight back to watching the microphone, and a recording waiting out its backoff does not hold up the ones behind it.
- **`output_dir` has to be yours alone.** With a hook configured, mimi creates it at 0700 at startup rather than at the first meeting, and then checks it: if it was already there and is owned by another user, is writable by group or other, or hands write access to anybody through an extended ACL, `on_complete` is refused at startup and mimi exits non-zero. Every directory on the way to it is checked too, and has to be owned by you or by root and closed to everyone else - a directory somebody else can rename away is one they can put their own in place of, which needs no write access to the directory mimi judged. A shared parent that is sticky (`/tmp`, `/Users/Shared`) passes: sticky is what stops anybody but the owner from renaming what is inside it. The directory it accepts is the one it then records into and delivers from, symlinks resolved once at startup, so a link on the way to it cannot be repointed after the check. The hook hands over whatever pending sidecars name, and in a directory somebody else can write that is no longer only what mimi recorded.
- **A stopping daemon does not wait for the command.** Ctrl-C, `launchctl` stop or an upgrade kills the in-flight run's process group the same way the timeout does, and nothing is flushed on the way out: the recording stays `pending` and the next start hands it over again.

Delivery state lives in the sidecar and nowhere else. A recording that completes under a configured hook carries `"on_complete": {"state": "pending"}` from the moment its sidecar is written, and `{"state": "done", "at": "2026-08-30T15:04:19+02:00"}` after the run that exited 0. At every start mimi scans `output_dir` for sidecars still `pending` and queues them oldest first, so a recording survives a hook that never succeeded, a daemon that was stopped, and a reboot.

Two consequences worth knowing:

- **The command must be idempotent.** A run that exited 0 in the window between the child's exit and the sidecar rewrite - a `kill -9` landing exactly there - is run again at the next start.
- **Nothing is published retroactively.** Sidecars written before the hook was configured carry no `on_complete` field and are never touched, so turning the hook on does not replay the archive. With `on_complete` unset no `on_complete` field is written at all - though every sidecar carries `file` and `label` either way.

Under launchd the environment is minimal - `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin` - so a command that needs anything else has to name it by absolute path or set it up itself.

## Install

mimi needs macOS 14.2 or newer: that is where the Core Audio process-tap API (`CATapDescription`, `AudioHardwareCreateProcessTap`) arrives. On anything older every session fails to create the tap.

The binary needs a stable code signature so that TCC keeps recognising it across rebuilds:

```sh
security find-identity -v -p codesigning
./scripts/build-signed.sh "Developer ID Application: Your Name (TEAMID)"
```

The script signs with the hardened runtime, which denies the microphone outright unless the binary carries `com.apple.security.device.audio-input` - `mimi.entitlements` is what grants it, and the script prints the entitlements back after signing so you can see it took.

A release goes one step further: the workflow notarizes the signed bundle with Apple and staples the ticket to it, so Gatekeeper answers `accepted` for the app the cask installs - `spctl --assess --type exec /Applications/Mimi.app` shows `source=Notarized Developer ID`. A bundle from `build-signed.sh` is signed but not notarized, which launchd does not mind (it starts the binary directly) and Finder does (it refuses to open the app). The release fails rather than ships if the verdict is anything else.

For distribution there is `scripts/bundle.sh <binary> <out-dir> [identity]`, which assembles `Mimi.app` around the same binary, gives it the icon and `LSUIElement`, and signs the bundle with the same entitlements. The bundle exists for one reason: TCC identifies a bundle by its identifier at a path that does not move, and a loose binary by its absolute path - which any package manager changes on every version, taking the microphone grant with it. The icon comes from `assets/AppIcon.icns`, which is committed; `scripts/icon.sh` regenerates it from `assets/icon.svg` through a headless browser and `iconutil`, and only needs running when the artwork changes.

The bundle-only keys (`CFBundleExecutable`, `CFBundlePackageType`, `LSUIElement`, `CFBundleIconFile`) are deliberately absent from `Info.plist.template` and added by `bundle.sh`, because `build.rs` embeds that template into the bare binary and declaring a loose daemon an `APPL` bundle makes macOS treat it as a UI application.

Then install the LaunchAgent, which points at the executable you run `install` from:

```sh
./target/release/mimi install
```

That writes `~/Library/LaunchAgents/dev.pkarpovich.mimi.plist` with `RunAtLoad` and `KeepAlive`, and loads it with `launchctl bootstrap gui/<uid>`. Re-installing over a loaded agent replaces it.

Installed from the cask, this runs by itself: the cask's `postflight` calls `mimi install` after every install and every upgrade. That is not decoration - Homebrew upgrades a cask by uninstalling the old version first, and the uninstall stanza unloads the agent and deletes its plist, so without the postflight the daemon would be gone after each upgrade and nothing would say so.

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

Every event goes to stderr, so under the LaunchAgent the file to read is `~/Library/Logs/mimi.err.log`. `~/Library/Logs/mimi.log` is the agent's `StandardOutPath` and stays empty unless a completion-hook command writes to stdout. mimi logs its version at startup, then one event per session start, session end, device rebuild, rebuild failure, silence verdict and dropped ring blocks - and, with a hook configured, one per delivered recording and one per run that left a recording undelivered, naming why. `mimi --version` (or `-V`) prints the same version, which is what tells you whether an upgrade actually took.

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
