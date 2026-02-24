# IPU6 Camera Daemon

An on-demand camera daemon for Intel IPU6 cameras on Linux. This solves the problem of IPU6 cameras not working with standard V4L2 applications (like Zoom, Google Meet, etc.) while also providing power-efficient on-demand activation.

## The Problem

Intel IPU6 cameras (found in many newer laptops including Dell Precision series) don't expose a standard V4L2 capture device. Instead, they only provide a GStreamer source (`icamerasrc`). Applications like Zoom use V4L2, so a relay is required to bridge GStreamer to V4L2.

Ubuntu's `v4l2-relayd` is supposed to handle this, but it has bugs with `icamerasrc` that cause crashes on stop/start cycles. Additionally, after recent kernel updates (6.x), the `intel-ipu6-dkms` package was removed (functionality moved in-tree), taking its udev rules with it and breaking permissions.

## The Solution

This daemon:

1. **Runs a lightweight placeholder stream** (black video) to keep the v4l2loopback device ready for applications
2. **Detects when an application opens the camera** using inotify
3. **Switches to the real camera** after a configurable delay (filters out brief probes)
4. **Switches back to placeholder** after applications close the camera (with configurable cooldown)

This means your camera LED is only on when actually in use, saving power and providing privacy.

## Requirements

- Intel IPU6 camera (check with `ls /dev/ipu-psys*`)
- `icamerasrc` GStreamer plugin (usually from `intel-ipu6-camera` package)
- `v4l2loopback` kernel module
- `gstreamer1.0-tools` package

### Tested On

- Dell Precision 5490
- Ubuntu 24.04
- Kernel 6.17.x

## Installation

### 1. Install dependencies

```bash
sudo apt install v4l2loopback-dkms gstreamer1.0-tools
```

### 2. Configure v4l2loopback

Create `/etc/modprobe.d/v4l2loopback.conf`:

```
options v4l2loopback devices=1 exclusive_caps=0 card_label="Intel MIPI Camera" max_buffers=8
```

Load the module:

```bash
sudo modprobe v4l2loopback
```

To load on boot, add `v4l2loopback` to `/etc/modules-load.d/v4l2loopback.conf`.

### 3. Set up udev rules for IPU6 permissions

Create `/etc/udev/rules.d/90-intel-ipu6.rules`:

```
SUBSYSTEM=="intel-ipu6-psys", MODE="0660", GROUP="video"
```

Reload udev rules:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Ensure your user is in the `video` group:

```bash
sudo usermod -aG video $USER
```

Log out and back in for group changes to take effect.

### 4. Build the daemon

```bash
# Install Rust if needed: https://rustup.rs/
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/OddballGreg/ipu6-camera-daemon.git
cd ipu6-camera-daemon
cargo build --release
```

### 5. Install systemd service

```bash
mkdir -p ~/.config/systemd/user

cat << 'EOF' > ~/.config/systemd/user/ipu6-camera-daemon.service
[Unit]
Description=Intel IPU6 On-Demand Camera Daemon
After=network.target

[Service]
Type=simple
ExecStart=%h/projects/ipu6-camera-daemon/target/release/ipu6-camera-daemon
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now ipu6-camera-daemon
```

### 6. Disable v4l2-relayd (if installed)

```bash
sudo systemctl disable --now v4l2-relayd@default.service
```

## Usage

The daemon runs automatically via systemd. Your camera will appear as "Intel MIPI Camera" in applications.

### Commands

```bash
# Check status
systemctl --user status ipu6-camera-daemon

# View logs
journalctl --user -u ipu6-camera-daemon -f

# Restart
systemctl --user restart ipu6-camera-daemon
```

### Command Line Options

```
Options:
  -d, --device <DEVICE>              V4L2 loopback device [default: /dev/video0]
  -w, --width <WIDTH>                Video width [default: 1920]
  -H, --height <HEIGHT>              Video height [default: 1080]
  -f, --framerate <FRAMERATE>        Video framerate [default: 30]
  -c, --cooldown-ms <COOLDOWN_MS>    Ms to wait before stopping camera [default: 5000]
  -b, --buffer-count <BUFFER_COUNT>  icamerasrc buffer count [default: 7]
      --poll-interval-ms <MS>        Polling interval [default: 50]
      --activation-delay-ms <MS>     Ms client must be present before activating [default: 1000]
  -h, --help                         Print help
  -V, --version                      Print version
```

## How It Works

```
┌─────────────────────────────────────────────────────────────┐
│                    ipu6-camera-daemon                        │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  ┌─────────────┐    inotify     ┌─────────────────────────┐ │
│  │ /dev/video0 │◄───────────────│  Monitor for clients    │ │
│  │ (loopback)  │                └─────────────────────────┘ │
│  └──────▲──────┘                                            │
│         │                                                    │
│         │ writes                                             │
│         │                                                    │
│  ┌──────┴──────┐         ┌─────────────────────────┐        │
│  │             │         │                         │        │
│  │ gst-launch  │◄────────│  State Machine          │        │
│  │             │ switch  │  - Placeholder (black)  │        │
│  └──────┬──────┘         │  - Camera (icamerasrc)  │        │
│         │                └─────────────────────────┘        │
│         │ reads                                              │
│         ▼                                                    │
│  ┌─────────────┐                                            │
│  │ IPU6 Camera │  (only when client connected > 1 sec)      │
│  └─────────────┘                                            │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

## Troubleshooting

### Camera not detected

1. Check if IPU6 device exists: `ls -la /dev/ipu-psys*`
2. Check permissions: `groups` should show `video`
3. Test icamerasrc: `gst-launch-1.0 icamerasrc ! fakesink`

### Black screen in apps

1. Check daemon is running: `systemctl --user status ipu6-camera-daemon`
2. Check logs: `journalctl --user -u ipu6-camera-daemon -f`
3. Verify v4l2loopback: `v4l2-ctl -d /dev/video0 --all`

### Camera LED always on

The daemon keeps a black placeholder stream running. The actual camera (LED on) only activates when an app uses it for more than 1 second.

## Contributing

Contributions welcome! Please open issues for bugs or feature requests.

## License

MIT License - see LICENSE file.

## Acknowledgments

- Intel for IPU6 Linux support
- The v4l2loopback project
- The GStreamer community
