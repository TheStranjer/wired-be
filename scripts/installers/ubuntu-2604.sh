#!/usr/bin/env bash
#
# scripts/installers/ubuntu-2604.sh: install or upgrade wired-be on Ubuntu
# 26.04 LTS.
#
# Written for exactly that release (its package names, PostgreSQL 18, nginx
# 1.28, certbot 4) and it refuses to run on any other. Another release gets its
# own file next to this one rather than branches in this one.
#
# Run as root. It is idempotent: the first run installs everything, and every
# later run upgrades to the configured git ref and re-applies the
# configuration. The settings of the first run are saved to
# /etc/wired-be/install.conf, so later runs need no arguments.
#
#   WIRED_BASE_URL=https://fedi.example.com \
#   WIRED_LOCAL_DOMAIN=example.com \
#   WIRED_CERTBOT_EMAIL=ops@example.com \
#   CLOUDFLARE_API_TOKEN=... \
#     bash scripts/installers/ubuntu-2604.sh
#
#   wired-be-setup                        # later: upgrade and re-apply
#   wired-be-setup --refresh-cloudflare   # refresh the Cloudflare IP ranges
#
# What it sets up:
#   * swap (when the machine has under 1 GiB), apt packages, a rustup toolchain
#     for an unprivileged build user, and a release build of WIRED_REF;
#   * PostgreSQL: a `wired` role and database reached over the Unix socket with
#     peer authentication, so there is no database password;
#   * /etc/wired-be/wired-be.env (managed, rewritten on every run) and
#     /etc/wired-be/local.env (yours, never touched; it overrides the managed
#     file), and a hardened systemd unit;
#   * nginx as a TLS reverse proxy. wired-be is API-only: nothing but the
#     backend is served, except an optional redirect of `/` (WIRED_ROOT_REDIRECT);
#   * behind Cloudflare (the default): nginx takes the client address from
#     CF-Connecting-IP, ufw admits 80/443 from Cloudflare's ranges only, and the
#     certificate comes from Let's Encrypt by DNS-01, which works while the
#     record is proxied and SSL mode is Full (strict). A weekly timer refreshes
#     the ranges.
#
# Settings (environment variables; saved for later runs):
#   WIRED_BASE_URL          required  public origin, e.g. https://fedi.example.com
#   WIRED_LOCAL_DOMAIN      optional  handle domain (LOCAL_DOMAIN), see README
#   WIRED_FRONTEND_BASE_URL optional  FRONTEND_BASE_URL
#   WIRED_ROOT_REDIRECT     optional  URL that `GET /` redirects to
#   WIRED_INSTANCE_NAME     optional  INSTANCE_NAME
#   WIRED_CONTACT_EMAIL     optional  CONTACT_EMAIL
#   WIRED_CERTBOT_EMAIL     optional  Let's Encrypt account email
#   WIRED_CLOUDFLARE        yes|no    default yes
#   WIRED_FFMPEG            yes|no    default no (video preview frames)
#   WIRED_REPO              default https://github.com/TheStranjer/wired-be.git
#   WIRED_REF               default master (branch, tag or commit); pin a
#                           commit or tag for a repeatable install
#   WIRED_SWAP_SIZE         default 4G
# One-shot inputs (never saved):
#   CLOUDFLARE_API_TOKEN    Zone:DNS:Edit token for DNS-01; stored for renewals
#                           in /root/.secrets/certbot/cloudflare.ini
#   WIRED_ADMIN_NICKNAME, WIRED_ADMIN_EMAIL
#                           create the first admin once; the generated password
#                           is printed one time
#   WIRED_FORCE_BUILD=yes   rebuild even when the commit has not changed
#
set -Eeuo pipefail

