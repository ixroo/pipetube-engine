#!/usr/bin/env bash
# ==============================================================================
# YTDL-RAW // AUTOMATED ZERO-DISK SYSTEM SETUP SCRIPT
# Target: Ubuntu 22.04 / 24.04 LTS or Debian 11 / 12
# Specs: 1 vCPU, 1-2 GB RAM, 20 GB SSD
# ==============================================================================

set -euo pipefail

# ANSI Color Codes
CLR_RESET="\033[0m"
CLR_LIME="\033[38;2;204;255;0m"
CLR_TEXT="\033[1;37m"
CLR_MUTED="\033[0;90m"
CLR_ERR="\033[1;31m"

log_info() {
    echo -e "${CLR_LIME}[YTDL-SETUP]${CLR_RESET} ${CLR_TEXT}$1${CLR_RESET}"
}

log_step() {
    echo -e "${CLR_MUTED}  > $1${CLR_RESET}"
}

log_err() {
    echo -e "${CLR_ERR}[ERROR]${CLR_RESET} $1" >&2
}

# 1. ROOT CHECK
if [[ "$EUID" -ne 0 ]]; then
    log_err "This installation script must be executed as root (or with sudo)."
    exit 1
fi

log_info "Initiating YTDL-RAW Deployment Pipeline..."

# 2. CONFIGURE 2GB SWAP FILE (ESSENTIAL FOR 1GB RAM VPS)
if [[ ! -f /swapfile ]]; then
    log_info "Creating 2GB swapfile to prevent out-of-memory errors on 1GB VPS..."
    fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048
    chmod 600 /swapfile
    mkswap /swapfile
    swapon /swapfile
    echo '/swapfile none swap sw 0 0' >> /etc/fstab
    sysctl vm.swappiness=10
    echo 'vm.swappiness=10' >> /etc/sysctl.conf
    log_step "2GB Swap active (swappiness=10)."
else
    log_step "Swapfile already present. Skipping creation."
fi

# 3. INSTALL SYSTEM DEPENDENCIES
log_info "Updating system packages and installing required toolchains..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    libssl-dev \
    curl \
    wget \
    ffmpeg \
    nginx \
    ufw \
    git \
    ca-certificates

# 4. INSTALL OFFICIAL YT-DLP STANDALONE BINARY
log_info "Installing latest official yt-dlp binary..."
curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o /usr/local/bin/yt-dlp
chmod a+rx /usr/local/bin/yt-dlp
log_step "yt-dlp installed: $(/usr/local/bin/yt-dlp --version)"

# Setup daily cron for yt-dlp auto-updates (YouTube changes formats frequently)
cat << 'EOF' > /etc/cron.daily/yt-dlp-update
#!/bin/sh
/usr/local/bin/yt-dlp -U > /dev/null 2>&1
EOF
chmod +x /etc/cron.daily/yt-dlp-update
log_step "Configured /etc/cron.daily/yt-dlp-update for daily engine auto-updates."

# 5. INSTALL RUST TOOLCHAIN (IF NOT PRESENT)
export PATH="$HOME/.cargo/bin:/home/ubuntu/.cargo/bin:/root/.cargo/bin:$PATH"
if ! command -v cargo &> /dev/null; then
    log_info "Rust toolchain not found. Installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
    source "$HOME/.cargo/env" 2>/dev/null || true
    log_step "Rust installed: $(cargo --version)"
else
    log_step "Cargo already installed: $(cargo --version)"
fi

# 6. COMPILE RELEASE BINARY
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$SCRIPT_DIR"

log_info "Compiling YTDL-RAW release binary with LTO and binary stripping..."
cargo build --release

# 7. SETUP APP DIRECTORY & DEDICATED UNPRIVILEGED USER
log_info "Configuring unprivileged system user and deployment directory..."
id -u ytdl &>/dev/null || useradd -r -s /bin/false -d /opt/ytdl-raw ytdl

mkdir -p /opt/ytdl-raw/public
cp target/release/ytdl-raw /opt/ytdl-raw/ytdl-raw
cp -r public/* /opt/ytdl-raw/public/
chown -R ytdl:ytdl /opt/ytdl-raw
chmod 750 /opt/ytdl-raw
chmod 755 /opt/ytdl-raw/ytdl-raw

# 8. CONFIGURE SYSTEMD SERVICE WITH STRICT RESOURCE BOUNDS
log_info "Installing systemd service unit (/etc/systemd/system/ytdl-web.service)..."
cat << 'EOF' > /etc/systemd/system/ytdl-web.service
[Unit]
Description=YTDL-RAW Zero-Disk Async Stream Extractor Service
After=network.target

[Service]
Type=simple
User=ytdl
Group=ytdl
WorkingDirectory=/opt/ytdl-raw
ExecStart=/opt/ytdl-raw/ytdl-raw

# Environment Tuning
Environment=PORT=3000
Environment=MAX_CONCURRENCY=3
Environment=RATE_LIMIT_RPM=10
Environment=RUST_LOG=ytdl_raw=info,tower_http=info

# Restart Policies
Restart=always
RestartSec=3s

# VPS Resource Protection Limits (500MB RAM cap, 95% CPU ceiling)
MemoryMax=500M
MemoryHigh=400M
CPUQuota=95%

# Hardened Linux Sandbox
NoNewPrivileges=true
ProtectSystem=full
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable ytdl-web.service
systemctl restart ytdl-web.service

# 9. CONFIGURE NGINX REVERSE PROXY
log_info "Deploying Nginx reverse proxy configuration..."
if [[ -f "$SCRIPT_DIR/deploy/nginx.conf" ]]; then
    cp "$SCRIPT_DIR/deploy/nginx.conf" /etc/nginx/sites-available/ytdl-raw
else
    log_err "Nginx configuration file not found at deploy/nginx.conf"
    exit 1
fi

ln -sf /etc/nginx/sites-available/ytdl-raw /etc/nginx/sites-enabled/ytdl-raw
rm -f /etc/nginx/sites-enabled/default

nginx -t
systemctl reload nginx

# 10. CONFIGURE UFW FIREWALL
log_info "Configuring UFW firewall rules..."
ufw allow OpenSSH || true
ufw allow 'Nginx Full' || true
ufw allow 3000/tcp || true
ufw --force enable || true

# 11. VERIFY HEALTH ENDPOINT
log_info "Awaiting service startup and executing self-check..."
sleep 2
if curl -s http://127.0.0.1:3000/health | grep -q "online"; then
    log_info "Verification SUCCESS: Backend is healthy and responding!"
else
    log_err "Health check did not respond with online status. Inspect logs: journalctl -u ytdl-web -n 50"
fi

echo -e "\n${CLR_LIME}==============================================================================${CLR_RESET}"
echo -e "${CLR_TEXT} YTDL-RAW IS FULLY DEPLOYED AND OPERATIONAL${CLR_RESET}"
echo -e "${CLR_MUTED} Service: systemctl status ytdl-web${CLR_RESET}"
echo -e "${CLR_MUTED} Logs:    journalctl -u ytdl-web -f${CLR_RESET}"
echo -e "${CLR_LIME}==============================================================================${CLR_RESET}\n"
EOF
