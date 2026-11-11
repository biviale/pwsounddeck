use dashmap::DashMap;
use log::{debug, error, info};
use openaction::*;
use serde::{Deserialize, Serialize};
use rodio::Source;
use serde_json::json;
use std::fs::File;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

// Global collection of active audio players, keyed by slot id.
// We store the Player here (which is Send+Sync), along with the OpenAction `instance_id`.
// The player is `None` between the moment a key press reserves the slot and the
// moment the audio thread has actually built a player: opening the sink and
// decoding take long enough for a second press to land in between.
// The MixerDeviceSink (stream) is kept alive in the spawned blocking thread.
static ACTIVE_PLAYERS: LazyLock<DashMap<u64, (String, Option<rodio::Player>)>> =
    LazyLock::new(DashMap::new);

// Global counter for stream IDs.
static STREAM_COUNTER: LazyLock<std::sync::atomic::AtomicU64> =
    LazyLock::new(|| std::sync::atomic::AtomicU64::new(0));

// Lock to safely set PULSE_SINK before creating the MixerDeviceSink.
static STREAM_CREATION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

// Where each Play Random Audio key stands in its shuffle, keyed by `instance_id`.
// Kept for the life of the plugin, page changes included, so a deleted key's
// entry stays behind: at most one bit per file of its folder.
static SHUFFLE_CYCLES: LazyLock<DashMap<String, ShuffleCycle>> = LazyLock::new(DashMap::new);

/// Represents a PulseAudio/PipeWire sink with its internal name and human-readable description.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct PaSinkInfo {
    name: String,
    description: String,
}

/// Queries `pactl` for available audio output sinks.
fn get_pulseaudio_sinks() -> Vec<PaSinkInfo> {
    // Spawning a child copies the whole environment, so this has to wait for
    // any in-flight PULSE_SINK juggling in `start_playback` (see the SAFETY note
    // there). `pactl` takes ~5 ms, so a key press waits at most that long, and
    // only while the Property Inspector is refreshing its device list.
    let output = {
        let _env_guard = STREAM_CREATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Command::new("pactl")
            .args(["-f", "json", "list", "sinks"])
            .output()
    };

    match output {
        Ok(output) => {
            if !output.status.success() {
                error!("pactl failed: {}", String::from_utf8_lossy(&output.stderr));
                return Vec::new();
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            match serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                Ok(sinks) => sinks
                    .iter()
                    .filter_map(|sink| {
                        let name = sink.get("name")?.as_str()?.to_string();
                        let description = sink
                            .get("description")
                            .and_then(|d| d.as_str())
                            .filter(|d| !d.is_empty() && *d != "(null)")
                            .or_else(|| {
                                sink.get("properties")
                                    .and_then(|p| p.get("device.description"))
                                    .and_then(|d| d.as_str())
                                    .filter(|d| !d.is_empty() && *d != "(null)")
                            })
                            .unwrap_or(name.as_str())
                            .to_string();
                        Some(PaSinkInfo { name, description })
                    })
                    .collect(),
                Err(e) => {
                    error!("Failed to parse pactl JSON: {}", e);
                    Vec::new()
                }
            }
        }
        Err(e) => {
            error!("Failed to run pactl: {}", e);
            Vec::new()
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct AudioSettings {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub device: String,
    #[serde(default = "default_playback_mode")]
    pub playback_mode: String,
    #[serde(default = "default_volume")]
    pub volume: String, // from slider 0-100
}

fn default_playback_mode() -> String {
    "restart".to_string()
}

fn default_volume() -> String {
    "100".to_string()
}

/// Settings of a Play Random Audio key. It has no playback mode: a press cuts
/// the key's current sound and draws another, see `RANDOM_PLAYBACK_MODE`.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct RandomAudioSettings {
    /// The folder the sounds are drawn from.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub device: String,
    #[serde(default = "default_volume")]
    pub volume: String, // from slider 0-100
}

/// The playback mode every Play Random Audio press goes through.
const RANDOM_PLAYBACK_MODE: &str = "restart";

/// What `key_down` should do, given the playback mode and whether this
/// button instance already has audio playing.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyDownDecision {
    /// Stop this instance's players and start nothing new (Loop or Play/Stop, second press).
    StopOnly,
    /// Do nothing at all (no file configured, nothing playing).
    DoNothing,
    /// Start a new playback.
    Play {
        /// Stop this instance's existing players first.
        stop_existing: bool,
        /// Repeat the source until it is stopped.
        looping: bool,
    },
}

/// Decides what `key_down` does for a given playback mode.
pub fn decide_key_down(
    playback_mode: &str,
    has_active_players: bool,
    has_audio_path: bool,
) -> KeyDownDecision {
    match playback_mode {
        // Loop and Play/Stop are toggles: a second press while the sound is
        // running stops it. This is checked before the path, so clearing the
        // path in the Property Inspector never leaves a sound that its own
        // button cannot stop.
        "loop" | "play_stop" if has_active_players => KeyDownDecision::StopOnly,
        // Nothing playing and no file configured: there is nothing to start.
        _ if !has_audio_path => KeyDownDecision::DoNothing,
        "loop" => KeyDownDecision::Play { stop_existing: false, looping: true },
        "stack" => KeyDownDecision::Play { stop_existing: false, looping: false },
        // "restart", "hold" and any unknown mode replace the current playback.
        // So does "play_stop", which only gets here with nothing playing.
        _ => KeyDownDecision::Play { stop_existing: true, looping: false },
    }
}

/// Whether `key_up` should stop this instance's players.
pub fn decide_key_up(playback_mode: &str) -> bool {
    playback_mode == "hold"
}

/// Reserves a registry slot for a press that is about to start playing.
///
/// The slot is inserted before the audio thread is spawned, so a second press
/// arriving while the sink is still opening already sees the instance as busy.
pub fn reserve_player_slot(instance_id: &str) -> u64 {
    let slot_id = STREAM_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ACTIVE_PLAYERS.insert(slot_id, (instance_id.to_string(), None));
    slot_id
}

/// Whether this button instance has any playback running *or starting up*.
pub fn instance_has_players(instance_id: &str) -> bool {
    ACTIVE_PLAYERS
        .iter()
        .any(|entry| entry.value().0 == instance_id)
}

/// Whether a reserved slot is still live, i.e. it has not been stopped.
pub fn slot_is_live(slot_id: u64) -> bool {
    ACTIVE_PLAYERS.contains_key(&slot_id)
}

/// Hands a freshly built player to its reserved slot.
///
/// Returns `false` if the slot was stopped while the audio was starting up, in
/// which case the player is stopped rather than left running unreferenced.
fn attach_player(slot_id: u64, player: rodio::Player) -> bool {
    match ACTIVE_PLAYERS.get_mut(&slot_id) {
        Some(mut slot) => {
            slot.value_mut().1 = Some(player);
            true
        }
        None => {
            player.stop();
            false
        }
    }
}

/// Stops and removes every player belonging to one button instance.
fn stop_instance_players(instance_id: &str) {
    let keys_to_remove: Vec<u64> = ACTIVE_PLAYERS
        .iter()
        .filter(|entry| entry.value().0 == instance_id)
        .map(|entry| *entry.key())
        .collect();
    for key in keys_to_remove {
        // Removing a not-yet-started slot is enough: the audio thread checks
        // whether its slot still exists and gives up if it does not.
        if let Some((_, (_, Some(player)))) = ACTIVE_PLAYERS.remove(&key) {
            player.stop();
        }
    }
}

// ------- Looping -------

/// Repeats `source` until its player is stopped.
///
/// rodio's `repeat_infinite` buffers its source one span at a time and takes
/// an empty span for the end of the sound. Its decoder can open on one: the
/// first packet of an MP3 with encoder delay decodes to nothing. Such a sound
/// played in every mode but Loop, where the buffer saw no sound at all.
/// Holding the first sample back makes the span the buffer starts on never
/// empty, unless the sound really has no sample.
fn looped<S>(source: S) -> impl Source + Send + 'static
where
    S: Source + Send + 'static,
{
    FirstSampleHeld::new(source).repeat_infinite()
}

/// A source whose first sample has already been pulled out of it.
struct FirstSampleHeld<S: Source> {
    first: Option<rodio::Sample>,
    source: S,
}

impl<S: Source> FirstSampleHeld<S> {
    fn new(mut source: S) -> Self {
        let first = source.next();
        Self { first, source }
    }
}

impl<S: Source> Iterator for FirstSampleHeld<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        self.first.take().or_else(|| self.source.next())
    }
}

