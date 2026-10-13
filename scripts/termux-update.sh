#!/data/data/com.termux/files/usr/bin/sh
# prime-agent-update — Termux-safe updater + auto-patch for Prime Agent.
#
# The upstream `prime-agent update` / TUI `/update` exec an installer that
# (a) refuses a non-managed install and (b) rejects Android/Bionic, so it
# cannot work here. This script does the update itself and re-applies the
# Termux patches:
#   * downloads the linux-arm64 release from the R2 channel, verifies sha256
#   * swaps the payload (keeps one .old generation for --rollback)
#   * patchelf's the binary to Termux's glibc loader (direct exec + daemon re-exec)
#   * rewrites the known-good launcher
#   * refreshes the glibc Python kernel venv from the new payload
#
# Usage:
#   prime-agent-update                 # update to the stable channel
#   prime-agent-update --nightly       # update from the beta channel
#   prime-agent-update --rollback      # restore the previous generation
#   prime-agent-update --archive FILE  # install a local release tarball
#   prime-agent-update --repair        # only re-apply patch + launcher + kernel
#   prime-agent-update --force         # run even if already current
set -eu

unset LD_PRELOAD

GLIBC=/data/data/com.termux/files/usr/glibc
PATCHELF="$GLIBC/bin/patchelf"
LOADER="$GLIBC/lib/ld-linux-aarch64.so.1"
GLIBC_LIB="$GLIBC/lib"

PREFIX="${PRIME_AGENT_RUST_PREFIX:-$HOME/.local}"
SHARE="$PREFIX/share/prime-agent"
BIN_DIR="$PREFIX/bin"
BIN="$SHARE/prime-agent"
LAUNCHER="$BIN_DIR/prime-agent"
VENV="${PRIME_AGENT_KERNEL_VENV:-$HOME/.prime/agent/kernel-venv}"
BASE="${PRIME_AGENT_DOWNLOAD_BASE_URL:-https://pub-728493de92a943e2a9b2d17b4719f318.r2.dev}"

CHANNEL="stable"
MODE="update"
ARCHIVE=""
FORCE="no"

usage() {
  sed -n '3,20p' "$0" | sed 's/^# \{0,1\}//'
}

say() { printf '%s\n' "$*"; }
die() { printf 'prime-agent-update: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --stable) CHANNEL="stable"; shift ;;
    --nightly|--beta) CHANNEL="beta"; shift ;;
    --rollback) MODE="rollback"; shift ;;
    --archive) MODE="archive"; ARCHIVE="${2:-}"; [ -n "$ARCHIVE" ] || die "--archive needs a file"; shift 2 ;;
    --repair|--patch-only) MODE="repair"; shift ;;
    update) shift ;;
    --force) FORCE="yes"; shift ;;
    --verbose) shift ;;
    -h|--help) usage; exit 0 ;;
    *) say "ignoring unknown argument: $1" >&2; shift ;;
  esac
done

safe_rm() { for p in "$@"; do if [ -n "$p" ] && [ "$p" != "/" ] && [ -e "$p" ]; then rm -rf "$p"; fi; done; return 0; }

# --- patch one binary so Termux can exec it directly ------------------------
patch_binary() {
  b="$1"
  [ -f "$b" ] || die "binary not found: $b"
  glibc-runner "$PATCHELF" --set-interpreter "$LOADER" --set-rpath "$GLIBC_LIB" "$b"
  chmod 0755 "$b"
  if ! ver="$(env -u LD_PRELOAD "$b" --version 2>&1)"; then
    die "patched binary failed to run: $ver"
  fi
  say "  patched binary: $b ($ver)"
}

