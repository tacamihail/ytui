#!/usr/bin/env bash
# ytui installer
#
# Installs what is missing, builds from source and drops the binary in
# ~/.local/bin. Nothing is downloaded at runtime except audio itself.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DST="$HOME/.local/bin/ytui"
BIN_REAL="$HOME/.local/bin/ytui.bin"
CFG_DIR="$HOME/.config/ytui"

c_dim=$'\e[2m'; c_ok=$'\e[32m'; c_yel=$'\e[33m'; c_ac=$'\e[36m'; c_rst=$'\e[0m'
info() { printf '%s\n' "${c_dim}::${c_rst} $*"; }
ok()   { printf '%s\n' "${c_ok} ✔${c_rst} $*"; }
warn() { printf '%s\n' "${c_yel} !${c_rst} $*"; }
die()  { printf '%s\n' "${c_yel} !${c_rst} $*" >&2; exit 1; }

[[ ${EUID} -eq 0 ]] && die "run as your normal user (sudo is requested only when needed)"

# --- package manager ---------------------------------------------------------
if command -v apt-get >/dev/null 2>&1; then
  PM=apt
  BUILD_DEPS=(build-essential pkg-config libasound2-dev)
  RUNTIME_DEPS=(libasound2)
elif command -v pacman >/dev/null 2>&1; then
  PM=pacman
  BUILD_DEPS=(base-devel alsa-lib)
  RUNTIME_DEPS=(alsa-lib)
elif command -v dnf >/dev/null 2>&1; then
  PM=dnf
  BUILD_DEPS=(gcc make pkgconf-pkg-config alsa-lib-devel)
  RUNTIME_DEPS=(alsa-lib)
else
  PM=unknown
  BUILD_DEPS=()
  RUNTIME_DEPS=()
  warn "unknown distribution, install the ALSA headers and yt-dlp manually if the build fails"
fi

pm_install() {
  case "$PM" in
    apt)    sudo apt-get update -qq && sudo apt-get install -y "$@" ;;
    pacman) sudo pacman -S --needed --noconfirm "$@" ;;
    dnf)    sudo dnf install -y "$@" ;;
    *)      return 1 ;;
  esac
}

# --- yt-dlp ------------------------------------------------------------------
# Needs a recent build: YouTube breaks old extractors within weeks.
need_ytdlp() {
  command -v yt-dlp >/dev/null 2>&1 || return 0
  local v year
  v="$(yt-dlp --version 2>/dev/null | head -1)"
  year="${v%%.*}"
  [[ "$year" =~ ^[0-9]+$ ]] || return 0
  ((year >= 2025)) && return 1
  return 0
}

install_ytdlp() {
  info "installing a fresh yt-dlp into ~/.local/bin"
  mkdir -p "$HOME/.local/bin"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
      -o "$HOME/.local/bin/yt-dlp"
    chmod +x "$HOME/.local/bin/yt-dlp"
    ok "yt-dlp $(yt-dlp --version 2>/dev/null || echo installed)"
  else
    pm_install yt-dlp || die "install yt-dlp manually: https://github.com/yt-dlp/yt-dlp"
  fi
}

if need_ytdlp; then
  install_ytdlp
else
  ok "yt-dlp $(yt-dlp --version 2>/dev/null | head -1)"
fi

# --- deno --------------------------------------------------------------------
# yt-dlp solves YouTube's JS challenges with a JS runtime; deno is the only one
# enabled by default. Without it most videos fail with "Unable to extract
# uploader id".
if command -v deno >/dev/null 2>&1; then
  ok "deno $(deno --version 2>/dev/null | head -1)"
else
  info "installing Deno (yt-dlp needs it to resolve YouTube streams)"
  mkdir -p "$HOME/.deno/bin"
  tmp="$(mktemp -d)"
  if curl -fsSL https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip \
       -o "$tmp/deno.zip"; then
    if command -v unzip >/dev/null 2>&1; then
      unzip -oq "$tmp/deno.zip" -d "$HOME/.deno/bin"
    else
      python3 -m zipfile -e "$tmp/deno.zip" "$HOME/.deno/bin"
    fi
    chmod +x "$HOME/.deno/bin/deno"
    export PATH="$HOME/.deno/bin:$PATH"
    ok "deno $(deno --version 2>/dev/null | head -1)"
  else
    warn "could not download Deno. Install it from https://deno.com or YouTube playback will fail"
  fi
  rm -rf "$tmp"
