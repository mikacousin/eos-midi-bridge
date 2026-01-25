# Eos MIDI-OSC Bridge (iCon Platform M+)

> [!NOTE]
> This project is in its early stages and was created with Google Antigravity.

A high-performance, asynchronous bridge connecting the **iCon Platform M+** (with D2 Display) to **ETC Eos Family** lighting consoles. Built in Rust for ultra-low latency and reliability.

## ✨ Key Features
- **Live Reconfiguration**: Change Eos IP, ports, or MIDI devices on the fly. No application restart required.
- **Multi-Controller Support**: Simultaneously use multiple supported controllers.
- **Auto-Hardware Detection**: Real-time monitoring of MIDI connections. Plug your iCon while the app is running, it will be detected instantly.
- **Bi-directional Motorized Feedback**: Faders move on your iCon when they move in Eos.
- **D2 Display Support**: Automatically pushes Eos fader labels (e.g., "Front Light", "Haze") to the iCon scribble strips.
- **Fader Touch Sensitivity**: Mutes feedback while you are touching a fader to prevent "motor fighting."
- **Visual Feedback**: Built-in GUI shows active cue, fader levels, and real-time connection status.
- **Page Navigation**: Bank `<` `>` buttons on the iCon move the Eos fader page and refresh all labels.
- **Cross-Platform**: Optimized for Windows, macOS, and Linux.

---

## ⚙️ Eos Console Configuration

To enable communication, go to **Setup > System > Network > OSC** on your Eos console:

1. **OSC RX Port**: `8000`
2. **OSC TX Port**: `8001`
3. **OSC TX IP Address**: The IP of the computer running this bridge.
4. **OSC UDP**: Ensure "UDP" is selected.
5. **OSC TX (Transmit)**: Set to **ON**.

---

## 🎹 Control Hardware Setup

### Tested and Supported Controllers
This bridge has been verified with a multi-controller setup using:

1.  **iCon Platform M+ (with D2 Display)**  
    *   **Mode**: Mackie Control (MCP).  
    *   **Features**: Motorized faders, touch sensitivity, text display (D2), navigation buttons.
    *   *Setup*: Hold Channel 1 Config button while powering on -> Select "MCP".

2.  **Behringer X-Touch Mini**  
    *   **Mode**: Standard Mode (NOT MC Mode).  
    *   **Features**: LED Ring feedback ("Fan" style), rotary encoders, buttons.
    *   *Setup*: Ensure device is in Standard Mode (Layer A/B LEDs active). Global Channel should be **1** (or 0 in internal config).

_Other MIDI controllers can be supported by creating custom JSON profiles in the `profiles/` directory._


---

## 🛠️ Installation

### Download
Grab the latest binary for your OS from the [Releases](https://github.com/YOUR_USERNAME/eos-midi-bridge/releases) tab.

### Manual Build
```bash
git clone https://github.com/mikacousin/eos-midi-bridge.git
cd eos-midi-bridge
cargo build --release
```

🧩 Default Mappings
By default, this app is pre-configured for:  
* Faders 1-9: Eos Faders 1-9 (of current page).  
* Bank < / >: Page Up / Page Down.  
* Scribble Strips: Displays Eos Target Names.
  
🧪 Troubleshooting
* **Check the Status Bar**: The bottom of the application window shows real-time status (e.g., `✓ Connected` or `❌ MIDI Error`).
* **No Labels?** Ensure "OSC TX" is **ON** in Eos and the Target IP in Eos matches your computer's IP.
* **Motors Fighting?** Ensure your iCon is in **MCP** mode so the Touch Sensitivity notes are sent correctly.
* **Windows Firewall**: You must allow UDP traffic on ports 8000 and 8001 when prompted.
