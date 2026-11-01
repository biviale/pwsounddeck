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

`ACTIVE_PLAYERS` is the shared state playback revolves around: a
`DashMap<u64, (String, Option<rodio::Player>)>` keyed by slot id, holding the
OpenAction `instance_id` of the button that owns it. One button can own several
slots (Stack mode). The only other shared state is `SHUFFLE_CYCLES`, covered
under *Play Random Audio* below.

**The `Option` is not an afterthought.** Opening a sink and decoding a file take
long enough that a second key press used to arrive before the first had
registered anything, so a Loop key pressed twice quickly started a second,
unstoppable loop. `start_playback`, which both play actions' `key_down` call,
therefore calls `reserve_player_slot` **before** spawning the audio thread,
inserting `None`; the thread later hands over its player through
`attach_player`. Consequences for any change here:

- **Every exit path in the audio thread must remove its slot.** A failed sink,
  a missing file, a decode error or a random folder with nothing in it that
  returns without releasing leaves the button marked busy forever, and in Loop
  mode that means permanently dead.
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
`start_playback`.** The mode is a bare string shared between the Property Inspector
and `AudioSettings.playback_mode`; an unknown string falls back to `restart`.

Ordering inside `decide_key_down` is load-bearing: the Loop and Play/Stop toggle
is checked *before* the audio path, so that clearing the path in the Property Inspector
cannot strand a running loop with no way to stop it.

Tests share the global `ACTIVE_PLAYERS` and `SHUFFLE_CYCLES` and run in
parallel, so **each test must use its own `instance_id`** and clean up after
itself. Tests that need files use `TestDir`, which deletes its directory even
when the test fails.

### Play Random Audio

`PlayRandomAudioAction` shares `start_playback` with Play Audio; the two differ
only in their `AudioSource`. It has **no playback mode**: every press goes
through `decide_key_down(RANDOM_PLAYBACK_MODE)`, which is Restart, and it has no
`key_up`. Random is not a mode — do not add it to the `<select>`.

The file is drawn in the audio thread, by `resolve_audio_path`, before
`STREAM_CREATION_LOCK` is taken, because listing a folder is blocking I/O. Its
two exits keep the registry rule above: a press that was already cancelled
returns before drawing (so it does not use up a sound of the shuffle), and a
folder with nothing to play removes the slot.

Memory is the point of the design, because a folder can hold 10 000+ files:

- `audio_file_names` reads the directory lazily and `draw_from_cycle` samples
  it in a single pass (reservoir sampling). **Never collect the listing into a
  `Vec`, and never store file names per key.**
- `SHUFFLE_CYCLES` holds one `ShuffleCycle` per key: one bit per file, indexed
  by its position in the listing, plus a fingerprint of the listing and the
  last name played. The bits only mean something for the listing they were set
  against, so a changed fingerprint starts a new cycle. This relies on
  `read_dir` listing an unchanged directory in the same order, which local
  filesystems do.
- The pass runs under the key's `SHUFFLE_CYCLES` entry lock; only another random
  key's draw can wait on it.
- Entries are never removed, not even in `will_disappear`: it also fires on page
  changes, which would reset the shuffle.

### Device routing goes through environment variables

There is no rodio API for "open this named sink", so the audio thread sets
`PULSE_SINK` and `PIPEWIRE_NODE`, opens the sink, and immediately unsets them —
all while holding `STREAM_CREATION_LOCK`, because the environment is
process-global and `std::env::set_var` is `unsafe` in edition 2024.

This is documented in issue #2, closed without a fix. **It is a soundness
problem, not a latency one** — that was measured and the latency framing was
dropped. `get_pulseaudio_sinks` now takes the lock around its `pactl` spawn, so
the common half of the race is closed. **The `rfd` file and folder dialogs
still read the environment unlocked** — `pick_file()` and `pick_folder()` block
until the user chooses, so they cannot hold the lock without freezing every key
press.

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
over `sendToPlugin` with a `command` field — `get_devices`, `open_file_picker`
and `open_folder_picker` — and the plugin answers through
`send_to_property_inspector` with an `event` field. Both sides run blocking work
(`pactl`, the native dialogs) inside `spawn_blocking`.

Both play actions use `pi/audio.html`. `audio.js` checks the action UUID: for
Play Random Audio it relabels the path field, hides the Playback Mode row
(with `style.display`, since `.sdpi-item`'s `display: flex` beats the `hidden`
attribute) and makes Browse open the folder dialog. The folder dialog answers
with the same `file_selected` event as the file dialog.

`AudioSettings` and `RandomAudioSettings` fields are all `String`, including
`volume`, because they come straight from HTML inputs; `volume` is parsed with a
fallback rather than trusted.

## Known rough edges

`README.md`'s *Known limitations* is the user-facing list and is kept honest —
looping holds the whole decoded file in memory, errors are logged but invisible
on the deck, and Stop Audio is all-or-nothing. `openaction` offers
`instance.show_alert()` and `instance.set_state()`, neither of which is used
yet; `set_state` would need a second state declared in `manifest.json`.
