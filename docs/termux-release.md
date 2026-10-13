# Prime Agent Termux — Native Google Antigravity Edition

Fork resmi Prime Agent dengan provider native **Google Antigravity** (Google CloudCode PA), OAuth 2.0 loopback flow, dukungan penuh untuk **Android Termux**, serta pipeline otomatis **GitHub Actions** untuk build release.

---

## 🚀 1. Instalasi / Update Cepat (1 Baris Curl)

Jalankan perintah berikut di Termux atau Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/Anniqa/prime-agent/main/install.sh | bash
```

### Opsi Tambahan:
- **Install versi spesifik:**
  ```bash
  curl -fsSL https://raw.githubusercontent.com/Anniqa/prime-agent/main/install.sh | bash -s -- --version v0.10.0
  ```
- **Repair instalasi (jika glibc/venv bermasalah tanpa download ulang):**
  ```bash
  prime-agent-update --repair
  # atau:
  pa-update --repair
  ```

---

## 🤖 2. Cara Kerja GitHub Action (Auto Patch & Build Release)

Workflow GitHub Actions berada di `.github/workflows/release-antigravity.yml`.

### Triggers:
1. **Push Git Tag**:
   Setiap kali Anda membuat tag rilis baru dan push ke GitHub:
   ```bash
   git tag v0.10.1
   git push origin v0.10.1
   ```
   GitHub Action akan otomatis meng-compile, mem-package, dan membuat GitHub Release.

2. **Manual Run di Web GitHub (Workflow Dispatch)**:
   - Buka tab **Actions** di repositori GitHub: `https://github.com/Anniqa/prime-agent/actions`
   - Pilih workflow **Release Native Antigravity** di sidebar kiri.
   - Klik tombol **Run workflow**.
   - (Opsional) Masukkan `Release Tag` (misal `v0.10.1`).
   - (Opsional) Centang **Sync with upstream PrimeIntellect-ai/prime-agent before building** jika ingin otomatis menarik update terbaru dari repo upstream.

### Proses Build di GitHub Runner:
1. Cross-compilation ke `aarch64-unknown-linux-gnu` (Linux ARM64 / Android) dan `x86_64-unknown-linux-gnu` (Linux x64).
2. Strip symbol binary untuk meminimalkan ukuran (~42 MB).
3. Packaging ke `prime-agent-linux-arm64.tar.gz` dan `prime-agent-linux-x64.tar.gz` beserta metadata, skill, runtime, dan checksum `SHA256SUMS.txt`.
4. Otomatis publish ke **GitHub Releases**.

---

## 🔑 3. Login & Menggunakan Google Antigravity

1. Jalankan Prime Agent:
   ```bash
   prime-agent
   ```
2. Di antarmuka TUI, ketik:
   ```text
   /login
   ```
3. Pilih **Google Antigravity · subscription**.
4. Browser HP akan otomatis terbuka (via `termux-open-url`).
   - Login dengan akun Google Anda dan setujui izin.
   - Browser akan me-redirect ke `http://127.0.0.1:51121/oauth-callback`.
   - Jika redirect terhalang browser, salin URL callback atau authorization code dan tempelkan (paste) langsung ke prompt terminal.
5. Selesai! Kredensial disimpan otomatis di `~/.prime/agent/auth.json` dan token akan di-refresh otomatis jika kedaluwarsa.

---

## 📋 4. Daftar Model Antigravity (Context Window 1.0M)

Semua model diatur dengan context window **1,048,576 tokens** (1M):

| Model ID | Nama Model | Reasoning |
|---|---|---|
| `gemini-3.8-flash-high` | Gemini 3.8 Flash (High) | Ya |
| `gemini-3.8-flash-medium` | Gemini 3.8 Flash (Medium) | Ya |
| `gemini-3.8-flash-low` | Gemini 3.8 Flash (Low) | Ya |
| `gemini-3.8-flash-tiered` | Gemini 3.8 Flash (Tiered) | Ya |
| `gemini-3.7-flash-high` | Gemini 3.7 Flash (High) | Ya |
| `gemini-3.6-flash-high` | Gemini 3.6 Flash (High) | Ya |
| `gemini-3.1-pro-high` | Gemini 3.1 Pro (High) | Ya |
| `gemini-2.5-pro` | Gemini 2.5 Pro | Ya |
| `gemini-2.5-flash` | Gemini 2.5 Flash | Ya |
| `claude-sonnet-4-6` | Claude Sonnet 4.6 (Thinking) | Ya |
| `claude-opus-4-6-thinking` | Claude Opus 4.6 (Thinking) | Ya |
| `gpt-oss-120b-medium` | GPT-OSS 120B (Medium) | Ya |

Jalankan perintah langsung dari terminal tanpa TUI:
```bash
prime-agent --provider antigravity --model gemini-3.8-flash-high -p "Halo, siapa kamu?" --no-session
```

---

## 🛠️ 5. Patch Teknis Khusus Termux

1. **Glibc Loader Patch (`patchelf`)**:
   Binary diatur menggunakan interpreter `/data/data/com.termux/files/usr/glibc/lib/ld-linux-aarch64.so.1` dan rpath `/data/data/com.termux/files/usr/glibc/lib`. Launcher menghapus `LD_PRELOAD` sehingga daemon dan multi-worker dapat melakukan `exec` tanpa crash Bionic.
2. **Fixed Thought Signature**:
   Google CloudCode PA mewajibkan bypass validator `thoughtSignature` pada setiap pemanggilan function/tool (`skip_thought_signature_validator`).
3. **Exa Web Search**:
   Skill web search menggunakan provider Exa AI (`~/.local/share/prime-agent/skills/websearch/`).
4. **Fast MCP Bridge**:
   Bridge MCP host diproteksi dengan timeout agar panggilan alat MCP yang macet langsung fail-fast tanpa menggantung kernel.
