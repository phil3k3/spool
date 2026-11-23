# Spool

A virtual macOS output device (like BlackHole) that records everything played to it
straight to disk, and can forward it to your speakers. See [DESIGN.md](DESIGN.md) for goals
and roadmap and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the components interact.

```
apps ─► coreaudiod ─► Spool.driver ──(POSIX shm ring)──► spoold ─┬─► ~/Music/Spool/*.wav
                                                           ▲      └─► speakers (optional)
                                                         spool (CLI)
```

| Crate | What |
|---|---|
| `crates/driver` | AudioServerPlugIn (`cdylib`): one output device, f32 stream, volume and mute, 44.1–192 kHz |
| `crates/ring` | Lock-free SPSC ring in POSIX shared memory (the `coreaudiod` sandbox allows `ipc-posix-shm`) |
| `crates/daemon` | `spoold`: drains the ring, writes 32-bit float WAV, rotates by size/duration, monitors to a real device |
| `crates/proto` | JSON-over-Unix-socket control protocol |
| `crates/ctl` | `spool` CLI |

## Build, test, install

```sh
cargo test --workspace          # ring, driver (via its vtable with a fake host), daemon, CLI
scripts/build-bundle.sh         # target/Spool.driver, ad-hoc signed (SIGN_IDENTITY=... for Developer ID)
scripts/install.sh              # sudo; driver, /usr/local/bin/{spool,spoold}, LaunchAgent;
                                #   restarts coreaudiod if the driver changed (audio drops out briefly)
scripts/uninstall.sh            # keeps recordings, settings and logs
```

## Use

Pick **Spool** as the output device (Sound settings or any app). On its own that is
silent; `spool` decides what happens to the audio:

```sh
spool start                       # listen; the first time it asks which device to play through
spool start --device "MacBook"    # or name it directly (name, part of a name, or UID)
spool record                      # record with a live view: levels, time, size, disk left
                                  #   (Ctrl-C stops recording; -d returns immediately)
spool start --record              # forward and record in one go
spool stop --record               # stop recording, keep listening
spool stop                        # stop both
spool split                       # close the current file, continue in a new one
spool status [--json]             # one-shot status; -w/--watch for the live view
spool devices                     # list output devices
```

States are layered: stopped → listening (`start`) → recording (`record` or
`start --record`). Commands are idempotent like `systemctl`: repeating one reports the
current state and exits 0. `stop --record` goes back to listening and `stop` stops both.
Every state-changing command prints one line saying what changed.

`start` always plays through a device you chose, never a guessed one. The first time, it
lists your output devices and asks (from a script it exits with the list instead). The
choice is remembered and a new `--device` replaces it. Recording only starts with
`--record` or `spool record`. Forwarding follows Spool's
volume and mute, so the volume keys work; recordings stay raw unless `--post-fader`.
Sample-rate differences and clock drift are handled by a drift-correcting resampler;
latency target is 50 ms (`spoold --monitor-latency-ms N`). Don't forward to an
aggregate/multi-output device that contains Spool, which would feed back.

`spoold` options: `--start [--device DEV]`, `--record`, `--max-minutes N`, `--max-mb N`
(WAV is capped at ~4 GB), `--post-fader` (record after device volume and mute; default is
the raw mix), `--prefix`, `--dir`, `--monitor-latency-ms N`, plus `--socket`, `--state` and
`--ring` for running a second instance. `spoold --help` has the defaults.

Try the daemon without installing the driver:

```sh
cargo build --release -p spool-ring --example tone -p spool-daemon -p spool-ctl
target/release/examples/tone /spool.tone 30 &       # 30 s of 440 Hz into a test ring
target/release/spoold --ring /spool.tone --socket /tmp/spool-try.sock --dir /tmp/spool-try --record
target/release/spool --socket /tmp/spool-try.sock status     # from another terminal
```

The separate `--socket` matters if Spool is installed: only one `spoold` may run per socket,
so a second one on the default socket exits with "another spoold is already running".

## Identifiers

The driver's bundle ID and the launchd names use a reverse-DNS prefix derived at build
time from the `origin` remote (`github.com/<owner>/…` → `io.github.<owner>`), so forks
don't clash with upstream. Override it with `SPOOL_ID_PREFIX=com.example`, and check it
with `scripts/spool-id.sh`. Details are in [docs/XPC.md](docs/XPC.md) §5.

## Behaviour notes

- When no app is playing, Core Audio stops IO and nothing is written. The file resumes
  when audio returns, so silence between sessions is not recorded. Short HAL overloads
  (< 0.5 s) are filled with silence to keep the timeline intact.
- The driver never blocks. It only writes while `spoold` is alive (heartbeat within 2 s);
  otherwise it skips the audio. If the ring is full it drops the newest audio
  (`spool status` shows these as dropped samples). Inside `spoold`, the ring is drained
  by its own thread, so slow devices or disks don't stall it; recording can fall about
  87 s behind (48 kHz stereo) before audio is dropped. Every loss while recording is
  counted as a gap ("discontinuities" in `spool status`).
- The shared-memory ring is mode 0666 because `coreaudiod` and `spoold` run as different
  users, and macOS cannot change a shm object's mode or owner after creation. **Any local
  user or process can therefore read (and write) the audio played to Spool.** Don't use
  it for sensitive audio on a shared Mac. An opt-in XPC transport that closes this is in progress
  ([docs/XPC.md](docs/XPC.md)); `scripts/install.sh --xpc` installs its spike, which does
  not carry audio yet. The driver never writes into a ring it does
  not own, `spoold` only attaches to one owned by `_coreaudiod` or by you (the test
  tone), everything read from the ring is validated, and only one consumer can be
  attached at a time.
- Only one `spoold` runs per socket (lock file `spoold.sock.lock` next to it). The socket
  is only usable by your user. The daemon log is `~/Library/Logs/Spool/spoold.log`.
- Driver errors go to the system log: `log stream --predicate 'eventMessage CONTAINS "Spool"'`.

## License

MIT, see [LICENSE](LICENSE).
