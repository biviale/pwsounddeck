# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

PWSoundDeck — an OpenDeck / OpenAction plugin, in Rust, playing audio files to a
chosen PipeWire/PulseAudio sink from a Stream Deck. Linux only. User-facing
documentation is in `README.md`; this file covers what you need to change the
code.

## Commands

```bash
cargo test                        # unit tests (no audio device needed)
cargo test loop_mode              # filter by test name, not function name
cargo build --release
./scripts/install.sh              # build + install into OpenDeck
./scripts/install.sh --restart    # the above, then restart OpenDeck

tail -f ~/.local/share/opendeck/logs/plugins/com.biviale.pwsounddeck.sdPlugin.log
```

`install.sh` stages the binary into `com.biviale.pwsounddeck.sdPlugin/` and
rsyncs that directory (with `--delete`) into
`~/.config/opendeck/plugins/`. **Nothing else may be kept in the installed
directory** — it is a mirror of the source tree and anything extra is removed on
the next install.

OpenDeck reads `manifest.json` only at startup, so **any manifest change needs
`--restart`**, and then the affected action's instances removed and re-added in
the OpenDeck UI.

Log timestamps are **UTC**, not local time — `simplelog`'s default. Do not hunt
for an event at the wrong end of the file.

## Layout

The repository root is a Cargo crate (`Cargo.toml`, `src/`, `scripts/`); the
files OpenDeck actually loads live in `com.biviale.pwsounddeck.sdPlugin/`
(`manifest.json`, `icons/`, `pi/`). The built binary is staged into that
directory by `install.sh` and is gitignored. Before v0.3.0 the repository root
*was* the plugin directory, so instructions found in old issues or commits may
not apply.

All the Rust lives in one file, `src/main.rs`.

## Architecture

### The player registry — read this before touching playback

`ACTIVE_PLAYERS` is the only shared state: a `DashMap<u64, (String,
Option<rodio::Player>)>` keyed by slot id, holding the OpenAction `instance_id`
of the button that owns it. One button can own several slots (Stack mode).

**The `Option` is not an afterthought.** Opening a sink and decoding a file take
long enough that a second key press used to arrive before the first had
registered anything, so a Loop key pressed twice quickly started a second,
unstoppable loop. `key_down` therefore calls `reserve_player_slot` **before**
spawning the audio thread, inserting `None`; the thread later hands over its
player through `attach_player`. Consequences for any change here:

- **Every exit path in the audio thread must remove its slot.** A failed sink,
  a missing file or a decode error that returns without releasing leaves the
  button marked busy forever, and in Loop mode that means permanently dead.
- `attach_player` returning `false` means the slot was cancelled while the audio
  was starting. It stops the player itself; the thread must just give up.
- Anything reading the map must handle `None`, which means "starting up", not
  "finished".

The audio thread polls the map every 100 ms rather than using rodio's
`sleep_until_end`, because the slot can disappear from under it (Stop Audio,
Restart mode, a Loop toggle). A looping source **never reports `empty()`**, so
for Loop mode that poll only ever ends by the slot being removed.

### Playback modes are decided by pure functions

`decide_key_down` and `decide_key_up` hold all the mode logic and touch no
audio, which is what makes the modes testable without a sound card. **Add a new
mode there and in `pi/audio.html`'s `<select>`, not with a new branch inside
`key_down`.** The mode is a bare string shared between the Property Inspector
and `AudioSettings.playback_mode`; an unknown string falls back to `restart`.

Ordering inside `decide_key_down` is load-bearing: the Loop toggle is checked
*before* the audio path, so that clearing the path in the Property Inspector
cannot strand a running loop with no way to stop it.

Tests share the one global `ACTIVE_PLAYERS` and run in parallel, so **each test
must use its own `instance_id`** and clean up after itself.

### Device routing goes through environment variables

There is no rodio API for "open this named sink", so `key_down` sets
`PULSE_SINK` and `PIPEWIRE_NODE`, opens the sink, and immediately unsets them —
all while holding `STREAM_CREATION_LOCK`, because the environment is
process-global and `std::env::set_var` is `unsafe` in edition 2024.

This is documented in issue #2, closed without a fix. **It is a soundness
problem, not a latency one** — that was measured and the latency framing was
dropped. `get_pulseaudio_sinks` now takes the lock around its `pactl` spawn, so
the common half of the race is closed. **The `rfd` file dialog still reads the
environment unlocked** — `pick_file()` blocks until the user chooses, so it
cannot hold the lock without freezing every key press.

The `unsafe` cannot be removed with this stack: cpal enumerates ALSA PCMs
(`default`, `pulse`, `pipewire`, `hw:CARD=…`), not PipeWire sinks — none of the
names `pactl` returns appear in `cpal::Host::output_devices()`. Targeting a sink
by name would mean replacing rodio's device layer with libpulse or pipewire
bindings. Do not start that without a reason bigger than this one.

Do not reopen this as a performance fix. Measured on PipeWire 1.6.8 / rodio
0.22.2, over 52 real key presses: `open_default_sink` is 7.4 ms of a 7.8 ms
synchronous path, press-to-sound is ~28 ms for WAV and ~40 ms for MP3, and
`STREAM_CREATION_LOCK` never contended once — a finger cannot press fast enough
to reach it. Caching a sink would also make things **worse** unless paired with
`with_buffer_size`: rodio aims for 50 ms of buffer, so a permanently open stream
measured 86 ms press-to-sound against 27 ms for the current open-per-press path.

### Property Inspector

`pi/` is plain HTML and JavaScript with no build step. It talks to the plugin
over `sendToPlugin` with a `command` field — `get_devices` and
`open_file_picker` — and the plugin answers through
`send_to_property_inspector` with an `event` field. Both sides run blocking work
(`pactl`, the native file dialog) inside `spawn_blocking`.

`AudioSettings` fields are all `String`, including `volume`, because they come
straight from HTML inputs; `volume` is parsed with a fallback rather than
trusted.

## Known rough edges

`README.md`'s *Known limitations* is the user-facing list and is kept honest —
looping holds the whole decoded file in memory, errors are logged but invisible
on the deck, and Stop Audio is all-or-nothing. `openaction` offers
`instance.show_alert()` and `instance.set_state()`, neither of which is used
yet; `set_state` would need a second state declared in `manifest.json`.
