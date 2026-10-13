#!/usr/bin/env bash
# ==============================================================================
# Prime Agent (Native Antigravity & Termux Edition) — One-Line Installer/Updater
# Repository: https://github.com/Anniqa/prime-agent
# ==============================================================================
set -euo pipefail

REPO="Anniqa/prime-agent"
PREFIX="${PRIME_AGENT_RUST_PREFIX:-$HOME/.local}"
SHARE="$PREFIX/share/prime-agent"
BIN_DIR="$PREFIX/bin"
BIN="$SHARE/prime-agent"
LAUNCHER="$BIN_DIR/prime-agent"
NATIVE_BACKUP="$PREFIX/share/prime-agent-native/prime-agent"
AGENT_DIR="${PRIME_AGENT_CODING_AGENT_DIR:-$HOME/.prime/agent}"

VERSION="latest"
MODE="install"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[!] WARN:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[x] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Options:
  --version <tag>    Install a specific version (e.g. v0.10.0) [default: latest]
  --repair           Repair current Termux installation without re-downloading
  -h, --help         Show this help message

One-line installation:
  curl -fsSL https://raw.githubusercontent.com/Anniqa/prime-agent/main/install.sh | bash
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --repair) MODE="repair"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) warn "ignoring unknown argument: $1"; shift ;;
  esac
done

# ------------------------------------------------------------------------------
# 1. Platform & Environment Detection
# ------------------------------------------------------------------------------
IS_TERMUX="no"
if [ -d "/data/data/com.termux" ] || [ -n "${TERMUX_VERSION:-}" ]; then
  IS_TERMUX="yes"
fi

UNAME_M="$(uname -m)"
case "$UNAME_M" in
  aarch64|arm64) PLATFORM="linux-arm64" ;;
  x86_64|amd64)  PLATFORM="linux-x64" ;;
  *) die "Unsupported architecture: $UNAME_M" ;;
esac

# ------------------------------------------------------------------------------
# 2. Termux Dependencies
# ------------------------------------------------------------------------------
setup_termux_prereqs() {
  say "Checking Termux dependencies..."
  local pkgs_to_install=()
  for cmd in curl tar patchelf; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
      pkgs_to_install+=("$cmd")
    fi
  done

  # glibc check
  if [ ! -d "/data/data/com.termux/files/usr/glibc" ]; then
    pkgs_to_install+=("glibc-repo" "glibc")
  fi

  if [ "${#pkgs_to_install[@]}" -gt 0 ]; then
    say "Installing required packages: ${pkgs_to_install[*]}"
    pkg install -y "${pkgs_to_install[@]}" || die "Failed to install packages via pkg"
  fi

  # Clean unsupported dynamic section entries (DT_AARCH64_BTI_PLT) that trigger Bionic linker warnings
  if [ -f "/data/data/com.termux/files/usr/lib/libc++_shared.so" ]; then
    if ! command -v termux-elf-cleaner >/dev/null 2>&1; then
      pkg install -y termux-elf-cleaner 2>/dev/null || true
    fi
    if command -v termux-elf-cleaner >/dev/null 2>&1; then
      termux-elf-cleaner /data/data/com.termux/files/usr/lib/libc++_shared.so 2>/dev/null || true
    fi
  fi
}

