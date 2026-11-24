# iPhone Photo Vault

Back up an iPhone's photos and videos to an external SSD at **original quality**, on Windows and macOS. Crash-safe, resumable, and verifiable. It also sorts what it copied into dated folders, finds your trips from the location saved in your photos, and can file each trip into its own folder.

- **Originals, untouched.** Files are copied byte for byte over USB. No conversion, so EXIF and GPS, Live Photo clips and `.AAE` edit files all survive. The iPhone is only ever read.
- **Fault tolerant.** Each file is written to a temporary file, checked, then renamed into place. Pull the cable or the power and the next run resumes where it stopped. Every copy has a SHA-256 fingerprint you can re-check.
- **Proves it matches.** A Health screen compares the phone with the SSD and can re-read every file to catch silent damage. Problems can be queued to be copied again.
- **Organised by when and where.** Dates come from the photo or video itself. Trips come from GPS, with offline place names. You can rename, add to, take from, create and delete trips, and optionally file each trip into `Trips/<year>/<name> (<month year>)/`.
- **Tags, no cloud.** Simple rules tag memories, trips, screenshots, chat images, duplicates and junk, with the reasons shown. An optional local Ollama model can suggest trip titles, and describe pictures if you give it a vision model.
- **Private.** Nothing leaves your computer. The engine listens on `127.0.0.1` only, behind a per-launch token.

The iPhone is read through Apple's USB service. On Windows, install **Apple Devices** from the Microsoft Store (or iTunes). macOS needs nothing extra. Format the SSD **exFAT** to use it on both.

## Run it

```powershell
python -m venv .venv
.venv\Scripts\activate            # macOS/Linux: source .venv/bin/activate
pip install -e ".[dev]"

photovault ui                      # opens the app in your browser
photovault run --limit 200         # or use the command line: copy a small test batch
photovault verify --deep
```

Settings live in `config.toml` (copy `config.example.toml`) or in the app's Setup screen. The backup folder name, the SSD path, the folder layout and every other path are configurable.

### Desktop app

```powershell
cd desktop
npm install
npm run dev                        # opens the app in its own window
```

The window starts the Python engine, waits for it, and stops it on exit. To make an installer that needs no Python, bundle the engine first, then build:

```powershell
pyinstaller --noconfirm --onedir --noconsole --name photovault-engine --distpath desktop/engine-dist `
  --add-data "src/photovault/ui;photovault/ui" --paths src `
  --collect-all pymobiledevice3 --collect-all geonamescache --collect-all pillow_heif --collect-all av `
  --collect-submodules uvicorn --collect-submodules fastapi --collect-submodules starlette --collect-submodules photovault `
  desktop/engine_entry.py
cd desktop; npm run build:win     # or build:mac on a Mac
```

Building needs Rust (current stable) and, on Windows, the Visual Studio C++ build tools with the Windows SDK. See `.github/workflows/desktop.yml` for the Windows and macOS build.

## How it is built

```
UI (HTML/JS)  ──  JSON and server-sent events on 127.0.0.1  ──  Engine (Python)  ──  iPhone (USB) · SSD · manifest.db
```

- **The engine** (`src/photovault`) does all the work: device access, the crash-safe copier, the manifest, metadata, tagging, trips, filing, verification and the optional Ollama client.
- **The manifest** is a SQLite file stored on the SSD (`<backup>/.photovault/manifest.db`), so the SSD is the source of truth and moves between machines.
- **The UI** (`src/photovault/ui`) talks only to the engine API. The API is exported as OpenAPI in `docs/engine-openapi.json`, so the engine can be reimplemented (for example in Rust) without touching the UI.
- **The desktop shell** (`desktop/`, Tauri) hosts the UI in a native window.

## The Rust engine

`engine-rs/` is a Rust engine that serves the same UI and the same API, taken over one route at a time. Because the UI only talks to the API, either engine can run behind it.

```powershell
cd engine-rs
cargo build
target\debug\photovault-engine ui --port 8765      # same flags as `photovault ui`
```

**Ported so far (and proven identical):** everything the app needs. Settings, drives and folders, status, the Review and Trips screens and every edit to them, tagging and trip detection (analysis), thumbnails, the whole backup (scan, resumable crash-safe copy, capture dates and GPS, live progress), Health (deep check, repair, phone-vs-SSD comparison) and filing photos into trip folders. `tests/test_contract_rust*.py` run the same sessions on the Python and the Rust engine, from identical copies of a vault and a stand-in phone folder (`PHOTOVAULT_FAKE_DEVICE`), and compare every answer, the manifest rows and the files on disk.

**Checked on a real iPhone 15 Pro:** the Rust engine's own iPhone link (`engine-rs/src/afc.rs`) listed 21,514 files and copied 305 (HEIC, JPEG, PNG, MP4); the manifest rows, dates and SHA-256 hashes matched the Python engine's on the same files, and HEIC and video previews drew through the operating system's thumbnailer. A full 120 GB run has not been done, and macOS is untested. HEIC photos get their camera data and size from the file header only. **Still Python only:** smart tags (Ollama) and the old-folder tools. Those routes answer `501 not ported`. To run the desktop app on the Rust engine, set `PHOTOVAULT_ENGINE` to the built program.

## Safety notes

- The phone is never written to or changed.
- Nothing is deleted. Filing, tags and trip edits only rename files inside the backup folder or write to the manifest, and each rename is journaled so a crash can be finished or undone.
- Matching an old backup folder by name and size is a shortcut. Use `photovault compare-old` before trusting it, and `photovault verify --deep` to confirm contents.

## Tests

```powershell
pytest
```

The suite uses a fake iPhone and a fake Ollama, so it needs no hardware.

## Status

Working: backup, resume, verify, trips, tags, filing, the web UI and the Windows desktop window. Not yet: a macOS build that has been run on a Mac, vision-model tagging tested against a real model, and the rest of the Rust engine (see below).

## Licence

MIT. See `LICENSE`.
