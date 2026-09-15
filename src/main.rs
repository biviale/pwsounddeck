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

/// Represents a PulseAudio/PipeWire sink with its internal name and human-readable description.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct PaSinkInfo {
    name: String,
    description: String,
}

/// Queries `pactl` for available audio output sinks.
fn get_pulseaudio_sinks() -> Vec<PaSinkInfo> {
    let output = Command::new("pactl")
        .args(["-f", "json", "list", "sinks"])
        .output();

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

/// What `key_down` should do, given the playback mode and whether this
/// button instance already has audio playing.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum KeyDownDecision {
    /// Stop this instance's players and start nothing new (loop toggle, second press).
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
        // Toggle: a second press while the loop is running stops it. This is
        // checked before the path, so clearing the path in the Property
        // Inspector never leaves a loop that its own button cannot stop.
        "loop" if has_active_players => KeyDownDecision::StopOnly,
        // Nothing playing and no file configured: there is nothing to start.
        _ if !has_audio_path => KeyDownDecision::DoNothing,
        "loop" => KeyDownDecision::Play { stop_existing: false, looping: true },
        "stack" => KeyDownDecision::Play { stop_existing: false, looping: false },
        // "restart", "hold" and any unknown mode replace the current playback.
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
        let audio_path = settings.path.clone();
        let target_device_name = settings.device.clone();
        let playback_mode = settings.playback_mode.clone();
        let instance_id_str = instance.instance_id.clone();
        
        let volume_percent = settings.volume.parse::<f32>().unwrap_or(100.0);
        let volume_factor = volume_percent / 100.0;

        info!("key_down triggered for instance {}, mode: {}", instance_id_str, playback_mode);

        let has_audio_path = !settings.path.is_empty();
        let has_active_players = instance_has_players(&instance_id_str);

        let looping = match decide_key_down(&playback_mode, has_active_players, has_audio_path) {
            KeyDownDecision::DoNothing => {
                debug!("Nothing to do for instance {}.", instance_id_str);
                return Ok(());
            }
            KeyDownDecision::StopOnly => {
                // Loop mode, second press: toggle the running loop off.
                stop_instance_players(&instance_id_str);
                return Ok(());
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
        // instance as busy instead of starting a second (never-ending) loop.
        let slot_id = reserve_player_slot(&instance_id_str);

        tokio::task::spawn_blocking(move || {
            // Acquire the lock so we can safely set env vars
            let lock = STREAM_CREATION_LOCK.lock().unwrap_or_else(|e| e.into_inner());

            // Set variables to route audio to the correct device
            if !target_device_name.is_empty() {
                // SAFETY: We hold the STREAM_CREATION_LOCK, so no other thread
                // is concurrently reading/writing env vars for stream creation.
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
                                        player.append(source.repeat_infinite());
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
                                                // A looping source never reports empty, so this
                                                // only ends when the slot is removed.
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
        if let Some(cmd) = payload.get("command").and_then(|c| c.as_str()) {
            match cmd {
                "get_devices" => {
                    let instance_id = instance.instance_id.clone();
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
                    let instance_id = instance.instance_id.clone();
                    tokio::task::spawn_blocking(move || {
                        let file = rfd::FileDialog::new()
                            .add_filter("Audio", &["mp3", "wav", "ogg", "flac"])
                            .pick_file();

                        if let Some(path) = file {
                            let path_str = path.to_string_lossy().to_string();
                            let response = json!({
                                "event": "file_selected",
                                "path": path_str
                            });

                            tokio::spawn(async move {
                                if let Some(instance) =
                                    openaction::get_instance(instance_id).await
                                {
                                    let _ =
                                        instance.send_to_property_inspector(response).await;
                                }
                            });
                        }
                    });
                }
                _ => {}
            }
        }
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
    fn without_a_path_and_without_playback_nothing_happens() {
        assert_eq!(
            decide_key_down("loop", false, false),
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
}
