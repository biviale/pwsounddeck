# PWSoundDeck

A native Rust plugin for [OpenDeck](https://github.com/v-b-boy/OpenDeck). It brings advanced and robust audio playback capabilities straight to a Stream Deck setup. Built with [rodio](https://github.com/RustAudio/rodio), [openaction-rs](https://github.com/v-b-boy/openaction-rs) and Gemini 3.1.

## Features

- **Play Audio Files**: Route `.mp3`, `.wav`, `.ogg`, or `.flac` files directly from your computer to any output device.
- **Stop Audio via Button**: Includes a dedicated "Stop Audio" action to instantly stop all active playback instances.
- **Per-Action Volume Slider**: Set playback volume (0% to 100%) independently for every button.
- **Granular Playback Modes**:
  - **Restart**: Pressing the button stops any audio triggered by that specific button and plays it from the start.
  - **Stack**: Pressing the button layers the audio multiple times (up to 10 concurrent streams per button to preserve CPU/RAM).
  - **Hold to Play (Push-to-Talk)**: The audio plays immediately when pressed and cuts off identically to a walkie-talkie as soon as the button is released!
- **Target Output Devices**: Uses `pactl` alongside environment variables (`PULSE_SINK` & `PIPEWIRE_NODE`) to ensure audio routes specifically to the selected device, even fully bypassing the default system audio!
- **Stream Deck Aesthetic**: The property inspector perfectly blends into the dark-mode OpenDeck style.

## Building and Installation

### Prerequisites

- [Rust & Cargo](https://rustup.rs/)
- PulseAudio CLI utilities: `pactl` should be installed and accessible.

### Compiling

Clone the repository and compile the native Rust binary:

```bash
git clone https://github.com/biviale/pwsounddeck.git
cd pwsounddeck/com.biviale.pwsounddeck.sdPlugin

# Build the release binary
cargo build --release

# Copy the generated binary out of the target folder
cp target/release/pwsounddeck ./
```

### Installation

Copy the compiled `.sdPlugin` folder into OpenDeck's plugin directory:

```bash
# assuming you are still in pwsounddeck/com.biviale.pwsounddeck.sdPlugin
cp -r ../com.biviale.pwsounddeck.sdPlugin ~/.config/opendeck/plugins/
```

Restart OpenDeck. The actions will appear under the **PWSoundDeck** category!

## Usage

1. Drag the **Play Audio** action onto any key.
2. In the Property Inspector below:
   - Click **Browse** and select an audio file.
   - Adjust the **Volume** slider.
   - Select your preferred **Output Device**.
   - Select your preferred **Playback Mode** (Restart vs Stack vs Hold to Play).
3. (Optional) Drag a **Stop Audio** action to immediately kill all playing streams universally.