impl<S: Source> Source for FirstSampleHeld<S> {
    fn current_span_len(&self) -> Option<usize> {
        match (self.first, self.source.current_span_len()) {
            // The held sample comes before whatever the source has left.
            (Some(_), Some(len)) => Some(len + 1),
            (_, len) => len,
        }
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.source.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.source.sample_rate()
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        self.source.total_duration()
    }
}

// ------- Random draws -------

/// Extensions the file picker offers and Play Random Audio draws from.
const AUDIO_EXTENSIONS: [&str; 4] = ["mp3", "wav", "ogg", "flac"];

/// Whether a file name is one this plugin plays. Hidden files are left out:
/// `._song.mp3` is macOS metadata on a USB stick, not a song.
pub fn is_audio_file_name(name: &str) -> bool {
    !name.starts_with('.')
        && std::path::Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                AUDIO_EXTENSIONS
                    .iter()
                    .any(|audio| audio.eq_ignore_ascii_case(extension))
            })
}

/// Where one Play Random Audio key stands in its shuffle. It holds one bit per
/// audio file rather than their names (only the last one played is kept by
/// name), so a 10 000-file folder costs about 1.3 KB per key, and nothing grows
/// from one press to the next.
#[derive(Default)]
pub struct ShuffleCycle {
    /// Hash of the folder's audio file names, in listing order, when the cycle began.
    fingerprint: u64,
    /// How many audio files the folder held when the cycle began.
    count: usize,
    /// Bit `i` is set once the `i`-th audio file of the listing has played this cycle.
    played: Vec<u64>,
    /// How many bits of `played` are set.
    played_count: usize,
    /// The file that played last, kept out of the next draw.
    last: Option<String>,
}

impl ShuffleCycle {
    fn is_played(&self, index: usize) -> bool {
        self.played
            .get(index / 64)
            .is_some_and(|word| word & (1u64 << (index % 64)) != 0)
    }

    fn mark_played(&mut self, index: usize) {
        self.played[index / 64] |= 1u64 << (index % 64);
        self.played_count += 1;
    }
}