# --- the launcher (must not pin PRIME_AGENT_DAEMON_SOCKET) ------------------
write_launcher() {
  mkdir -p "$BIN_DIR"
  cat > "$LAUNCHER" <<'LAUNCHER_EOF'
#!/data/data/com.termux/files/usr/bin/sh
# prime-agent — Termux launcher (custom, not upstream install-rust.sh).
# Binary is patchelf'd to Termux's glibc loader, so it can be exec'd directly
# (including the daemon/worker re-execs). LD_PRELOAD must be cleared.
unset LD_PRELOAD
export PRIME_AGENT_CODING_AGENT_DIR="${PRIME_AGENT_CODING_AGENT_DIR:-$HOME/.prime/agent}"
# Do NOT pin PRIME_AGENT_DAEMON_SOCKET: status/shutdown discovery scans the
# platform default socket dir, and the pin would hide the daemon from it.
export PRIME_AGENT_KERNEL_VENV="${PRIME_AGENT_KERNEL_VENV:-$HOME/.prime/agent/kernel-venv}"
here="$(dirname "$0")"
# Route `update` to the Termux-safe updater (the built-in one refuses here).
case "${1:-}" in
  update) exec "$here/prime-agent-update" "$@" ;;
esac
exec "$here/../share/prime-agent/prime-agent" "$@"
LAUNCHER_EOF
  chmod 0755 "$LAUNCHER"
  say "  launcher written: $LAUNCHER"
}

# --- patch runtime sources with Termux fail-fast timeouts -------------------
patch_runtime() {
  python3 - <<'PY'
import os
from pathlib import Path
share = Path(os.path.expanduser("~/.local/share/prime-agent"))
mcp_py = share / "prime-agent-runtime/src/rlm/mcp.py"
repl_py = share / "prime-agent-runtime/src/rlm/repl.py"
if mcp_py.exists():
    code = mcp_py.read_text()
    if "PRIME_AGENT_MCP_DISPATCH_TIMEOUT" not in code:
        code = code.replace(
            '_DEFAULT_CALL_TIMEOUT = 60.0',
            '_DEFAULT_CALL_TIMEOUT = 60.0\n# Termux fail-fast timeout\n_DEFAULT_DISPATCH_TIMEOUT = float(os.environ.get("PRIME_AGENT_MCP_DISPATCH_TIMEOUT", "30"))'
        )
        code = code.replace(
            '    current = asyncio.get_running_loop()\n    owner = _registry.bind_owner()\n    if current is owner:\n        return await operation()',
            '    if timeout is None:\n        timeout = _DEFAULT_DISPATCH_TIMEOUT\n    current = asyncio.get_running_loop()\n    owner = _registry.bind_owner()\n    if current is owner:\n        async with asyncio.timeout(timeout):\n            return await operation()'
        )
        code = code.replace(
            'return await _dispatch(lambda: _registry.call(server, tool, arguments or {}))',
            'return await _dispatch(lambda: _registry.call(server, tool, arguments or {}), timeout=_DEFAULT_CALL_TIMEOUT + 10)'
        )
        mcp_py.write_text(code)
if repl_py.exists():
    code = repl_py.read_text()
    if "PRIME_AGENT_MCP_HOST_TIMEOUT" not in code:
        target = '''    try:
        _send({"event": "host_request", "id": rid, "data": data})
        return await future
    finally:'''
        replacement = '''    try:
        _send({"event": "host_request", "id": rid, "data": data})
        request_type = data.get("type")
        if isinstance(request_type, str) and request_type.startswith("mcp."):
            timeout = float(os.environ.get("PRIME_AGENT_MCP_HOST_TIMEOUT", "20"))
            try:
                return await asyncio.wait_for(future, timeout)
            except asyncio.TimeoutError:
                raise RuntimeError(f"host did not answer {request_type!r} within {timeout:g}s") from None
        return await future
    finally:'''
        if target in code:
            code = code.replace(target, replacement)
            repl_py.write_text(code)
PY

  # Restore Exa websearch patch if preserved in config
  if [ -f "$HOME/.prime/agent/patches/websearch/websearch.py" ]; then
    cp -f "$HOME/.prime/agent/patches/websearch/websearch.py" \
          "$SHARE/skills/websearch/src/websearch/websearch.py" 2>/dev/null || true
    cp -f "$HOME/.prime/agent/patches/websearch/SKILL.md" \
          "$SHARE/skills/websearch/SKILL.md" 2>/dev/null || true
  fi
}

