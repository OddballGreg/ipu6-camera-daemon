#!/bin/bash
set -e

# IPU6 Camera Daemon Installer for Ubuntu
# https://github.com/OddballGreg/ipu6-camera-daemon

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}=== IPU6 Camera Daemon Installer ===${NC}"
echo ""

# Check if running as root
if [[ $EUID -eq 0 ]]; then
   echo -e "${RED}Error: Don't run this script as root. It will ask for sudo when needed.${NC}"
   exit 1
fi

# Check for IPU6 device
if ! ls /dev/ipu-psys* &>/dev/null; then
    echo -e "${RED}Error: No IPU6 device found (/dev/ipu-psys*).${NC}"
    echo "This installer is for Intel IPU6 cameras only."
    exit 1
fi

echo -e "${GREEN}✓ IPU6 device detected${NC}"

# Determine install location
INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"

# Check if we're running from git repo with prebuilt binary or need to download
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -f "$SCRIPT_DIR/target/release/ipu6-camera-daemon" ]]; then
    BINARY_SOURCE="$SCRIPT_DIR/target/release/ipu6-camera-daemon"
    echo "Installing from local build..."
elif [[ -f "$SCRIPT_DIR/ipu6-camera-daemon" ]]; then
    BINARY_SOURCE="$SCRIPT_DIR/ipu6-camera-daemon"
    echo "Installing from release package..."
else
    echo "Downloading latest release..."
    BINARY_SOURCE="/tmp/ipu6-camera-daemon"
    curl -fsSL "https://github.com/OddballGreg/ipu6-camera-daemon/releases/latest/download/ipu6-camera-daemon-linux-x86_64" -o "$BINARY_SOURCE"
    chmod +x "$BINARY_SOURCE"
fi

# Install base dependencies
echo ""
echo -e "${YELLOW}Installing system dependencies (requires sudo)...${NC}"
sudo apt-get update
sudo apt-get install -y v4l2loopback-dkms gstreamer1.0-tools

