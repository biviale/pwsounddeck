use dashmap::DashMap;
use log::{debug, error, info};
use openaction::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::File;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

// Global collection of active audio players.
// We store the Player here (which is Send+Sync), along with the OpenAction `instance_id`.
// The MixerDeviceSink (stream) is kept alive in the spawned blocking thread.
static ACTIVE_PLAYERS: LazyLock<DashMap<u64, (String, rodio::Player)>> = LazyLock::new(DashMap::new);

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
        if settings.path.is_empty() {
            debug!("No audio path specified.");
            return Ok(());
        }

        let audio_path = settings.path.clone();
        let target_device_name = settings.device.clone();
        let playback_mode = settings.playback_mode.clone();
        let instance_id_str = instance.instance_id.clone();
        
        let volume_percent = settings.volume.parse::<f32>().unwrap_or(100.0);
        let volume_factor = volume_percent / 100.0;

        info!("key_down triggered for instance {}, mode: {}", instance_id_str, playback_mode);

        if playback_mode == "restart" || playback_mode == "hold" {
            // Stop any existing players playing for this button instance
            let mut keys_to_remove = Vec::new();
            for entry in ACTIVE_PLAYERS.iter() {
                if entry.value().0 == instance_id_str {
                    keys_to_remove.push(*entry.key());
                }
            }
            for key in keys_to_remove {
                if let Some((_, (_, player))) = ACTIVE_PLAYERS.remove(&key) {
                    player.stop();
                }
            }
        } else if playback_mode == "stack" {
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
                    if let Some((_, (_, player))) = ACTIVE_PLAYERS.remove(key) {
                        player.stop();
                    }
                }
            }
        }

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
                                    player.append(source);
                                    let id = STREAM_COUNTER
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    ACTIVE_PLAYERS.insert(id, (instance_id_str, player));

                                    // Poll until the player is empty or has been stopped/removed
                                    loop {
                                        match ACTIVE_PLAYERS.get(&id) {
                                            Some(entry) => {
                                                if entry.value().1.empty() {
                                                    break;
                                                }
                                            }
                                            None => break, // Removed by StopAudioAction or Restart mode
                                        }
                                        std::thread::sleep(std::time::Duration::from_millis(100));
                                    }
                                    // Clean up after playback completes
                                    ACTIVE_PLAYERS.remove(&id);
                                    // stream (MixerDeviceSink) is dropped here, closing the PA connection
                                },
                                Err(e) => error!("Failed to decode audio file: {}", e),
                            }
                        }
                        Err(e) => error!("Failed to open audio file {}: {}", audio_path, e),
                    }
                }
                Err(e) => error!("Failed to open output stream: {}", e),
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
        
        if playback_mode == "hold" {
            // "Hold to Play" mode: stop all audio from this instance when the key is released.
            let mut keys_to_remove = Vec::new();
            for entry in ACTIVE_PLAYERS.iter() {
                if entry.value().0 == instance_id_str {
                    keys_to_remove.push(*entry.key());
                }
            }
            for key in keys_to_remove {
                if let Some((_, (_, player))) = ACTIVE_PLAYERS.remove(&key) {
                    player.stop();
                }
            }
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
            if let Some((_, (_, player))) = ACTIVE_PLAYERS.remove(&key) {
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