# ------------------------------------------------------------------------------
# 3. Patch Binary for Termux
# ------------------------------------------------------------------------------
patch_termux_binary() {
  local target="$1"
  [ -f "$target" ] || die "Target binary not found: $target"
  local loader="/data/data/com.termux/files/usr/glibc/lib/ld-linux-aarch64.so.1"
  local glibc_lib="/data/data/com.termux/files/usr/glibc/lib"
  local patchelf_cmd="/data/data/com.termux/files/usr/glibc/bin/patchelf"

  if [ ! -x "$patchelf_cmd" ]; then
    patchelf_cmd="$(command -v patchelf || echo "patchelf")"
  fi

  say "Patching ELF interpreter and rpath for Termux: $target"
  if command -v glibc-runner >/dev/null 2>&1 && [ -f "/data/data/com.termux/files/usr/glibc/bin/patchelf" ]; then
    glibc-runner "/data/data/com.termux/files/usr/glibc/bin/patchelf" \
      --set-interpreter "$loader" \
      --set-rpath "$glibc_lib" \
      "$target"
  else
    patchelf --set-interpreter "$loader" --set-rpath "$glibc_lib" "$target"
  fi
  chmod 0755 "$target"

  # Test run
  local ver
  if ! ver="$(env -u LD_PRELOAD "$target" --version 2>&1)"; then
    die "Patched binary verification failed: $ver"
  fi
  say "Patched binary OK: $ver"
}

# ------------------------------------------------------------------------------
# 4. Write Launcher
# ------------------------------------------------------------------------------
write_launcher() {
  mkdir -p "$BIN_DIR"
  if [ "$IS_TERMUX" = "yes" ]; then
    cat > "$LAUNCHER" <<'LAUNCHER_EOF'
#!/data/data/com.termux/files/usr/bin/sh
# prime-agent — Termux launcher
unset LD_PRELOAD
export PRIME_AGENT_CODING_AGENT_DIR="${PRIME_AGENT_CODING_AGENT_DIR:-$HOME/.prime/agent}"
export PRIME_AGENT_KERNEL_VENV="${PRIME_AGENT_KERNEL_VENV:-$HOME/.prime/agent/kernel-venv}"
here="$(dirname "$0")"
case "${1:-}" in
  update) exec "$here/prime-agent-update" "$@" ;;
esac
exec "$here/../share/prime-agent/prime-agent" "$@"
LAUNCHER_EOF
  else
    cat > "$LAUNCHER" <<'LAUNCHER_EOF'
#!/usr/bin/env bash
# prime-agent — Linux launcher
here="$(dirname "$0")"
case "${1:-}" in
  update) exec "$here/prime-agent-update" "$@" ;;
esac
exec "$here/../share/prime-agent/prime-agent" "$@"
LAUNCHER_EOF
  fi
  chmod 0755 "$LAUNCHER"
  say "Launcher installed: $LAUNCHER"
}

