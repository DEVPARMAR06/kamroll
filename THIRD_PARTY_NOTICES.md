# Third-party notices

Kamroll itself is MIT licensed (see `LICENSE`). It includes or builds on the following.

## Data

- **GeoNames cities (15,000+ inhabitants)**, used offline to name the places of a trip (`engine/data/cities.tsv`, `engine/data/countries.tsv`). Data from [GeoNames](https://www.geonames.org/), licensed under [Creative Commons Attribution 4.0](https://creativecommons.org/licenses/by/4.0/). The files were exported from the `geonamescache` Python package, which bundles the GeoNames dumps.

## Software the engine and app are built from

Everything below is permissively licensed. Full licence texts ship with each crate; run `cargo about` or `cargo license` in `engine/` and `desktop/src-tauri/` for the complete list.

| Component | Licence |
|---|---|
| Rust standard library and most crates in `engine/` (axum, tokio, rusqlite and bundled SQLite, serde, chrono, clap, regex, image, jpeg-decoder, kamadak-exif, rustls with ring, plist, ureq, sha1, sha2, include_dir, sysinfo, toml, base64) | MIT, Apache-2.0, ISC, BSD, Zlib, Unicode-3.0 or the public domain, as listed per crate |
| [Tauri](https://tauri.app) and its plugins (`desktop/`) | MIT or Apache-2.0 (a few of its dependencies are MPL-2.0, which applies to those files only) |
| SQLite | Public domain |
| Windows system API bindings (`windows` crate) | MIT or Apache-2.0 |

## The Python reference engine (`reference/`, not shipped in the apps)

It depends on `pymobiledevice3` (GPL-3.0), FastAPI, Pillow, pillow-heif and PyAV. These are only installed for development and testing. The installers contain only the Rust engine, which has its own implementation of the iPhone link written from the public protocol descriptions and shares no code with those libraries.

## Trademarks

"iPhone", "iCloud", "Live Photos", "macOS" and "Apple" are trademarks of Apple Inc. "Windows" is a trademark of Microsoft Corporation. They are used only to describe what Kamroll works with. Kamroll is not affiliated with or endorsed by either company.