CONF_DIR=/etc/wired-be
INSTALL_CONF=$CONF_DIR/install.conf
MANAGED_ENV=$CONF_DIR/wired-be.env
LOCAL_ENV=$CONF_DIR/local.env
BUILD_HOME=/opt/wired-be
SRC=$BUILD_HOME/src
DATA=/var/lib/wired-be
BIN=/usr/local/bin/wired-be
SELF=/usr/local/sbin/wired-be-setup
CF_CREDS=/root/.secrets/certbot/cloudflare.ini
ACME_ROOT=/var/www/wired-be-acme
NGINX_SITE=/etc/nginx/sites-available/wired-be
NGINX_REALIP=/etc/nginx/conf.d/wired-be-cloudflare.conf
NGINX_DEFAULT=/etc/nginx/conf.d/wired-be-default.conf
UNIT=/etc/systemd/system/wired-be.service
LISTEN=127.0.0.1:4000
# This script's own path, resolved before main changes directory; empty when
# it arrives on standard input.
SCRIPT_PATH=$(realpath -e "${BASH_SOURCE[0]:-/nonexistent}" 2>/dev/null || true)
SETTINGS=(WIRED_BASE_URL WIRED_LOCAL_DOMAIN WIRED_FRONTEND_BASE_URL WIRED_ROOT_REDIRECT
  WIRED_INSTANCE_NAME WIRED_CONTACT_EMAIL WIRED_CERTBOT_EMAIL WIRED_CLOUDFLARE
  WIRED_FFMPEG WIRED_REPO WIRED_REF WIRED_SWAP_SIZE)

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '\033[1;33m    WARN: %s\033[0m\n' "$*"; }
die() {
  printf '\033[1;31m!!! %s\033[0m\n' "$*" >&2
  exit 1
}
trap 'die "Failed at line ${LINENO}: ${BASH_COMMAND}"' ERR