# ------------------------------------------------------------------------------
# 5. Ensure Antigravity Models Configuration
# ------------------------------------------------------------------------------
ensure_antigravity_config() {
  mkdir -p "$AGENT_DIR"
  python3 - "$AGENT_DIR/models.json" <<'PY'
import json, sys, os
p = sys.argv[1]
try:
    d = json.load(open(p))
    if not isinstance(d, dict): d = {}
except Exception:
    d = {}
d.setdefault("providers", {})
d["providers"]["antigravity"] = {
    "name": "Google Antigravity",
    "api": "antigravity",
    "models": [
        {"id": "gemini-3.8-flash-high", "name": "Gemini 3.8 Flash (High)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.8-flash-medium", "name": "Gemini 3.8 Flash (Medium)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (Low)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.8-flash-tiered", "name": "Gemini 3.8 Flash (Tiered)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.7-flash-high", "name": "Gemini 3.7 Flash (High)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.6-flash-high", "name": "Gemini 3.6 Flash (High)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.1-pro-high", "name": "Gemini 3.1 Pro (High)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-3.1-pro-low", "name": "Gemini 3.1 Pro (Low)", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-2.5-pro", "name": "Gemini 2.5 Pro", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "gemini-2.5-flash", "name": "Gemini 2.5 Flash", "reasoning": True, "input": ["text", "image"], "contextWindow": 1048576, "maxTokens": 65536},
        {"id": "claude-sonnet-4-6", "name": "Claude Sonnet 4.6 (Thinking)", "reasoning": True, "input": ["text", "image"], "contextWindow": 250000, "maxTokens": 65536},
        {"id": "claude-opus-4-6-thinking", "name": "Claude Opus 4.6 (Thinking)", "reasoning": True, "input": ["text", "image"], "contextWindow": 250000, "maxTokens": 65536},
        {"id": "gpt-oss-120b-medium", "name": "GPT-OSS 120B (Medium)", "reasoning": True, "input": ["text"], "contextWindow": 131072, "maxTokens": 65536}
    ]
}
json.dump(d, open(p, "w"), indent=2)
PY
  say "Configured Antigravity models (1M context) in $AGENT_DIR/models.json"
}

# ------------------------------------------------------------------------------
# 6. Kernel Venv Setup / Stamp
# ------------------------------------------------------------------------------
stamp_kernel_venv() {
  local venv="$AGENT_DIR/kernel-venv"
  if [ -d "$venv" ]; then
    python3 - "$venv/.bootstrap-version" "$SHARE" <<'PY_STAMP'
import sys, json, os, hashlib
from pathlib import Path
out_path = Path(sys.argv[1])
share = Path(sys.argv[2])
source_dir = share / "prime-agent-runtime"
skills_dir = share / "skills"

rlm_dir = source_dir / "src" / "rlm"
files = [source_dir / "pyproject.toml"]
if rlm_dir.exists():
    for root, _, filenames in os.walk(rlm_dir):
        for f in filenames:
            if f.endswith(".py"):
                files.append(Path(root) / f)
    machines_dir = rlm_dir / "machines"
    if machines_dir.is_dir():
        for root, _, filenames in os.walk(machines_dir):
            for f in filenames:
                files.append(Path(root) / f)

files.sort()
hasher = hashlib.sha256()
for file in files:
    rel = file.relative_to(source_dir)
    hasher.update(str(rel).encode())
    hasher.update(b"\x00")
    hasher.update(file.read_bytes())
    hasher.update(b"\x00")
runtime_id = f"sha256:{hasher.hexdigest()}"

skills = []
if skills_dir.exists():
    for s in sorted(skills_dir.iterdir()):
        if (s / "pyproject.toml").exists():
            h = hashlib.sha256((s / "pyproject.toml").read_bytes()).hexdigest()
            import_name = s.name.replace("-", "_")
            if (s / "src").exists():
                for p in (s / "src").iterdir():
                    if p.is_dir() or p.name.endswith(".py"):
                        import_name = p.stem
            skills.append({
                "import_name": import_name,
                "package_path": str(s),
                "pyproject_path": str(s / "pyproject.toml"),
                "pyproject_hash": f"sha256:{h}"
            })

ver = {
    "schema": 9,
    "runtime": runtime_id,
    "snapshot": "dill",
    "extra_uv_args": [
        "requests", "httpx", "pyyaml", "tomli", "python-dotenv",
        "pandas", "numpy", "scipy", "beautifulsoup4", "lxml",
        "pydantic", "tyro"
    ],
    "python_skills": skills
}
out_path.write_text(json.dumps(ver, indent=2) + "\n")
print(f"Kernel venv stamped ({runtime_id})")
PY_STAMP
    say "Kernel venv stamped (.bootstrap-version)"
  fi
}

# ------------------------------------------------------------------------------
# 7. Write Updater Script
# ------------------------------------------------------------------------------
write_updater() {
  local updater="$BIN_DIR/prime-agent-update"
  cat > "$updater" <<'UPDATER_EOF'
#!/usr/bin/env bash
set -euo pipefail
here="$(dirname "$0")"
exec curl -fsSL https://raw.githubusercontent.com/Anniqa/prime-agent/main/install.sh | bash -s -- "$@"
UPDATER_EOF
  chmod 0755 "$updater"
  ln -sf "prime-agent-update" "$BIN_DIR/pa-update"
  say "Updater script registered: $updater (alias: pa-update)"
}

# ==============================================================================
# Execution Flow
# ==============================================================================
if [ "$MODE" = "repair" ]; then
  say "Running Repair Mode..."
  if [ "$IS_TERMUX" = "yes" ]; then
    patch_termux_binary "$BIN"
  fi
  write_launcher
  ensure_antigravity_config
  stamp_kernel_venv
  write_updater
  say "Repair finished! Verify with: prime-agent --version"
  exit 0
fi

# Install / Update Mode
say "Starting Prime Agent installation..."
if [ "$IS_TERMUX" = "yes" ]; then
  setup_termux_prereqs
fi

# Determine download URL
resolve_download_url() {
  local target_tag="$VERSION"
  if [ "$target_tag" = "latest" ]; then
    local api_tag
    api_tag="$(curl -fsSL -H "Accept: application/vnd.github.v3+json" "https://api.github.com/repos/${REPO}/releases" 2>/dev/null | python3 -c '
import sys, json
try:
    data = json.load(sys.stdin)
    if isinstance(data, list) and len(data) > 0:
        print(data[0].get("tag_name", ""))
except Exception:
    pass
' 2>/dev/null || true)"
    if [ -n "$api_tag" ]; then
      target_tag="$api_tag"
    fi
  fi

  if [ "$target_tag" != "latest" ] && [ -n "$target_tag" ]; then
    echo "https://github.com/${REPO}/releases/download/${target_tag}/${TARBALL_NAME}"
  else
    echo "https://github.com/${REPO}/releases/latest/download/${TARBALL_NAME}"
  fi
}

TARBALL_NAME="prime-agent-${PLATFORM}.tar.gz"
DOWNLOAD_URL="$(resolve_download_url)"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

say "Downloading release: $DOWNLOAD_URL"
if ! curl -fSL -o "$TMP_DIR/$TARBALL_NAME" "$DOWNLOAD_URL"; then
  # Fallback if release not found yet: check local native backup
  if [ -f "$NATIVE_BACKUP" ]; then
    warn "Release asset not found online; restoring from local native backup..."
    mkdir -p "$SHARE"
    cp -f "$NATIVE_BACKUP" "$BIN"
  else
    die "Could not download $DOWNLOAD_URL. Check if a release exists at https://github.com/${REPO}/releases"
  fi
else
  say "Extracting archive to $SHARE..."
  mkdir -p "$SHARE"
  tar -xzf "$TMP_DIR/$TARBALL_NAME" -C "$SHARE" --strip-components=1 2>/dev/null || \
  tar -xzf "$TMP_DIR/$TARBALL_NAME" -C "$SHARE"
fi

if [ "$IS_TERMUX" = "yes" ]; then
  patch_termux_binary "$BIN"
  mkdir -p "$(dirname "$NATIVE_BACKUP")"
  cp -f "$BIN" "$NATIVE_BACKUP"
fi

write_launcher
ensure_antigravity_config
stamp_kernel_venv
write_updater

# Ensure $HOME/.local/bin is on PATH
if ! echo "$PATH" | grep -q "$BIN_DIR"; then
  echo "export PATH=\"$BIN_DIR:\$PATH\"" >> "$HOME/.bashrc"
  echo "export PATH=\"$BIN_DIR:\$PATH\"" >> "$HOME/.profile" 2>/dev/null || true
  warn "Added $BIN_DIR to PATH. Run: export PATH=\"$BIN_DIR:\$PATH\" or restart terminal."
fi

say "=========================================================================="
say "🎉 Prime Agent (Native Antigravity Edition) BERHASIL DIPASANG!"
say "=========================================================================="
echo "Versi: $(env -u LD_PRELOAD "$BIN" --version 2>&1 || echo "0.10.0")"
echo ""
echo "Cara Penggunaan:"
echo "  1. Buka TUI:          prime-agent"
echo "  2. Login Antigravity: ketik /login lalu pilih 'Google Antigravity'"
echo "  3. Cek daftar model:  prime-agent model list"
echo "  4. Update kapan saja: prime-agent update (atau pa-update)"
echo "=========================================================================="