# Check if icamerasrc is available
echo ""
echo -e "${YELLOW}Checking for icamerasrc GStreamer plugin...${NC}"
if ! gst-inspect-1.0 icamerasrc &>/dev/null; then
    echo -e "${YELLOW}icamerasrc not found. Installing Intel IPU6 camera packages...${NC}"
    
    # Add Intel IPU6 PPA
    if ! grep -q "oem-solutions-group/intel-ipu6" /etc/apt/sources.list.d/*.list 2>/dev/null; then
        echo -e "${YELLOW}Adding Intel IPU6 PPA...${NC}"
        sudo add-apt-repository -y ppa:oem-solutions-group/intel-ipu6
        sudo apt-get update
    fi
    
    # Install IPU6 packages
    # Package names vary by Ubuntu version, try common ones
    sudo apt-get install -y \
        gstreamer1.0-icamera \
        libcamhal-ipu6ep \
        libipu6ep \
        2>/dev/null || \
    sudo apt-get install -y \
        gstreamer1.0-icamera \
        libcamhal0 \
        2>/dev/null || \
    echo -e "${YELLOW}Warning: Could not auto-install IPU6 packages. You may need to install them manually.${NC}"
    
    # Verify installation
    if gst-inspect-1.0 icamerasrc &>/dev/null; then
        echo -e "${GREEN}✓ icamerasrc installed successfully${NC}"
    else
        echo -e "${RED}Warning: icamerasrc still not available.${NC}"
        echo -e "${YELLOW}You may need to manually install the Intel IPU6 camera packages for your system.${NC}"
        echo -e "${YELLOW}See: https://launchpad.net/~oem-solutions-group/+archive/ubuntu/intel-ipu6${NC}"
        echo ""
        read -p "Continue anyway? (y/N) " -n 1 -r
        echo
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then
            exit 1
        fi
    fi
else
    echo -e "${GREEN}✓ icamerasrc already installed${NC}"
fi

# Configure v4l2loopback
echo ""
echo -e "${YELLOW}Configuring v4l2loopback...${NC}"
sudo tee /etc/modprobe.d/v4l2loopback.conf > /dev/null << 'MODCONF'
options v4l2loopback devices=1 exclusive_caps=0 card_label="Intel MIPI Camera" max_buffers=8
MODCONF

# Ensure module loads on boot
echo "v4l2loopback" | sudo tee /etc/modules-load.d/v4l2loopback.conf > /dev/null

# Load module now
sudo modprobe -r v4l2loopback 2>/dev/null || true
sudo modprobe v4l2loopback

echo -e "${GREEN}✓ v4l2loopback configured${NC}"

# Set up udev rules for IPU6 permissions
echo ""
echo -e "${YELLOW}Setting up udev rules...${NC}"
sudo tee /etc/udev/rules.d/90-intel-ipu6.rules > /dev/null << 'UDEV'
SUBSYSTEM=="intel-ipu6-psys", MODE="0660", GROUP="video"
UDEV

sudo udevadm control --reload-rules
sudo udevadm trigger

# Add user to video group
if ! groups | grep -q video; then
    echo -e "${YELLOW}Adding $USER to video group...${NC}"
    sudo usermod -aG video "$USER"
    echo -e "${YELLOW}NOTE: You'll need to log out and back in for group changes to take effect.${NC}"
fi

echo -e "${GREEN}✓ udev rules configured${NC}"

# Install binary
echo ""
echo -e "${YELLOW}Installing daemon...${NC}"
cp "$BINARY_SOURCE" "$INSTALL_DIR/ipu6-camera-daemon"
chmod +x "$INSTALL_DIR/ipu6-camera-daemon"
echo -e "${GREEN}✓ Installed to $INSTALL_DIR/ipu6-camera-daemon${NC}"

# Install systemd service
echo ""
echo -e "${YELLOW}Installing systemd service...${NC}"
mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/ipu6-camera-daemon.service" << EOF
[Unit]
Description=Intel IPU6 On-Demand Camera Daemon
After=network.target

[Service]
Type=simple
ExecStart=$INSTALL_DIR/ipu6-camera-daemon
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
EOF

# Disable and mask conflicting services
echo ""
echo -e "${YELLOW}Disabling conflicting services...${NC}"

# Disable v4l2-relayd if present
if systemctl list-unit-files 2>/dev/null | grep -q v4l2-relayd; then
    echo "  Disabling v4l2-relayd..."
    sudo systemctl stop v4l2-relayd@default.service 2>/dev/null || true
    sudo systemctl stop v4l2-relayd.service 2>/dev/null || true
    sudo systemctl disable v4l2-relayd@default.service 2>/dev/null || true
    sudo systemctl disable v4l2-relayd.service 2>/dev/null || true
    sudo systemctl mask v4l2-relayd@default.service 2>/dev/null || true
    sudo systemctl mask v4l2-relayd.service 2>/dev/null || true
fi

# Remove old ipu6-camera.service if it exists (from earlier testing)
if [[ -f /etc/systemd/system/ipu6-camera.service ]]; then
    echo "  Removing old ipu6-camera.service..."
    sudo systemctl stop ipu6-camera.service 2>/dev/null || true
    sudo systemctl disable ipu6-camera.service 2>/dev/null || true
    sudo rm -f /etc/systemd/system/ipu6-camera.service
    sudo systemctl daemon-reload
fi

echo -e "${GREEN}✓ Conflicting services disabled${NC}"

# Enable and start the service
systemctl --user daemon-reload
systemctl --user enable ipu6-camera-daemon.service
systemctl --user stop ipu6-camera-daemon.service 2>/dev/null || true

# Kill any leftover gst-launch processes
pkill -f "gst-launch.*v4l2sink.*video0" 2>/dev/null || true
sleep 1

systemctl --user start ipu6-camera-daemon.service

echo -e "${GREEN}✓ Service installed and started${NC}"

# Verify
echo ""
echo -e "${GREEN}=== Installation Complete ===${NC}"
echo ""
systemctl --user status ipu6-camera-daemon.service --no-pager || true
echo ""
echo -e "Your camera should now appear as '${YELLOW}Intel MIPI Camera${NC}' in applications."
echo ""
echo "Useful commands:"
echo "  Status:  systemctl --user status ipu6-camera-daemon"
echo "  Logs:    journalctl --user -u ipu6-camera-daemon -f"
echo "  Restart: systemctl --user restart ipu6-camera-daemon"
echo ""
echo -e "${YELLOW}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${YELLOW}║  IMPORTANT: A reboot is recommended for all changes to take    ║${NC}"
echo -e "${YELLOW}║  effect (udev rules, kernel modules, group membership).        ║${NC}"
echo -e "${YELLOW}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""
if ! groups | grep -q video; then
    echo -e "${YELLOW}⚠ You were added to the video group - reboot required for this.${NC}"
fi
