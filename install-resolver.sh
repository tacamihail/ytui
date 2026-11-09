#!/usr/bin/env bash
# ytui-resolver installer
#
# Installs only the resolver server: a recent yt-dlp, deno, Rust, and
# ~/.local/bin/ytui-resolver. The player has its own install.sh; this script
# does not need ALSA, the TUI crates or a sound card.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HOME/.local/bin/ytui-resolver"
CFG_DIR="$HOME/.config/ytui"
OS="$(uname -s)"
ARCH="$(uname -m)"
PATH_AT_START="$PATH"

c_dim=$'\e[2m'; c_ok=$'\e[32m'; c_yel=$'\e[33m'; c_ac=$'\e[36m'; c_rst=$'\e[0m'
info() { printf '%s\n' "${c_dim}::${c_rst} $*"; }
ok()   { printf '%s\n' "${c_ok} ✔${c_rst} $*"; }
warn() { printf '%s\n' "${c_yel} !${c_rst} $*"; }
die()  { printf '%s\n' "${c_yel} !${c_rst} $*" >&2; exit 1; }

usage() {
  cat <<EOF
usage: ./install-resolver.sh [--token] [--from-git]

Builds and installs ytui-resolver, plus yt-dlp, deno and Rust when they are
missing. With --from-git it uses \`cargo install --git\` instead of a local
checkout, so only the resolver package is built.

  --token     also generate ~/.config/ytui/resolver.token (public/token mode;
              for a trusted LAN/tailnet the default tokenless setup is enough)
  --from-git  install from github, no local checkout required (uses
              YTUI_BRANCH, default remote-resolver)
EOF
}

GEN_TOKEN=0
FROM_GIT=0
for arg in "$@"; do
  case "$arg" in
    --token) GEN_TOKEN=1 ;;
    --from-git) FROM_GIT=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option: $arg (try --help)" ;;
  esac
done
BRANCH="${YTUI_BRANCH:-remote-resolver}"
REPO="${YTUI_REPO:-https://github.com/tacamihail/ytui.git}"

[[ ${EUID} -eq 0 ]] && die "run as your normal user (sudo is requested only when needed)"

# --- package manager ---------------------------------------------------------
if [[ "$OS" == Darwin ]]; then
  PM=brew
  BUILD_DEPS=()
  if ! xcode-select -p >/dev/null 2>&1; then
    die "install the Xcode command line tools first: xcode-select --install"
  fi
elif command -v apt-get >/dev/null 2>&1; then
  PM=apt
  BUILD_DEPS=(build-essential)
elif command -v pacman >/dev/null 2>&1; then
  PM=pacman
  BUILD_DEPS=(base-devel)
elif command -v dnf >/dev/null 2>&1; then
  PM=dnf
  BUILD_DEPS=(gcc make)
else
  PM=unknown
  BUILD_DEPS=()
  warn "unknown distribution, install a C linker and yt-dlp manually if the build fails"
fi

pm_install() {
  case "$PM" in
    apt)    sudo apt-get update -qq && sudo apt-get install -y "$@" ;;
    pacman) sudo pacman -S --needed --noconfirm "$@" ;;
    dnf)    sudo dnf install -y "$@" ;;
    brew)   brew install "$@" ;;
    *)      return 1 ;;
  esac
}

# --- curl --------------------------------------------------------------------
if ! command -v curl >/dev/null 2>&1; then
  info "installing curl"
  pm_install curl || die "curl is required to download yt-dlp and deno"
fi

# --- yt-dlp ------------------------------------------------------------------
need_ytdlp() {
  command -v yt-dlp >/dev/null 2>&1 || return 0
  local v year
  v="$(yt-dlp --version 2>/dev/null | head -1)"
  year="${v%%.*}"
  [[ "$year" =~ ^[0-9]+$ ]] || return 0
  ((year >= 2025)) && return 1
  return 0
}

if need_ytdlp; then
  info "installing a fresh yt-dlp into ~/.local/bin"
  mkdir -p "$HOME/.local/bin"
  curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
    -o "$HOME/.local/bin/yt-dlp"
  chmod +x "$HOME/.local/bin/yt-dlp"
  export PATH="$HOME/.local/bin:$PATH"
  ok "yt-dlp $(yt-dlp --version 2>/dev/null || echo installed)"
else
  ok "yt-dlp $(yt-dlp --version 2>/dev/null | head -1)"
fi

# --- deno --------------------------------------------------------------------
if command -v deno >/dev/null 2>&1; then
  ok "deno $(deno --version 2>/dev/null | head -1)"
else
  info "installing deno (yt-dlp needs it for YouTube's JS challenge)"
  mkdir -p "$HOME/.deno/bin"
  tmp="$(mktemp -d)"
  case "$OS/$ARCH" in
    Linux/x86_64|Linux/amd64)    deno_url=https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip ;;
    Linux/aarch64|Linux/arm64)   deno_url=https://github.com/denoland/deno/releases/latest/download/deno-aarch64-unknown-linux-gnu.zip ;;
    Darwin/x86_64)               deno_url=https://github.com/denoland/deno/releases/latest/download/deno-x86_64-apple-darwin.zip ;;
    Darwin/arm64|Darwin/aarch64) deno_url=https://github.com/denoland/deno/releases/latest/download/deno-aarch64-apple-darwin.zip ;;
    *)                           deno_url= ;;
  esac
  if [[ -z "$deno_url" ]]; then
    warn "no deno build for $OS/$ARCH; install it from https://deno.com"
  elif curl -fsSL "$deno_url" -o "$tmp/deno.zip"; then
    if command -v unzip >/dev/null 2>&1; then
      unzip -oq "$tmp/deno.zip" -d "$HOME/.deno/bin" || warn "could not unpack deno"
    elif command -v python3 >/dev/null 2>&1; then
      python3 -m zipfile -e "$tmp/deno.zip" "$HOME/.deno/bin" || warn "could not unpack deno"
    else
      warn "need unzip or python3 to unpack deno"
    fi
    if [[ -f "$HOME/.deno/bin/deno" ]]; then
      chmod +x "$HOME/.deno/bin/deno"
      export PATH="$HOME/.deno/bin:$PATH"
      ok "deno $(deno --version 2>/dev/null | head -1)"
    else
      warn "could not install deno; get it from https://deno.com"
    fi
  else
    warn "could not download deno; get it from https://deno.com"
  fi
  rm -rf "$tmp"
fi

# --- build dependencies ------------------------------------------------------
if ((${#BUILD_DEPS[@]})); then
  missing=()
  if [[ "$PM" == apt ]]; then
    for p in "${BUILD_DEPS[@]}"; do
      dpkg -s "$p" >/dev/null 2>&1 || missing+=("$p")
    done
  else
    command -v cc >/dev/null 2>&1 || missing+=("${BUILD_DEPS[@]}")
    command -v make >/dev/null 2>&1 || missing+=("${BUILD_DEPS[@]}")
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
# The resolver package does not depend on cpal, symphonia or ratatui, so this
# small dependency tree is all a server needs. With --from-git cargo fetches
# the repository but still builds only ytui-resolver and ytui-resolve.
if ((FROM_GIT)); then
  info "installing ytui-resolver from $REPO ($BRANCH)"
  cargo install --git "$REPO" --branch "$BRANCH" --root "$HOME/.local" ytui-resolver
else
  info "building ytui-resolver from $DIR"
  (cd "$DIR" && cargo build --release -p ytui-resolver)
  mkdir -p "$HOME/.local/bin"
  cp "$DIR/target/release/ytui-resolver" "$BIN"
  chmod 755 "$BIN"
fi
ok "installed -> $BIN"

# --- config ------------------------------------------------------------------
mkdir -p "$CFG_DIR"
if ((GEN_TOKEN)); then
  "$BIN" --gen-token
fi

# --- PATH --------------------------------------------------------------------
add_path_line() {
  local dir="$1" rc
  case ":$PATH_AT_START:" in
    *":$dir:"*) return 1 ;;
  esac
  if [[ "$OS" == Darwin ]]; then
    rc="$HOME/.zprofile"
    [[ -f "$rc" ]] || : > "$rc"
    printf '\nexport PATH="%s:$PATH"\n' "$dir" >> "$rc"
    ok "added $dir to PATH in $(basename "$rc") (open a new shell)"
    return 0
  fi
  for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    [[ -f "$rc" ]] || continue
    printf '\nexport PATH="%s:$PATH"\n' "$dir" >> "$rc"
    ok "added $dir to PATH in $(basename "$rc") (open a new shell)"
  done
  return 0
}

add_path_line "$HOME/.local/bin" || ok "~/.local/bin already in PATH"
add_path_line "$HOME/.deno/bin" || ok "~/.deno/bin already in PATH"

echo
printf '%s\n' "${c_ac}  ytui-resolver installed.${c_rst}"
if ((GEN_TOKEN)); then
  printf '%s\n' "  ytui-resolver --bind 0.0.0.0:8787   # public/token mode, token file is ready"
else
  printf '%s\n' "  ytui-resolver --bind 0.0.0.0:8787 --allow-private   # LAN/tailnet, no token"
  printf '%s\n' "  ./install-resolver.sh --token                        # add a token later"
fi
printf '%s\n' "  ytui doctor                         # on the player machine"
printf '%s\n' "  docs/resolver.md has systemd and public-address notes"