# --- refresh the glibc Python kernel venv from the payload (best effort) ----
repair_kernel() {
  [ -x "$VENV/bin/python" ] || { say "  kernel venv absent; skipping"; return 0; }
  command -v uv >/dev/null 2>&1 || { say "  uv not found; skipping kernel refresh"; return 0; }
  [ -d "$SHARE/prime-agent-runtime" ] || return 0
  patch_runtime
  say "  refreshing kernel venv (runtime + skills)"
  set -- ./prime-agent-runtime
  for s in skills/agent-message skills/agent-observe skills/attach-image \
           skills/compact skills/edit skills/goal skills/refine \
           skills/rlm-heartbeat skills/websearch; do
    if [ -d "$s" ]; then set -- "$@" "$s"; fi
  done
  # One resolve so the local prime-agent-runtime satisfies the skills that
  # depend on it (installing them separately fails: not in the registry).
  if ( cd "$SHARE" && uv pip install --no-build-isolation --reinstall -q \
         --python "$VENV/bin/python" "$@" ); then
    say "  kernel venv refreshed"
  else
    say "  ! kernel refresh failed (run: prime-agent-update --repair)"
  fi

  # Stamp .bootstrap-version so Prime Agent recognizes the venv as ready
  # and never attempts to run `uv python install` (which hangs on Android).
  python3 - <<'PY'
import hashlib, os, json
from pathlib import Path
source_dir = Path(os.path.expanduser("~/.local/share/prime-agent/prime-agent-runtime"))
rlm_dir = source_dir / "src" / "rlm"
files = [source_dir / "pyproject.toml"]
for root, _, filenames in os.walk(rlm_dir):
    for f in filenames:
        if f.endswith(".py") or "machines" in root:
            files.append(Path(root) / f)
files.sort()
hasher = hashlib.sha256()
for f in files:
    rel = f.relative_to(source_dir)
    hasher.update(str(rel).encode())
    hasher.update(b"\x00")
    hasher.update(f.read_bytes())
    hasher.update(b"\x00")
runtime_id = f"sha256:{hasher.hexdigest()}"
skills_dir = Path(os.path.expanduser("~/.local/share/prime-agent/skills"))
skills = []
for s in sorted(skills_dir.iterdir()):
    if (s / "pyproject.toml").exists():
        h = hashlib.sha256((s / "pyproject.toml").read_bytes()).hexdigest()
        import_name = s.name.replace("-", "_")
        if (s / "src").exists():
            for p in (s / "src").iterdir():
                if p.is_dir() or p.name.endswith(".py"):
                    import_name = p.stem
        skills.append({
            "name": s.name,
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
Path(os.path.expanduser("~/.prime/agent/kernel-venv/.bootstrap-version")).write_text(json.dumps(ver, indent=2) + "\n")
PY
}

# --- fetch the channel version + expected sha256 for linux-arm64 ------------
channel_version() {
  v="$(curl -fsSL "$BASE/$CHANNEL")" || die "could not read channel '$CHANNEL' from $BASE"
  v="$(printf '%s' "$v" | tr -d '[:space:]')"
  v="${v#v}"
  [ -n "$v" ] || die "empty channel version"
  printf '%s' "$v"
}

manifest_file() { [ "$CHANNEL" = "beta" ] && printf 'beta.json' || printf 'latest.json'; }

asset_sha() { # $1=manifest url, $2=version
  curl -fsSL "$1" | python3 -c '
import json,sys
want=sys.argv[1]
d=json.load(sys.stdin)
for key in ("binaries","binaries_v2"):
    for row in d.get(key) or []:
        if row.get("platform")=="linux-arm64" and want in (row.get("file") or ""):
            print(row.get("sha256") or ""); raise SystemExit(0)
raise SystemExit("no linux-arm64 row for "+want)
' "prime-agent-$2-linux-arm64.tar.gz" || die "could not resolve linux-arm64 asset for v$2"
}

stop_running_daemons() {
  say "  stopping any active daemons before file swap..."
  if [ -x "$LAUNCHER" ]; then
    "$LAUNCHER" shutdown --force >/dev/null 2>&1 || true
  fi
  for pid in $(ps -ef 2>/dev/null | grep -i "share/prime-agent" | grep -v grep | awk '{print $2}'); do
    kill -9 "$pid" 2>/dev/null || true
  done
}

# --- install a release tarball into the payload (preserving glibc python) ----
install_payload() { # $1=tarball path, $2=version label
  tar_file="$1"; label="$2"
  staging="$SHARE.staging.$$"
  safe_rm "$staging"
  mkdir -p "$staging"
  say "  extracting $tar_file"
  tar -xzf "$tar_file" -C "$staging" || die "extraction failed"
  if [ ! -f "$staging/prime-agent" ] && [ -f "$staging/pa/prime-agent" ]; then
    mv "$staging/pa"/* "$staging/pa"/.[!.]* "$staging"/ 2>/dev/null || true
    rmdir "$staging/pa" 2>/dev/null || true
  fi
  [ -f "$staging/prime-agent" ] || die "release archive has no prime-agent binary"

  patch_binary "$staging/prime-agent"

  # stop background daemons before touching the live share directory
  stop_running_daemons

  # swap: keep one .old generation, carry the glibc Python tree across
  old="$SHARE.old.$label"
  safe_rm "$old"
  if [ -d "$SHARE" ]; then mv "$SHARE" "$old"; fi
  mv "$staging" "$SHARE"
  if [ -d "$old/python" ]; then mv "$old/python" "$SHARE/python"; fi

  # Forward any /update call triggered from payload's install.sh to our updater
  cat > "$SHARE/install.sh" << 'INST_EOF'
#!/data/data/com.termux/files/usr/bin/sh
exec "$HOME/.local/bin/prime-agent-update" "$@"
INST_EOF
  chmod 0755 "$SHARE/install.sh"

  say "  installed payload -> $SHARE (previous kept at $old)"
}

# ============================================================================
case "$MODE" in
  repair)
    say "repairing Termux install"
    patch_binary "$BIN"
    write_launcher
    repair_kernel
    say "done. verify: prime-agent --version"
    ;;

  rollback)
    old="$(ls -d "$SHARE".old.* 2>/dev/null | sort | tail -n 1 || true)"
    [ -n "$old" ] || die "no previous generation to roll back to"
    say "rolling back to $old"
    cur="$SHARE.rollbackcur.$$"
    mv "$SHARE" "$cur"
    mv "$old" "$SHARE"
    # carry the glibc Python tree across (it lives only in the installed gen)
    if [ -d "$cur/python" ] && [ ! -d "$SHARE/python" ]; then mv "$cur/python" "$SHARE/python"; fi
    mv "$cur" "$old"
    patch_binary "$BIN"
    write_launcher
    repair_kernel
    say "done. verify: prime-agent --version"
    ;;

  archive)
    [ -f "$ARCHIVE" ] || die "--archive file not found: $ARCHIVE"
    say "installing local archive $ARCHIVE"
    install_payload "$ARCHIVE" "archive"
    write_launcher
    repair_kernel
    say "done. verify: prime-agent --version"
    ;;

  update)
    say "checking ${CHANNEL} channel at $BASE"
    version="$(channel_version)"
    say "channel version: v$version"
    current="$(env -u LD_PRELOAD "$BIN" --version 2>/dev/null || echo none)"
    say "installed version: $current"
    if [ "$FORCE" != "yes" ] && [ "$current" = "$version" ]; then
      say "already up to date (use --force to reinstall); re-applying patch anyway"
      patch_binary "$BIN"; write_launcher
      say "done."
      exit 0
    fi
    mfile="$(manifest_file)"
    sha="$(asset_sha "$BASE/$mfile" "$version")"
    file="prime-agent-$version-linux-arm64.tar.gz"
    url="$BASE/releases/v$version/$file"
    tmp="$HOME/tmp/pa-dl-$$"; mkdir -p "$tmp"
    trap 'safe_rm "$tmp"' EXIT
    say "  downloading $url"
    curl -fsSL -o "$tmp/$file" "$url" || die "download failed"
    got="$(sha256sum "$tmp/$file" | awk '{print $1}')"
    if [ -n "$sha" ] && [ "$got" != "$sha" ]; then
      die "sha256 mismatch (expected $sha, got $got)"
    fi
    say "  sha256 ok: $got"
    install_payload "$tmp/$file" "$version"
    write_launcher
    repair_kernel
    new="$(env -u LD_PRELOAD "$BIN" --version 2>/dev/null || echo FAILED)"
    say "done. installed version: $new"
    [ "$new" != "FAILED" ] || exit 1
    ;;
esac