fi

# --- runtime audio deps ------------------------------------------------------
# cpal needs the ALSA runtime; on WSL the PulseAudio plugin is what actually
# makes sound (there is no sound card, only $PULSE_SERVER).
on_wsl() { grep -qi microsoft /proc/version 2>/dev/null; }

# No sudo? Pull the plugin out of its .deb and keep it in the home directory.
# The launcher then points ALSA_PLUGIN_DIR (and a private ALSA config) at it.
extract_plugins() {
  local tmp
  tmp="$(mktemp -d)" || return 1
  if ! ( cd "$tmp" \
         && apt-get download libasound2-plugins >/dev/null 2>&1 \
         && dpkg -x libasound2-plugins_*.deb "$HOME/.local/alsa" ); then
    rm -rf "$tmp"
    return 1
  fi
  rm -rf "$tmp"
}

if [[ "$PM" == apt ]]; then
  rt=()
  for p in "${RUNTIME_DEPS[@]}"; do
    dpkg -s "$p" >/dev/null 2>&1 || rt+=("$p")
  done
  if on_wsl; then
    dpkg -s libasound2-plugins >/dev/null 2>&1 || rt+=(libasound2-plugins)
  fi
  if ((${#rt[@]})); then
    info "installing audio runtime: ${rt[*]}"
    if pm_install "${rt[@]}"; then
      ok "audio runtime installed"
    elif printf '%s\n' "${rt[@]}" | grep -qx libasound2-plugins; then
      warn "apt could not install libasound2-plugins, extracting it into ~/.local/alsa"
      if extract_plugins; then
        ok "plugin extracted -> ~/.local/alsa (the launcher points ALSA at it)"
      else
        warn "could not extract the plugin either"
        warn "run: sudo apt-get install -y libasound2-plugins"
      fi
    else
      warn "could not install ${rt[*]}"
      warn "on WSL run: sudo apt-get install -y libasound2-plugins"
    fi
  else
    ok "audio runtime present"
  fi
fi

# --- build dependencies ------------------------------------------------------
if [[ "$PM" != unknown ]]; then
  missing=()
  if [[ "$PM" == apt ]]; then
    for p in "${BUILD_DEPS[@]}"; do
      dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p")
    done
  else
    # Cheap heuristic: a working compiler + pkg-config + ALSA headers.
    command -v cc >/dev/null 2>&1 || missing+=("${BUILD_DEPS[@]}")
    command -v pkg-config >/dev/null 2>&1 || missing+=("${BUILD_DEPS[@]}")
    [[ -e /usr/include/alsa/asoundlib.h ]] || missing+=("${BUILD_DEPS[@]}")
  fi
  if ((${#missing[@]})); then
    info "installing build deps: ${missing[*]}"
    pm_install "${missing[@]}" || warn "some build deps failed to install"
  else
    ok "build deps present"
  fi
fi

# --- rust --------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  info "installing Rust via rustup"
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi
command -v cargo >/dev/null 2>&1 || die "cargo not found, install Rust: https://rustup.rs"
ok "cargo $(cargo --version | cut -d' ' -f2)"

# --- build -------------------------------------------------------------------
info "building ytui (the first build downloads crates, 1-3 min)"
(cd "$DIR" && cargo build --release)
install -Dm755 "$DIR/target/release/ytui" "$BIN_REAL"

# Launcher, not the binary: on WSL the ALSA pulse plugin may only exist as an
# extracted copy under ~/.local/alsa, so point ALSA_PLUGIN_DIR at it before
# exec. No-op on normal Linux, where the system plugin is found anyway.
cat > "$BIN_DST" <<'LAUNCHER'
#!/bin/sh
# ytui launcher: locate the WSL ALSA pulse plugin if it was extracted locally.
if [ -z "${ALSA_PLUGIN_DIR:-}" ]; then
    for d in \
        "$HOME/.local/alsa/usr/lib/x86_64-linux-gnu/alsa-lib" \
        "$HOME/.local/alsa/usr/lib/aarch64-linux-gnu/alsa-lib"
    do
        if [ -d "$d" ]; then
            ALSA_PLUGIN_DIR="$d"
            export ALSA_PLUGIN_DIR
            break
        fi
    done
fi

# An extracted plugin also has its ALSA definition outside the system config
# directories, so give ytui a private config that includes it. Only set when
# the plugin was found above; with the package installed the system config
# already defines the same PCM.
if [ -n "${ALSA_PLUGIN_DIR:-}" ] && [ -z "${ALSA_CONFIG_PATH:-}" ] \
   && [ -r "$HOME/.config/ytui/asound.conf" ]; then
    ALSA_CONFIG_PATH="$HOME/.config/ytui/asound.conf"
    export ALSA_CONFIG_PATH
fi

self=$0
case $self in
    */*) ;;
    *) self=$(command -v -- "$self") ;;
esac
dir=$(CDPATH= cd -- "$(dirname -- "$self")" && pwd)
exec "$dir/ytui.bin" "$@"
LAUNCHER
chmod 755 "$BIN_DST"
ok "installed -> $BIN_DST (+ $BIN_REAL)"

# --- config ------------------------------------------------------------------
mkdir -p "$CFG_DIR"
# Remember the checkout so `ytui update` can pull and rebuild it later.
printf '%s\n' "$DIR" > "$CFG_DIR/source"
if [[ ! -f "$CFG_DIR/config.json" ]]; then
  cp "$DIR/config.example.json" "$CFG_DIR/config.json"
  ok "seeded config -> $CFG_DIR/config.json"
else
  ok "config already present"
fi

# A locally extracted plugin is invisible to ALSA's normal configuration
# search, so the launcher needs a config file that includes its PCM
# definition and makes it the default. Scoped to ytui through ALSA_CONFIG_PATH;
# ~/.asoundrc is left alone.
if [[ -d "$HOME/.local/alsa" ]]; then
  cat > "$CFG_DIR/asound.conf" <<EOF
# Generated by install.sh for a libasound2-plugins copy under ~/.local/alsa.
</usr/share/alsa/alsa.conf>
<$HOME/.local/alsa/usr/share/alsa/alsa.conf.d/50-pulseaudio.conf>
pcm.!default {
    type pulse
}
ctl.!default {
    type pulse
}
EOF
  ok "wrote $CFG_DIR/asound.conf (used only by the launcher)"
fi

# --- PATH --------------------------------------------------------------------
add_path_line() {
  local dir="$1" rc
  case ":$PATH:" in
    *":$dir:"*) return 1 ;;
  esac
  for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    [[ -f "$rc" ]] || continue
    printf '\nexport PATH="%s:$PATH"\n' "$dir" >> "$rc"
    ok "added $dir to PATH in $(basename "$rc") (open a new shell)"
  done
  return 0
}

add_path_line "$HOME/.local/bin" || ok "~/.local/bin already in PATH"
add_path_line "$HOME/.deno/bin" || ok "~/.deno/bin already in PATH"

# --- WSL audio note ----------------------------------------------------------
if on_wsl; then
  if [[ "$PM" == apt ]] && ! dpkg -s libasound2-plugins >/dev/null 2>&1 \
     && [[ ! -d "$HOME/.local/alsa/usr/lib" ]]; then
    warn "WSL without libasound2-plugins. If audio fails, run:"
    warn "  sudo apt-get install -y libasound2-plugins"
  fi
fi

echo
printf '%s\n' "${c_ac}  ytui installed.${c_rst}"
printf '%s\n' "  ytui doctor                 check yt-dlp + audio device"
printf '%s\n' "  ytui auth <cookies.txt>     connect Liked Music / playlists / subscriptions"
printf '%s\n' "  ytui                        open the player ( / search · ? help · q quit )"