# Loads install.conf, lets the environment override it, applies defaults and
# writes the result back, so the next run repeats this one.
load_settings() {
  local key
  declare -A given=()
  for key in "${SETTINGS[@]}"; do
    if [[ -v $key ]]; then given[$key]=${!key}; fi
  done
  if [[ -f $INSTALL_CONF ]]; then
    # shellcheck source=/dev/null
    source "$INSTALL_CONF"
  fi
  for key in "${!given[@]}"; do printf -v "$key" '%s' "${given[$key]}"; done
  : "${WIRED_CLOUDFLARE:=yes}" "${WIRED_FFMPEG:=no}" "${WIRED_SWAP_SIZE:=4G}"
  : "${WIRED_REPO:=https://github.com/TheStranjer/wired-be.git}" "${WIRED_REF:=master}"
  [[ -n ${WIRED_BASE_URL:-} ]] || die "Set WIRED_BASE_URL (e.g. https://fedi.example.com)."
  [[ $WIRED_BASE_URL =~ ^https://([a-z0-9.-]+)/?$ ]] ||
    die "WIRED_BASE_URL must be https://<lowercase host> with no port or path."
  HOST=${BASH_REMATCH[1]}
  WIRED_BASE_URL=https://$HOST
  install -d -m 0755 "$CONF_DIR"
  {
    echo "# Saved by wired-be-setup; edit and re-run wired-be-setup to apply."
    for key in "${SETTINGS[@]}"; do printf '%s=%q\n' "$key" "${!key:-}"; done
  } >"$INSTALL_CONF.new"
  chmod 0600 "$INSTALL_CONF.new"
  mv -f "$INSTALL_CONF.new" "$INSTALL_CONF"
}

ensure_swap() {
  local swap_kb
  swap_kb=$(awk '/^SwapTotal:/ {print $2}' /proc/meminfo)
  if ((swap_kb >= 1048576)); then return; fi
  log "Adding ${WIRED_SWAP_SIZE} of swap (the release build needs it)"
  if [[ ! -f /swapfile ]]; then
    fallocate -l "$WIRED_SWAP_SIZE" /swapfile
    chmod 0600 /swapfile
    mkswap /swapfile >/dev/null
  fi
  swapon /swapfile 2>/dev/null || true
  grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >>/etc/fstab
}

install_packages() {
  log "Installing packages"
  local packages=(ca-certificates curl git build-essential pkg-config cmake
    postgresql nginx ufw certbot)
  [[ $WIRED_CLOUDFLARE == yes ]] && packages+=(python3-certbot-dns-cloudflare)
  [[ $WIRED_FFMPEG == yes ]] && packages+=(ffmpeg)
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends "${packages[@]}" >/dev/null
}

ensure_users() {
  id wired >/dev/null 2>&1 ||
    useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin wired
  id wiredbuild >/dev/null 2>&1 ||
    useradd --system --home-dir "$BUILD_HOME" --create-home --shell /usr/sbin/nologin wiredbuild
  install -d -o wired -g wired -m 0750 "$DATA" "$DATA/uploads" "$DATA/emoji" "$DATA/instance"
}

# Runs a command as the build user with its own toolchain on PATH.
as_build() {
  runuser -u wiredbuild -- env HOME="$BUILD_HOME" \
    PATH="$BUILD_HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" CARGO_BUILD_JOBS="$(nproc)" "$@"
}

# Installs this script as wired-be-setup: the checkout's copy when the ref has
# one, else the file being run. A rename, never an overwrite, because bash
# reads a running script as it goes.
install_self() {
  local from=$SRC/scripts/installers/ubuntu-2604.sh
  [[ -f $from ]] || from=$SCRIPT_PATH
  [[ -n $from && -f $from && $(realpath "$from") != "$SELF" ]] || return 0
  install -m 0755 "$from" "$SELF.new"
  mv -f "$SELF.new" "$SELF"
}

build() {
  log "Fetching ${WIRED_REPO} @ ${WIRED_REF}"
  if [[ ! -d $SRC/.git ]]; then as_build git clone --quiet "$WIRED_REPO" "$SRC"; fi
  as_build git -C "$SRC" remote set-url origin "$WIRED_REPO"
  as_build git -C "$SRC" fetch --quiet --prune --tags origin
  local commit
  commit=$(as_build git -C "$SRC" rev-parse --verify --quiet "origin/$WIRED_REF^{commit}" ||
    as_build git -C "$SRC" rev-parse --verify "$WIRED_REF^{commit}")
  as_build git -C "$SRC" checkout --quiet --detach "$commit"
  install_self
  info "commit ${commit}"
  if [[ -x $BIN && $(cat "$BUILD_HOME/built-commit" 2>/dev/null) == "$commit" &&
    ${WIRED_FORCE_BUILD:-no} != yes ]]; then
    info "Already built; skipping the build."
    return
  fi
  log "Installing the Rust toolchain"
  if [[ -x $BUILD_HOME/.cargo/bin/rustup ]]; then
    as_build rustup update --no-self-update stable >/dev/null
  else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
      as_build sh -s -- -y --quiet --profile minimal --default-toolchain stable --no-modify-path
  fi
  log "Building wired-be (a first build on one small CPU takes a long while)"
  as_build env -C "$SRC" nice -n 10 cargo build --release --locked
  install -o root -g root -m 0755 "$SRC/target/release/wired-be" "$BIN.new"
  mv -f "$BIN.new" "$BIN"
  echo "$commit" >"$BUILD_HOME/built-commit"
  BUILT=yes
}

setup_database() {
  log "Preparing PostgreSQL"
  systemctl enable --now postgresql >/dev/null 2>&1
  local psql=(runuser -u postgres -- psql -X -q -t -A -v ON_ERROR_STOP=1)
  [[ $("${psql[@]}" -c "SELECT 1 FROM pg_roles WHERE rolname = 'wired'") == 1 ]] ||
    runuser -u postgres -- createuser wired
  [[ $("${psql[@]}" -c "SELECT 1 FROM pg_database WHERE datname = 'wired'") == 1 ]] ||
    runuser -u postgres -- createdb --owner wired --encoding UTF8 wired
  "${psql[@]}" -d wired -c 'CREATE EXTENSION IF NOT EXISTS citext;
    CREATE EXTENSION IF NOT EXISTS pgcrypto; CREATE EXTENSION IF NOT EXISTS pg_trgm;'
}

# Prints KEY="value" in systemd EnvironmentFile syntax, skipping empty values.
env_line() {
  [[ -n $2 ]] || return 0
  local value=${2//\\/\\\\}
  printf '%s="%s"\n' "$1" "${value//\"/\\\"}"
}

write_config() {
  log "Writing ${MANAGED_ENV}"
  local ffmpeg=off
  [[ $WIRED_FFMPEG == yes ]] && ffmpeg=ffmpeg
  {
    echo "# Managed by wired-be-setup and rewritten on every run."
    echo "# Put your own settings in ${LOCAL_ENV}; they override these."
    env_line DATABASE_URL "postgres://wired@localhost/wired?host=/var/run/postgresql"
    env_line BASE_URL "$WIRED_BASE_URL"
    env_line LOCAL_DOMAIN "${WIRED_LOCAL_DOMAIN:-}"
    env_line FRONTEND_BASE_URL "${WIRED_FRONTEND_BASE_URL:-}"
    env_line INSTANCE_NAME "${WIRED_INSTANCE_NAME:-}"
    env_line CONTACT_EMAIL "${WIRED_CONTACT_EMAIL:-}"
    env_line BIND_ADDR "$LISTEN"
    env_line UPLOADS_DIR "$DATA/uploads"
    env_line EMOJI_DIR "$DATA/emoji"
    env_line INSTANCE_DOCS_DIR "$DATA/instance"
    env_line FFMPEG_PATH "$ffmpeg"
    env_line TRUST_PROXY_HEADERS true
    env_line TRUSTED_PROXY_HOPS 1
    env_line JOB_CONCURRENCY 2
    env_line MAX_IMAGE_PIXELS 16000000
    env_line RUST_LOG info
  } >"$MANAGED_ENV.new"
  chown root:wired "$MANAGED_ENV.new"
  chmod 0640 "$MANAGED_ENV.new"
  if cmp -s "$MANAGED_ENV.new" "$MANAGED_ENV"; then rm -f "$MANAGED_ENV.new"; else
    mv -f "$MANAGED_ENV.new" "$MANAGED_ENV"
    CHANGED=yes
  fi
  if [[ ! -f $LOCAL_ENV ]]; then
    printf '%s\n' "# Your wired-be settings (see .env.example); they override ${MANAGED_ENV}." \
      "# Restart after editing: systemctl restart wired-be" >"$LOCAL_ENV"
    chown root:wired "$LOCAL_ENV"
    chmod 0640 "$LOCAL_ENV"
  fi
}

write_unit() {
  cat >"$UNIT.new" <<EOF
# Managed by wired-be-setup.
[Unit]
Description=wired-be fediverse server
After=network-online.target postgresql.service
Wants=network-online.target
Requires=postgresql.service

[Service]
Type=simple
User=wired
Group=wired
WorkingDirectory=$DATA
EnvironmentFile=$MANAGED_ENV
EnvironmentFile=-$LOCAL_ENV
ExecStart=$BIN serve
Restart=always
RestartSec=5
TimeoutStopSec=60
LimitNOFILE=65536
MemoryHigh=60%
MemoryMax=75%
UMask=0027
ReadWritePaths=$DATA
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectProc=invisible
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
EOF
  if cmp -s "$UNIT.new" "$UNIT"; then rm -f "$UNIT.new"; else
    mv -f "$UNIT.new" "$UNIT"
    systemctl daemon-reload
    CHANGED=yes
  fi
}

# Prints Cloudflare's published ranges, one CIDR per line; fails rather than
# returning an empty or malformed list, which would lock every visitor out.
cloudflare_ranges() {
  local ranges
  ranges=$(curl -fsS --max-time 20 https://www.cloudflare.com/ips-v4 &&
    echo && curl -fsS --max-time 20 https://www.cloudflare.com/ips-v6)
  ranges=$(printf '%s\n' "$ranges" | tr -d '\r' | grep -E '^[0-9a-f:.]+/[0-9]+$')
  (($(wc -l <<<"$ranges") >= 10)) || die "Could not read Cloudflare's IP ranges."
  printf '%s\n' "$ranges"
}

setup_firewall() {
  log "Configuring the firewall"
  ufw allow 22/tcp comment ssh >/dev/null
  if [[ $WIRED_CLOUDFLARE != yes ]]; then
    ufw allow 80,443/tcp comment web >/dev/null
  else
    local ranges cidr number source
    ranges=$(cloudflare_ranges)
    while read -r cidr; do
      ufw allow proto tcp from "$cidr" to any port 80,443 comment cloudflare >/dev/null
    done <<<"$ranges"
    # Drop rules for ranges Cloudflare no longer publishes, highest number first.
    ufw status numbered | sed -n 's/^\[ *\([0-9]*\)\].* IN  *\([^ ]*\).*# cloudflare$/\1 \2/p' |
      sort -rn | while read -r number source; do
      grep -qxF "$source" <<<"$ranges" || ufw --force delete "$number" >/dev/null
    done
    {
      echo "# Managed by wired-be-setup: trust CF-Connecting-IP from Cloudflare only."
      while read -r cidr; do echo "set_real_ip_from $cidr;"; done <<<"$ranges"
      echo "real_ip_header CF-Connecting-IP;"
    } >"$NGINX_REALIP"
  fi
  ufw --force enable >/dev/null
  info "$(ufw status | head -1)"
}

write_nginx() {
  local cert=/etc/letsencrypt/live/$HOST
  install -d -m 0755 "$ACME_ROOT"
  [[ $WIRED_CLOUDFLARE == yes ]] || rm -f "$NGINX_REALIP"
  rm -f /etc/nginx/sites-enabled/default
  cat >"$NGINX_DEFAULT" <<'EOF'
# Managed by wired-be-setup: refuse every request for a name we do not serve.
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name _;
    server_tokens off;
    return 444;
}
server {
    listen 443 ssl default_server;
    listen [::]:443 ssl default_server;
    server_name _;
    server_tokens off;
    ssl_reject_handshake on;
}
EOF
  {
    cat <<EOF
# Managed by wired-be-setup and rewritten on every run.
upstream wired_be {
    server $LISTEN;
    keepalive 16;
}
map \$http_upgrade \$wired_be_connection {
    default upgrade;
    ''      '';
}
server {
    listen 80;
    listen [::]:80;
    server_name $HOST;
    server_tokens off;
    location /.well-known/acme-challenge/ { root $ACME_ROOT; }
EOF
    if [[ ! -f $cert/fullchain.pem ]]; then
      echo "    location / { return 503; }"
      echo "}"
    else
      cat <<EOF
    location / { return 301 https://\$host\$request_uri; }
}
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name $HOST;
    server_tokens off;
    ssl_certificate $cert/fullchain.pem;
    ssl_certificate_key $cert/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;

    # UPLOAD_LIMIT_BYTES (16 MB by default) plus multipart overhead.
    client_max_body_size 20m;

    proxy_http_version 1.1;
    proxy_set_header Host \$host;
    # Overwrite rather than append: one trusted hop (TRUSTED_PROXY_HOPS=1).
    proxy_set_header X-Forwarded-For \$remote_addr;
    proxy_set_header X-Forwarded-Proto \$scheme;
    proxy_set_header Upgrade \$http_upgrade;
    proxy_set_header Connection \$wired_be_connection;
EOF
      if [[ -n ${WIRED_ROOT_REDIRECT:-} ]]; then
        echo "    location = / { return 301 ${WIRED_ROOT_REDIRECT}; }"
      fi
      cat <<'EOF'
    location /api/v1/streaming {
        proxy_pass http://wired_be;
        proxy_buffering off;
        proxy_read_timeout 1h;
    }
    # No URI on proxy_pass: the request line reaches wired-be unchanged, which
    # HTTP Signatures over (request-target) depend on.
    location / { proxy_pass http://wired_be; }
}
EOF
    fi
  } >"$NGINX_SITE"
  ln -sfn "$NGINX_SITE" /etc/nginx/sites-enabled/wired-be
  nginx -t -q
  systemctl enable --now nginx >/dev/null 2>&1
  systemctl reload nginx
}

obtain_certificate() {
  log "TLS certificate for ${HOST}"
  local email=(--register-unsafely-without-email) auth
  [[ -n ${WIRED_CERTBOT_EMAIL:-} ]] && email=(-m "$WIRED_CERTBOT_EMAIL")
  if [[ $WIRED_CLOUDFLARE == yes ]]; then
    if [[ -n ${CLOUDFLARE_API_TOKEN:-} ]]; then
      install -d -m 0700 "$(dirname "$CF_CREDS")"
      (umask 077 && printf 'dns_cloudflare_api_token = %s\n' "$CLOUDFLARE_API_TOKEN" >"$CF_CREDS")
    fi
    if [[ ! -f $CF_CREDS ]]; then
      warn "No CLOUDFLARE_API_TOKEN given and no ${CF_CREDS}: skipping TLS."
      warn "Re-run with a Zone:DNS:Edit token to finish; nginx answers 503 until then."
      return
    fi
    auth=(--authenticator dns-cloudflare --dns-cloudflare-credentials "$CF_CREDS"
      --dns-cloudflare-propagation-seconds 30)
  else
    auth=(--authenticator webroot --webroot-path "$ACME_ROOT")
  fi
  certbot certonly --non-interactive --agree-tos --keep-until-expiring --quiet \
    --cert-name "$HOST" -d "$HOST" "${email[@]}" "${auth[@]}" \
    --deploy-hook 'systemctl reload nginx'
  systemctl enable --now certbot.timer >/dev/null 2>&1 || true
  info "$(certbot certificates --cert-name "$HOST" 2>/dev/null | grep -m1 'Expiry Date' | xargs)"
}

install_refresh_timer() {
  [[ $WIRED_CLOUDFLARE == yes ]] || return 0
  cat >/etc/systemd/system/wired-be-cloudflare-refresh.service <<EOF
[Unit]
Description=Refresh the Cloudflare IP ranges nginx and ufw trust for wired-be
[Service]
Type=oneshot
ExecStart=$SELF --refresh-cloudflare
EOF
  cat >/etc/systemd/system/wired-be-cloudflare-refresh.timer <<'EOF'
[Unit]
Description=Weekly refresh of Cloudflare IP ranges for wired-be
[Timer]
OnCalendar=weekly
RandomizedDelaySec=6h
Persistent=true
[Install]
WantedBy=timers.target
EOF
  systemctl daemon-reload
  systemctl enable --now wired-be-cloudflare-refresh.timer >/dev/null 2>&1
}

start_service() {
  log "Starting wired-be"
  systemctl enable wired-be >/dev/null 2>&1
  if [[ ${BUILT:-no} == yes || ${CHANGED:-no} == yes ]] || ! systemctl is-active -q wired-be; then
    systemctl restart wired-be
  fi
  local attempt
  for attempt in $(seq 1 60); do
    if curl -fsS -o /dev/null -H "Host: $HOST" "http://$LISTEN/api/v1/instance"; then
      info "Up after ${attempt}s."
      return
    fi
    systemctl is-active -q wired-be || break
    sleep 1
  done
  journalctl -u wired-be -n 40 --no-pager >&2
  die "wired-be did not come up; the log is above."
}

create_admin() {
  [[ -n ${WIRED_ADMIN_NICKNAME:-} ]] || return 0
  [[ ! -e $DATA/.admin-created ]] || {
    info "An admin was already created by this script; skipping."
    return 0
  }
  [[ -n ${WIRED_ADMIN_EMAIL:-} ]] || die "WIRED_ADMIN_NICKNAME needs WIRED_ADMIN_EMAIL."
  log "Creating admin ${WIRED_ADMIN_NICKNAME}"
  local password
  password=$(head -c 24 /dev/urandom | base64 | tr -d '/+=')
  printf '%s\n' "$password" | systemd-run --quiet --pipe --wait --collect \
    --uid=wired --gid=wired -p WorkingDirectory="$DATA" \
    -p EnvironmentFile="$MANAGED_ENV" -p EnvironmentFile=-"$LOCAL_ENV" \
    "$BIN" create-admin --nickname "$WIRED_ADMIN_NICKNAME" \
    --email "$WIRED_ADMIN_EMAIL" --password-stdin
  touch "$DATA/.admin-created"
  local file=/root/wired-be-admin-password
  (umask 077 && printf '%s %s\n' "$WIRED_ADMIN_NICKNAME" "$password" >"$file")
  info "The password is in ${file} (root only). Delete it once you have stored it."
}

smoke_test() {
  log "Smoke test (HTTP status codes; 200 is good)"
  local probe=(-sS -o /dev/null -w '%{http_code}' --max-time 10)
  info "instance: $(curl "${probe[@]}" -H "Host: $HOST" "http://$LISTEN/api/v1/instance")"
  info "nodeinfo: $(curl "${probe[@]}" -H "Host: $HOST" "http://$LISTEN/.well-known/nodeinfo")"
  if [[ -f /etc/letsencrypt/live/$HOST/fullchain.pem ]]; then
    info "via nginx and TLS: $(curl "${probe[@]}" --resolve "$HOST:443:127.0.0.1" \
      "https://$HOST/api/v1/instance")"
  fi
}

refresh_cloudflare() {
  load_settings
  [[ $WIRED_CLOUDFLARE == yes ]] || exit 0
  setup_firewall
  nginx -t -q
  systemctl reload nginx
}

main() {
  [[ $(id -u) == 0 ]] || die "Run as root."
  local release
  release=$(. /etc/os-release && echo "${ID:-}-${VERSION_ID:-}")
  [[ $release == ubuntu-26.04 ]] ||
    die "This installer is for Ubuntu 26.04 only; this machine is ${release}."
  cd /
  case ${1:-} in
    --refresh-cloudflare)
      refresh_cloudflare
      return
      ;;
    -h | --help)
      sed -n '2,/^set -Eeuo/p' "$0" | sed '$d; s/^# \{0,1\}//'
      return
      ;;
    "") ;;
    *) die "Unknown argument: $1 (see --help)" ;;
  esac
  load_settings
  ensure_swap
  install_packages
  ensure_users
  build
  setup_database
  write_config
  write_unit
  setup_firewall
  write_nginx
  obtain_certificate
  write_nginx
  install_refresh_timer
  start_service
  create_admin
  smoke_test
  log "Done: ${WIRED_BASE_URL} (settings in ${INSTALL_CONF}; upgrade with ${SELF})"
}

main "$@"