/// Draws the next file of a shuffle from `names`, the folder's audio files in
/// listing order. Every file plays once per cycle, and never twice in a row
/// while the folder holds another one.
///
/// `names` is read in a single pass and never collected, so a big folder costs
/// time, not memory. `roll(k)` must return a random index below `k`.
pub fn draw_from_cycle(
    names: impl Iterator<Item = String>,
    cycle: &mut ShuffleCycle,
    mut roll: impl FnMut(usize) -> usize,
) -> Option<String> {
    if cycle.played_count >= cycle.count {
        // Every file has played: a new cycle begins.
        cycle.played.fill(0);
        cycle.played_count = 0;
    }

    let mut hasher = std::hash::DefaultHasher::new();
    let mut count = 0;
    let mut last_index = None;
    // Reservoir sampling: the k-th candidate replaces the pick with probability
    // 1/k, which leaves every candidate equally likely without listing them.
    let (mut any_seen, mut any_pick) = (0, None);
    let (mut unplayed_seen, mut unplayed_pick) = (0, None);
    for (index, name) in names.enumerate() {
        std::hash::Hash::hash(&name, &mut hasher);
        count += 1;
        if cycle.last.as_ref() == Some(&name) {
            last_index = Some(index);
            continue;
        }
        any_seen += 1;
        if roll(any_seen) == 0 {
            any_pick = Some((index, name.clone()));
        }
        if !cycle.is_played(index) {
            unplayed_seen += 1;
            if roll(unplayed_seen) == 0 {
                unplayed_pick = Some((index, name));
            }
        }
    }
    if count == 0 {
        return None;
    }

    let fingerprint = std::hash::Hasher::finish(&hasher);
    let pick = if fingerprint == cycle.fingerprint && count == cycle.count {
        unplayed_pick
    } else {
        // The folder changed since the cycle began, so its bits no longer match
        // the files: start a fresh cycle over the new listing.
        cycle.fingerprint = fingerprint;
        cycle.count = count;
        cycle.played = vec![0; count.div_ceil(64)];
        cycle.played_count = 0;
        any_pick
    };
    // No other candidate: the folder holds only the file that just played.
    let (index, name) = pick.or_else(|| Some((last_index?, cycle.last.clone()?)))?;
    cycle.mark_played(index);
    cycle.last = Some(name.clone());
    Some(name)
}

/// The audio files directly inside `folder`, read lazily from the directory
/// listing: even a 10 000-file folder is never held in memory.
pub fn audio_file_names(folder: &str) -> std::io::Result<impl Iterator<Item = String>> {
    Ok(std::fs::read_dir(folder)?.filter_map(|entry| {
        let entry = entry.ok()?;
        // The shuffle remembers its last file as a `String`, so names that
        // are not UTF-8 are skipped.
        let name = entry.file_name().into_string().ok()?;
        // `file_type` comes with the listing itself: no extra system call.
        (is_audio_file_name(&name) && !entry.file_type().ok()?.is_dir()).then_some(name)
    }))
}

// ------- Playback, shared by both play actions -------

/// Where a key press finds the file it plays.
enum AudioSource {
    /// Play Audio: this one file.
    File(String),
    /// Play Random Audio: a file drawn from this folder, in the audio thread.
    RandomIn(String),
}

/// Draws the file a Play Random Audio key plays next. `None`, logged, when the
/// folder cannot be read or holds no audio file.
fn pick_random_file(instance_id: &str, folder: &str) -> Option<String> {
    let started = std::time::Instant::now();
    let names = match audio_file_names(folder) {
        Ok(names) => names,
        Err(e) => {
            error!("Failed to read folder {}: {}", folder, e);
            return None;
        }
    };
    // The folder is read under this key's entry lock: only another random
    // key's draw can wait on it, never a player or the stream lock.
    let mut cycle = SHUFFLE_CYCLES.entry(instance_id.to_string()).or_default();
    let Some(name) = draw_from_cycle(names, &mut cycle, |k| rand::random_range(0..k)) else {
        error!("No audio file in folder {}", folder);
        return None;
    };
    info!(
        "Instance {} drew {} out of {} audio files in {:?}",
        instance_id,
        name,
        cycle.count,
        started.elapsed()
    );
    Some(std::path::Path::new(folder).join(name).to_string_lossy().into_owned())
}

/// Turns a press's source into the file to play, in the audio thread. `None`
/// means there is nothing to play, and the slot has already been released.
fn resolve_audio_path(source: AudioSource, instance_id: &str, slot_id: u64) -> Option<String> {
    match source {
        AudioSource::File(path) => Some(path),
        AudioSource::RandomIn(folder) => {
            // A second press already cancelled this one: don't spend a draw on it.
            if !slot_is_live(slot_id) {
                return None;
            }
            let file = pick_random_file(instance_id, &folder);
            if file.is_none() {
                // Nothing to play: release the slot, or the key stays busy forever.
                ACTIVE_PLAYERS.remove(&slot_id);
            }
            file
        }
    }
}

