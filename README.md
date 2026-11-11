# PWSoundDeck

A soundboard for Linux, driven from a Stream Deck. Point a key at an audio file,
pick which output device it should come out of, and press. PWSoundDeck is a
plugin for the [OpenAction API](https://openaction.amankhanna.me/), so any
OpenAction server can load it. It is developed and tested against
[OpenDeck](https://github.com/nekename/OpenDeck), the reference server, which is
what the instructions below assume.

Per-key volume, five playback modes, a key that draws a random sound from a
folder, and routing straight to a chosen PipeWire/PulseAudio sink — so a sound
effect can go to your headset, or to the virtual sink your friends hear, without
touching the system default.

<p align="center">
  <img src="com.biviale.pwsounddeck.sdPlugin/icons/icon.png" alt="PWSoundDeck icon" width="128">
</p>

## Requirements

- Linux and an OpenAction server. OpenDeck is the tested one; others
  implementing the API should work but are unverified
- PipeWire or PulseAudio, with `pactl` on `PATH` — it is what enumerates the
  output devices
- Rust and Cargo ([rustup](https://rustup.rs/)): the plugin is built from
  source, there is no prebuilt download yet

## Install

### With OpenDeck

```bash
git clone https://github.com/biviale/pwsounddeck.git
cd pwsounddeck
./scripts/install.sh --restart
```

The script builds the plugin, copies it into
`~/.config/opendeck/plugins/com.biviale.pwsounddeck.sdPlugin`, and restarts
OpenDeck. Leave out `--restart` if you would rather restart OpenDeck yourself;
it does have to restart, because it only reads a plugin's manifest at startup.

The actions show up in OpenDeck's action list, under **PWSoundDeck**.

To update, pull and run the same command again.

### Another OpenAction server

`install.sh` only knows OpenDeck's directory. Anywhere else, build the binary,
stage it inside the plugin directory, and copy that directory into wherever
your server keeps its plugins:

```bash
cargo build --release
install -m 755 target/release/pwsounddeck com.biviale.pwsounddeck.sdPlugin/
cp -r com.biviale.pwsounddeck.sdPlugin /path/to/plugins/
```

It is the same plugin directory either way. The manifest declares Linux only,
since device routing goes through PipeWire/PulseAudio.

## The keys

**Play Audio** plays one file. Drop it on a key, then in the Property Inspector:

- **Browse** for an audio file — `.mp3`, `.wav`, `.ogg` or `.flac`
- **Volume**, from 0% to 100%, independent for every key
- **Output Device**, any sink `pactl` reports, or the system default.
  *Refresh* re-reads the list after plugging something in
- **Playback Mode**, below

**Play Random Audio** plays a sound drawn at random from a folder. **Browse**
picks the folder: every `.mp3`, `.wav`, `.ogg` and `.flac` file directly inside
it is in the draw, while subfolders and hidden files are not. The draw is a
shuffle — each sound plays once before any of them comes back, and never twice
in a row unless it is the only one. Pressing again while a sound plays cuts it and draws the next one;
releasing the key does nothing, and there is no playback mode to choose.
**Volume** and **Output Device** work as for Play Audio.

To change the sounds, add or remove files in the folder; OpenDeck does not need
to know. Changing the folder's contents starts a new shuffle, and so does
restarting OpenDeck. Nothing is loaded ahead of time: each press reads the
folder once, and the shuffle remembers one bit per sound, so a folder of
thousands of files stays light.

**Stop Audio** stops everything this plugin is playing, on every key at once. It
takes no configuration.

## Playback modes

| Mode | Press | Release |
|---|---|---|
| **Restart** | Stops what this key was playing, plays from the start | — |
| **Stack** | Layers another copy over the ones already playing | — |
| **Play/Stop** | Plays once, or stops it if it is still playing | — |
| **Hold to Play** | Plays, like a walkie-talkie | Cuts off |
| **Loop (Toggle)** | Starts the file looping, or stops it if it is already looping | — |

Stack is capped at 10 concurrent sounds per key to prevent high CPU/RAM usage. 
Past that, the oldest is dropped to make room.

A loop runs until you press its key again or hit a **Stop Audio** key.

These modes belong to Play Audio. Play Random Audio has none and always behaves
like Restart.

## Known limitations

- **A looping file is held in memory** for as long as it loops, decoded — on the
  order of 100 MB for a five-minute track, and each loop start allocates its
  own copy. Fine for a jingle or a shanty, less so for a full album.
- **There is a delay between the press and the sound.** Every press opens its
  own connection to PipeWire/PulseAudio, and those opens are serialised, so
  several keys pressed together queue up behind each other.
- **Stop Audio is all-or-nothing.** It cannot stop one key while leaving the
  others running.
- **Errors are silent on the deck.** A missing or undecodable file, or a random
  key whose folder is empty or gone, logs and does nothing visible; check the
  log if a key seems dead.
- **The shuffle needs a folder that lists in a stable order.** Local disks do.
  On some network mounts the order changes from one listing to the next, and a
  random key then only guarantees that a sound never plays twice in a row.

## Troubleshooting

The plugin's log is at:

```
~/.local/share/opendeck/logs/plugins/com.biviale.pwsounddeck.sdPlugin.log
```

It records key presses and errors. To also see the file each press plays, its
format and when its sound ends, start OpenDeck with `PWSOUNDDECK_LOG=debug`:

```bash
PWSOUNDDECK_LOG=debug opendeck
```

- **The actions do not appear** — OpenDeck only reads plugin manifests at
  startup. Restart it.
- **The device list is empty** — check that `pactl list sinks` works from a
  terminal as the same user.
- **Sound comes out of the wrong device** — if the sink you picked has since
  disappeared, the Property Inspector falls back to showing *Default OS Device*
  while the key still holds the old one. Re-pick the device and it will save.

## Development

```bash
cargo test           # unit tests for the playback modes, the registry and the shuffle
cargo build --release
./scripts/install.sh --restart
```

The playback-mode decisions (`decide_key_down`, `decide_key_up`), the player
registry and the random draw (`draw_from_cycle`) are kept free of audio I/O so
they can be tested without a sound device. The parts that actually open a sink
are not covered by tests.

Built with [rodio](https://github.com/RustAudio/rodio) and
[openaction-rs](https://github.com/OpenActionAPI/rust), with AI assistance.

## Licence

See [LICENSE](LICENSE).
