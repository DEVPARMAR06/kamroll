# Contributing to Kamroll

Thanks for helping. Bug reports with a clear description are as useful as code.

## Before you start

- Open an issue for anything bigger than a small fix, so we can agree on the approach.
- Kamroll only ever **reads** the iPhone and never deletes your files. Changes that would break either promise will not be accepted.
- Nothing may leave the user's computer: no telemetry, no network calls except to a local Ollama on `127.0.0.1`.

## Set up

```powershell
python -m venv .venv; .venv\Scripts\activate          # macOS/Linux: source .venv/bin/activate
pip install -e "reference[dev]"
cd engine; cargo build; cargo test; cd ..
python -m pytest
```

You need Rust (stable) and Python 3.11+. The desktop window also needs Node 22 (`cd desktop; npm install; npm run dev`).

## How the code is checked

The Rust engine in `engine/` is the product. `reference/` is the original Python engine, kept only as the oracle for the tests in `tests/test_contract_rust*.py`: they run the same session on both engines (against a stand-in phone folder and a fake Ollama) and compare every answer, every manifest row and every file on disk.

So a behaviour change goes in both engines, with a test that shows the same result from each. If you add an API route, add it to the contract test and re-export `docs/engine-openapi.json`.

## Pull requests

- Keep them focused, with a short description of what changed and why.
- `cargo test` and `python -m pytest` must pass. CI runs both on Windows, macOS and Linux.
- UI changes: keep it plain HTML, CSS and JavaScript (no build step), keep light and dark themes working, and keep every screen usable with the keyboard.
- Photos are people's memories. Be careful with anything that moves, renames or overwrites a file, and make it recoverable.

## Reporting a security problem

Do not open a public issue. See [SECURITY.md](SECURITY.md).