/// What a key press does whichever play action it belongs to: stop or stack
/// according to the playback mode, reserve a slot, and play in an audio thread.
fn start_playback(
    instance_id: &str,
    source: AudioSource,
    device: &str,
    volume: &str,
    playback_mode: &str,
) {
    let target_device_name = device.to_string();
    let instance_id_str = instance_id.to_string();

    let volume_percent = volume.parse::<f32>().unwrap_or(100.0);
    let volume_factor = volume_percent / 100.0;

    info!("key_down triggered for instance {}, mode: {}", instance_id_str, playback_mode);

    let has_audio_path = match &source {
        AudioSource::File(path) | AudioSource::RandomIn(path) => !path.is_empty(),
    };
    let has_active_players = instance_has_players(&instance_id_str);

    let looping = match decide_key_down(playback_mode, has_active_players, has_audio_path) {
        KeyDownDecision::DoNothing => {
            debug!("Nothing to do for instance {}.", instance_id_str);
            return;
        }
        KeyDownDecision::StopOnly => {
            // Loop or Play/Stop mode, second press: toggle the running sound off.
            stop_instance_players(&instance_id_str);
            return;
        }
        KeyDownDecision::Play { stop_existing, looping } => {
            if stop_existing {
                stop_instance_players(&instance_id_str);
            }
            looping
        }
    };

    if playback_mode == "stack" {
        // Keep at most 10 stacked sounds. If greater, stop the oldest.
        let mut active_keys: Vec<u64> = ACTIVE_PLAYERS
            .iter()
            .filter(|e| e.value().0 == instance_id_str)
            .map(|e| *e.key())
            .collect();

        if active_keys.len() >= 10 {
            // sort ascending (oldest first)
            active_keys.sort();
            // We want to make room for 1 more, so remove enough to bring it to 9.
            let overage = active_keys.len() - 9;
            for key in active_keys.iter().take(overage) {
                if let Some((_, (_, Some(player)))) = ACTIVE_PLAYERS.remove(key) {
                    player.stop();
                }
            }
        }
    }

    // Reserved before spawning, so a second press during start-up sees this
    // instance as busy: without it a Loop key would start a second, unstoppable
    // loop, and a Play/Stop key a second copy of its sound.
    let slot_id = reserve_player_slot(&instance_id_str);

    tokio::task::spawn_blocking(move || {
        // Drawn here rather than in key_down: listing a folder is blocking I/O,
        // and doing it before the lock below keeps it from delaying other keys.
        let Some(audio_path) = resolve_audio_path(source, &instance_id_str, slot_id) else {
            return;
        };

        // Acquire the lock so we can safely set env vars
        let lock = STREAM_CREATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Set variables to route audio to the correct device
        if !target_device_name.is_empty() {
            // SAFETY: STREAM_CREATION_LOCK is held here and by
            // `get_pulseaudio_sinks`, the other place that reads the
            // environment often enough to race. The `rfd` file and folder
            // dialogs in `handle_property_inspector` still read it unlocked:
            // they block until the user picks, so taking this lock there
            // would freeze every key press for as long as a dialog is open.
            // See issue #2.
            unsafe {
                std::env::set_var("PULSE_SINK", &target_device_name);
                std::env::set_var("PIPEWIRE_NODE", &target_device_name);
            };
        } else {
            unsafe {
                std::env::remove_var("PULSE_SINK");
                std::env::remove_var("PIPEWIRE_NODE");
            };
        }

        // Create the output device sink (binds to the current device specified by env variables)
        let stream_result = rodio::DeviceSinkBuilder::open_default_sink();

        // Reset env vars immediately after binding, then release the lock
        unsafe {
            std::env::remove_var("PULSE_SINK");
            std::env::remove_var("PIPEWIRE_NODE");
        };
        drop(lock);

        match stream_result {
            Ok(mut stream) => {
                stream.log_on_drop(false);
                match File::open(&audio_path) {
                    Ok(file) => {
                        match rodio::Decoder::try_from(file) {
                            Ok(source) => {
                                let player = rodio::Player::connect_new(stream.mixer());
                                player.set_volume(volume_factor);
                                if looping {
                                    player.append(looped(source));
                                } else {
                                    player.append(source);
                                }

                                if !attach_player(slot_id, player) {
                                    // Stopped while we were starting up; nothing to play.
                                    debug!("Slot {} was cancelled during start-up.", slot_id);
                                    return;
                                }

                                // Poll until the player is empty or has been stopped/removed
                                loop {
                                    match ACTIVE_PLAYERS.get(&slot_id) {
                                        Some(entry) => match &entry.value().1 {
                                            // A looping source never reports empty unless the
                                            // sound has no sample, so this otherwise only
                                            // ends when the slot is removed.
                                            Some(player) if player.empty() => break,
                                            _ => {}
                                        },
                                        None => break, // Removed by StopAudioAction or Restart mode
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                }
                                // Clean up after playback completes
                                ACTIVE_PLAYERS.remove(&slot_id);
                                // stream (MixerDeviceSink) is dropped here, closing the PA connection
                            },
                            Err(e) => {
                                error!("Failed to decode audio file: {}", e);
                                ACTIVE_PLAYERS.remove(&slot_id);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to open audio file {}: {}", audio_path, e);
                        ACTIVE_PLAYERS.remove(&slot_id);
                    }
                }
            }
            Err(e) => {
                error!("Failed to open output stream: {}", e);
                ACTIVE_PLAYERS.remove(&slot_id);
            }
        }
    });
}

/// Answers the Property Inspector of either play action.
fn handle_property_inspector(instance_id: String, payload: &serde_json::Value) {
    if let Some(cmd) = payload.get("command").and_then(|c| c.as_str()) {
        match cmd {
            "get_devices" => {
                tokio::spawn(async move {
                    // Pactl blocks briefly, so offload to spawn_blocking.
                    let sinks = tokio::task::spawn_blocking(get_pulseaudio_sinks)
                        .await
                        .unwrap_or_default();

                    let devices: Vec<serde_json::Value> = sinks
                        .iter()
                        .map(|s| {
                            json!({
                                "name": s.name,
                                "description": s.description
                            })
                        })
                        .collect();

                    let response = json!({
                        "event": "device_list",
                        "devices": devices
                    });

                    if let Some(instance) = openaction::get_instance(instance_id).await {
                        let _ = instance.send_to_property_inspector(response).await;
                    }
                });
            }
            "open_file_picker" => {
                tokio::task::spawn_blocking(move || {
                    let file = rfd::FileDialog::new()
                        .add_filter("Audio", &AUDIO_EXTENSIONS)
                        .pick_file();

                    if let Some(path) = file {
                        send_picked_path(instance_id, path);
                    }
                });
            }
            "open_folder_picker" => {
                tokio::task::spawn_blocking(move || {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        send_picked_path(instance_id, path);
                    }
                });
            }
            _ => {}
        }
    }
}

/// Hands the path chosen in a native dialog to the Property Inspector. Files
/// and folders share the `file_selected` event: the field takes either.
fn send_picked_path(instance_id: String, path: std::path::PathBuf) {
    let path_str = path.to_string_lossy().to_string();
    let response = json!({
        "event": "file_selected",
        "path": path_str
    });

    tokio::spawn(async move {
        if let Some(instance) = openaction::get_instance(instance_id).await {
            let _ = instance.send_to_property_inspector(response).await;
        }
    });
}

pub struct PlayAudioAction;

#[async_trait]
impl Action for PlayAudioAction {
    const UUID: &'static str = "com.biviale.pwsounddeck.playaudio";
    type Settings = AudioSettings;

    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        start_playback(
            &instance.instance_id,
            AudioSource::File(settings.path.clone()),
            &settings.device,
            &settings.volume,
            &settings.playback_mode,
        );
        Ok(())
    }

    async fn key_up(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        let playback_mode = settings.playback_mode.clone();
        let instance_id_str = instance.instance_id.clone();
        
        info!("key_up triggered for instance {}, mode: {}", instance_id_str, playback_mode);
        
        if decide_key_up(&playback_mode) {
            // "Hold to Play" mode: stop all audio from this instance when the key is released.
            stop_instance_players(&instance_id_str);
        }
        
        Ok(())
    }

    async fn send_to_plugin(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
        payload: &serde_json::Value,
    ) -> OpenActionResult<()> {
        handle_property_inspector(instance.instance_id.clone(), payload);
        Ok(())
    }
}

// ------- Play Random Audio Action -------

pub struct PlayRandomAudioAction;

#[async_trait]
impl Action for PlayRandomAudioAction {
    const UUID: &'static str = "com.biviale.pwsounddeck.playrandom";
    type Settings = RandomAudioSettings;

    async fn key_down(
        &self,
        instance: &Instance,
        settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        start_playback(
            &instance.instance_id,
            AudioSource::RandomIn(settings.path.clone()),
            &settings.device,
            &settings.volume,
            RANDOM_PLAYBACK_MODE,
        );
        Ok(())
    }

    // No key_up: releasing the key leaves the sound playing to its end.

    async fn send_to_plugin(
        &self,
        instance: &Instance,
        _settings: &Self::Settings,
        payload: &serde_json::Value,
    ) -> OpenActionResult<()> {
        handle_property_inspector(instance.instance_id.clone(), payload);
        Ok(())
    }
}

// ------- Stop Audio Action -------

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct StopAudioSettings {}

pub struct StopAudioAction;

#[async_trait]
impl Action for StopAudioAction {
    const UUID: &'static str = "com.biviale.pwsounddeck.stopaudio";
    type Settings = StopAudioSettings;

    async fn key_down(
        &self,
        _instance: &Instance,
        _settings: &Self::Settings,
    ) -> OpenActionResult<()> {
        info!("Stopping all audio playback...");
        // Explicitly stop each player before removing it.
        // This avoids deadlocking with the playback threads.
        let keys: Vec<u64> = ACTIVE_PLAYERS.iter().map(|entry| *entry.key()).collect();
        for key in keys {
            if let Some((_, (_, Some(player)))) = ACTIVE_PLAYERS.remove(&key) {
                player.stop();
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> OpenActionResult<()> {
    {
        use simplelog::*;
        // OpenDeck captures plugin stderr to <log_dir>/plugins/<uuid>.log.
        // WriteLogger to stderr ensures all log!() macros are visible there.
        // TermLogger would silently fail because stdout/stderr are not TTYs
        // when the plugin is spawned as a child process.
        WriteLogger::init(
            LevelFilter::Info,
            Config::default(),
            std::io::stderr(),
        )
        .expect("Failed to initialize logger");
    }

    info!("Starting audio plugin...");
    register_action(PlayAudioAction).await;
    register_action(PlayRandomAudioAction).await;
    register_action(StopAudioAction).await;
    run(std::env::args().collect()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_mode_starts_a_looping_playback_when_nothing_is_playing() {
        assert_eq!(
            decide_key_down("loop", false, true),
            KeyDownDecision::Play { stop_existing: false, looping: true }
        );
    }

    #[test]
    fn loop_mode_stops_playback_on_second_press() {
        assert_eq!(decide_key_down("loop", true, true), KeyDownDecision::StopOnly);
    }

    #[test]
    fn loop_mode_does_not_stop_on_key_up() {
        assert!(!decide_key_up("loop"));
    }

    #[test]
    fn play_stop_mode_plays_once_when_nothing_is_playing() {
        assert_eq!(
            decide_key_down("play_stop", false, true),
            KeyDownDecision::Play { stop_existing: true, looping: false }
        );
    }

    #[test]
    fn play_stop_mode_stops_playback_on_second_press() {
        assert_eq!(decide_key_down("play_stop", true, true), KeyDownDecision::StopOnly);
    }

    #[test]
    fn play_stop_mode_does_not_stop_on_key_up() {
        assert!(!decide_key_up("play_stop"));
    }

    #[test]
    fn restart_mode_replaces_the_current_playback() {
        assert_eq!(
            decide_key_down("restart", true, true),
            KeyDownDecision::Play { stop_existing: true, looping: false }
        );
    }

    #[test]
    fn stack_mode_adds_a_playback_without_stopping_the_others() {
        assert_eq!(
            decide_key_down("stack", true, true),
            KeyDownDecision::Play { stop_existing: false, looping: false }
        );
    }

    #[test]
    fn hold_mode_replaces_on_press_and_stops_on_release() {
        assert_eq!(
            decide_key_down("hold", true, true),
            KeyDownDecision::Play { stop_existing: true, looping: false }
        );
        assert!(decide_key_up("hold"));
    }

    #[test]
    fn a_running_loop_can_be_stopped_even_after_the_path_was_cleared() {
        assert_eq!(
            decide_key_down("loop", true, false),
            KeyDownDecision::StopOnly
        );
    }

    #[test]
    fn a_play_stop_sound_can_be_stopped_even_after_the_path_was_cleared() {
        assert_eq!(
            decide_key_down("play_stop", true, false),
            KeyDownDecision::StopOnly
        );
    }

    #[test]
    fn without_a_path_and_without_playback_nothing_happens() {
        assert_eq!(
            decide_key_down("loop", false, false),
            KeyDownDecision::DoNothing
        );
        assert_eq!(
            decide_key_down("play_stop", false, false),
            KeyDownDecision::DoNothing
        );
        assert_eq!(
            decide_key_down("restart", false, false),
            KeyDownDecision::DoNothing
        );
    }

    #[test]
    fn unknown_mode_falls_back_to_restart_behaviour() {
        assert_eq!(
            decide_key_down("something-else", true, true),
            KeyDownDecision::Play { stop_existing: true, looping: false }
        );
        assert!(!decide_key_up("something-else"));
    }

    #[test]
    fn a_reserved_slot_makes_the_instance_active_before_audio_starts() {
        let id = "test-instance-reserve";
        assert!(!instance_has_players(id));

        let slot = reserve_player_slot(id);

        assert!(
            instance_has_players(id),
            "a slot must count as active while the audio thread is still opening the sink"
        );
        assert!(slot_is_live(slot));
        stop_instance_players(id);
    }

    #[test]
    fn a_second_press_while_the_loop_is_starting_up_stops_it_instead_of_stacking() {
        let id = "test-instance-race";
        reserve_player_slot(id);

        assert_eq!(
            decide_key_down("loop", instance_has_players(id), true),
            KeyDownDecision::StopOnly
        );
        stop_instance_players(id);
    }

    #[test]
    fn stopping_an_instance_cancels_a_slot_that_has_not_started_yet() {
        let id = "test-instance-cancel";
        let slot = reserve_player_slot(id);

        stop_instance_players(id);

        assert!(!slot_is_live(slot));
        assert!(!instance_has_players(id));
    }

    #[test]
    fn stopping_one_instance_leaves_other_instances_playing() {
        let a = "test-instance-a";
        let b = "test-instance-b";
        reserve_player_slot(b);
        reserve_player_slot(a);

        stop_instance_players(a);

        assert!(!instance_has_players(a));
        assert!(
            instance_has_players(b),
            "stopping one button must not touch another button's audio"
        );
        stop_instance_players(b);
    }

    // ------- Looping -------

    /// A sound handed out one span at a time, the way a decoder hands out its
    /// packets. Like rodio's decoder, it reports the length of the span it is
    /// in and only moves on to the next one when asked for a sample.
    struct Spans {
        spans: Vec<Vec<f32>>,
        span: usize,
        offset: usize,
    }

    fn spans(spans: &[&[f32]]) -> Spans {
        Spans { spans: spans.iter().map(|span| span.to_vec()).collect(), span: 0, offset: 0 }
    }

    impl Iterator for Spans {
        type Item = f32;

        fn next(&mut self) -> Option<f32> {
            while self.span < self.spans.len() && self.offset >= self.spans[self.span].len() {
                self.span += 1;
                self.offset = 0;
            }
            let sample = *self.spans.get(self.span)?.get(self.offset)?;
            self.offset += 1;
            Some(sample)
        }
    }

    impl Source for Spans {
        fn current_span_len(&self) -> Option<usize> {
            Some(self.spans.get(self.span).map_or(0, Vec::len))
        }

        fn channels(&self) -> rodio::ChannelCount {
            rodio::ChannelCount::new(1).unwrap()
        }

        fn sample_rate(&self) -> rodio::SampleRate {
            rodio::SampleRate::new(44_100).unwrap()
        }

        fn total_duration(&self) -> Option<std::time::Duration> {
            None
        }
    }

    #[test]
    fn a_loop_plays_a_sound_whose_first_span_is_empty() {
        // What rodio's decoder gives for an MP3 with encoder delay: nothing in
        // the first packet, the sound in the ones after it.
        let sound = spans(&[&[], &[0.1, 0.2], &[0.3]]);

        let played: Vec<f32> = looped(sound).take(7).collect();

        assert_eq!(played, [0.1, 0.2, 0.3, 0.1, 0.2, 0.3, 0.1]);
    }

    #[test]
    fn a_loop_repeats_a_sound_from_its_first_sample_to_its_last() {
        let sound = spans(&[&[0.1, 0.2], &[0.3]]);

        let played: Vec<f32> = looped(sound).take(7).collect();

        assert_eq!(played, [0.1, 0.2, 0.3, 0.1, 0.2, 0.3, 0.1]);
    }

    #[test]
    fn a_loop_of_a_sound_with_no_samples_ends_instead_of_spinning() {
        let played: Vec<f32> = looped(spans(&[&[]])).take(5).collect();

        assert!(played.is_empty());
    }

    // ------- Random draws -------

    const ABC: [&str; 3] = ["a.mp3", "b.mp3", "c.mp3"];
    const ABCDE: [&str; 5] = ["a.mp3", "b.mp3", "c.mp3", "d.mp3", "e.mp3"];

    fn listing(names: &[&str]) -> impl Iterator<Item = String> {
        names.iter().map(|name| name.to_string())
    }

    /// Draws `presses` times from an unchanging folder, as consecutive key presses would.
    fn draws(names: &[&str], presses: usize, mut roll: impl FnMut(usize) -> usize) -> Vec<String> {
        let mut cycle = ShuffleCycle::default();
        (0..presses)
            .map(|_| {
                draw_from_cycle(listing(names), &mut cycle, &mut roll)
                    .expect("a folder with sounds in it always has one to play")
            })
            .collect()
    }

    // Deterministic rolls: `roll(k) == 0` makes the k-th candidate the pick.
    fn first_candidate(k: usize) -> usize {
        k - 1
    }

    fn latest_candidate(_: usize) -> usize {
        0
    }

    fn real_roll(k: usize) -> usize {
        rand::random_range(0..k)
    }

    #[test]
    fn audio_files_are_recognised_by_extension_whatever_the_case() {
        for name in ["a.mp3", "b.WAV", "c.Ogg", "d.flac"] {
            assert!(is_audio_file_name(name), "{name}");
        }
    }

    #[test]
    fn other_files_and_hidden_files_are_not_audio() {
        for name in ["notes.txt", "mp3", "song.mp3.part", ".cache.mp3", "._song.mp3"] {
            assert!(!is_audio_file_name(name), "{name}");
        }
    }

    #[test]
    fn an_empty_folder_has_nothing_to_play() {
        let mut cycle = ShuffleCycle::default();
        // The real roll panics on an empty range, which would kill the audio thread.
        let roll = |_| -> usize { panic!("nothing to roll for in an empty folder") };
        assert_eq!(draw_from_cycle(std::iter::empty(), &mut cycle, roll), None);
    }

    #[test]
    fn a_shuffle_plays_every_sound_once_before_repeating_one() {
        for picks in [draws(&ABC, 3, first_candidate), draws(&ABC, 3, latest_candidate)] {
            let mut sorted = picks.clone();
            sorted.sort();
            assert_eq!(sorted, ABC, "drawn: {picks:?}");
        }
    }

    #[test]
    fn a_new_cycle_never_starts_with_the_sound_that_just_played() {
        let mut cycle = ShuffleCycle::default();
        // Taking the first candidate each time plays the cycle in listing order: a, b, c.
        for _ in 0..3 {
            draw_from_cycle(listing(&ABC), &mut cycle, first_candidate);
        }
        // Taking the latest candidate would now pick c.mp3 again, were it allowed.
        let next = draw_from_cycle(listing(&ABC), &mut cycle, latest_candidate);
        assert_eq!(next.as_deref(), Some("b.mp3"));
    }

    #[test]
    fn a_folder_with_a_single_sound_plays_it_every_time() {
        let mut cycle = ShuffleCycle::default();
        for _ in 0..3 {
            let pick = draw_from_cycle(listing(&["a.mp3"]), &mut cycle, latest_candidate);
            assert_eq!(pick.as_deref(), Some("a.mp3"));
        }
    }

    #[test]
    fn a_sound_added_to_the_folder_starts_a_fresh_cycle() {
        let mut cycle = ShuffleCycle::default();
        let first = draw_from_cycle(listing(&ABC), &mut cycle, first_candidate);
        assert_eq!(first.as_deref(), Some("a.mp3"));
        // z.mp3 is now listed where a.mp3 was: a stale cycle would think it had played.
        let changed = ["z.mp3", "a.mp3", "b.mp3", "c.mp3"];
        let next = draw_from_cycle(listing(&changed), &mut cycle, first_candidate);
        assert_eq!(next.as_deref(), Some("z.mp3"));
    }

    #[test]
    fn a_renamed_sound_starts_a_fresh_cycle() {
        let mut cycle = ShuffleCycle::default();
        let first = draw_from_cycle(listing(&ABC), &mut cycle, first_candidate);
        assert_eq!(first.as_deref(), Some("a.mp3"));
        // b.mp3 became y.mp3, listed first: same count, but not the same files.
        let renamed = ["y.mp3", "a.mp3", "c.mp3"];
        let next = draw_from_cycle(listing(&renamed), &mut cycle, first_candidate);
        assert_eq!(next.as_deref(), Some("y.mp3"));
    }

    #[test]
    fn a_folder_changed_mid_cycle_gets_a_full_new_cycle() {
        let mut cycle = ShuffleCycle::default();
        // Two of three sounds played: the cycle is two draws in.
        for _ in 0..2 {
            draw_from_cycle(listing(&ABC), &mut cycle, first_candidate);
        }
        let mut picks: Vec<String> = (0..ABCDE.len())
            .map(|_| draw_from_cycle(listing(&ABCDE), &mut cycle, first_candidate).unwrap())
            .collect();
        picks.sort();
        assert_eq!(picks, ABCDE);
    }

    #[test]
    fn a_shuffle_never_plays_the_same_sound_twice_in_a_row() {
        let picks = draws(&ABCDE, 1_000, real_roll);
        for pair in picks.windows(2) {
            assert_ne!(pair[0], pair[1], "drawn twice in a row");
        }
    }

    #[test]
    fn every_cycle_plays_every_sound_once() {
        let picks = draws(&ABCDE, 1_000, real_roll);
        for one_cycle in picks.chunks(ABCDE.len()) {
            let mut sorted = one_cycle.to_vec();
            sorted.sort();
            assert_eq!(sorted, ABCDE, "one cycle drew {one_cycle:?}");
        }
    }

    #[test]
    fn a_ten_thousand_file_folder_costs_one_bit_per_file() {
        let mut cycle = ShuffleCycle::default();
        let names = (0..10_000).map(|i| format!("{i}.mp3"));
        draw_from_cycle(names, &mut cycle, real_roll);
        // 10 000 bits round up to 157 words of 64 bits.
        let bytes = cycle.played.capacity() * 8;
        assert!(bytes <= 1_256, "the cycle holds {bytes} bytes");
    }

    /// A directory of its own for one test, removed when the test ends, even on failure.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("pwsounddeck-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }

        fn touch(&self, name: &str) {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            File::create(path).unwrap();
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_audio_files_directly_inside_the_folder_are_listed() {
        let dir = TestDir::new("listing");
        for name in ["x.mp3", "Y.WAV", "notes.txt", ".cache.mp3", "sub.mp3/z.mp3"] {
            dir.touch(name);
        }
        let mut names: Vec<String> = audio_file_names(dir.path()).unwrap().collect();
        names.sort();
        assert_eq!(names, ["Y.WAV", "x.mp3"]);
    }

    #[test]
    fn listing_a_missing_folder_is_an_error() {
        assert!(audio_file_names("/nonexistent/pwsounddeck-test").is_err());
    }

    #[test]
    fn a_random_press_plays_a_file_from_its_folder() {
        let id = "test-instance-random-draw";
        let dir = TestDir::new("random-draw");
        dir.touch("a.mp3");
        let slot = reserve_player_slot(id);

        let file = resolve_audio_path(AudioSource::RandomIn(dir.path().to_string()), id, slot);

        assert_eq!(file, Some(format!("{}/a.mp3", dir.path())));
        assert!(slot_is_live(slot), "the slot stays reserved for the player");
        stop_instance_players(id);
        SHUFFLE_CYCLES.remove(id);
    }

    #[test]
    fn an_empty_random_folder_releases_its_slot() {
        let id = "test-instance-random-empty";
        let dir = TestDir::new("random-empty");
        let slot = reserve_player_slot(id);

        let file = resolve_audio_path(AudioSource::RandomIn(dir.path().to_string()), id, slot);

        assert_eq!(file, None);
        assert!(!slot_is_live(slot), "with nothing to play, the key must not stay busy");
        SHUFFLE_CYCLES.remove(id);
    }

    #[test]
    fn a_missing_random_folder_releases_its_slot() {
        let id = "test-instance-random-missing";
        let slot = reserve_player_slot(id);

        let file = resolve_audio_path(
            AudioSource::RandomIn("/nonexistent/pwsounddeck-test".to_string()),
            id,
            slot,
        );

        assert_eq!(file, None);
        assert!(!slot_is_live(slot), "with nothing to play, the key must not stay busy");
        SHUFFLE_CYCLES.remove(id);
    }

    #[test]
    fn a_cancelled_random_press_spends_no_draw() {
        let id = "test-instance-random-cancelled";
        let dir = TestDir::new("random-cancelled");
        dir.touch("a.mp3");
        let slot = reserve_player_slot(id);
        // A second press got there first and stopped this one.
        stop_instance_players(id);

        let file = resolve_audio_path(AudioSource::RandomIn(dir.path().to_string()), id, slot);

        assert_eq!(file, None);
        assert!(
            !SHUFFLE_CYCLES.contains_key(id),
            "a cancelled press must not move the shuffle on"
        );
    }
}
