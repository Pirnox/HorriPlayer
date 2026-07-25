# HorriPlayer

Music player for local files and Subsonic/Navidrome servers. Runs as a Windows
desktop app (Tauri + Rust) and, unchanged, as a single HTML file in a browser.

## Features

- **Local files** — MP3, FLAC, WAV, OGG/Opus, M4A/AAC, WebM. Tags and cover art
  are read by built-in ID3v2 and FLAC parsers, so metadata works offline.
- **Servers** — Subsonic/Navidrome libraries: albums, artists, playlists, songs,
  search. In the desktop app requests go through Rust (`reqwest`) with real HTTP
  errors and timeouts; the browser build falls back to JSONP.
- **10-band equalizer** — 31 Hz to 16 kHz, preamp, 8 presets, settings persist.
- **Spectrum visualizer** — Canvas2D, follows the accent colour.
- **Lyrics** — synced lyrics from the server (OpenSubsonic) or LRCLIB, with the
  current line highlighted; click a line to seek there.
- **Themes** — Dark / OLED / Light plus six accent colours.
- Shuffle, repeat (off/all/one), media-key and lock-screen integration, volume,
  queue and playback settings remembered between sessions.

## Layout

| Path | Purpose |
| --- | --- |
| `index.html` | The whole UI and player. **Source of truth** — edit this file. |
| `ui/index.html` | Build input for Tauri. A copy of `index.html`. |
| `src-tauri/src/lib.rs` | Rust side: HTTP for the Subsonic API, `hpaudio` stream proxy. |
| `src-tauri/tauri.conf.json` | Window, bundle and app identity. |

After editing `index.html`, copy it into `ui/` before building:

```bash
cp index.html ui/index.html
```

## Build

Requires Rust (MSVC toolchain), Node.js and Visual Studio Build Tools.

```bash
npm install
```

```bash
npx tauri dev
```

```bash
npx tauri build
```

`tauri build` produces `src-tauri/target/release/horriplayer.exe` and an NSIS
installer under `src-tauri/target/release/bundle/nsis/`.

```bash
cd src-tauri && cargo test --lib
```

## Why audio is routed through two elements

Cross-origin media piped through `createMediaElementSource` is silenced by the
Web Audio API when the server sends no CORS headers. So local files (and, in the
desktop app, streams proxied through the Rust `hpaudio` protocol) play on an
element wired into the equalizer graph, while unproxied server streams play on a
plain element — audible, but without EQ or spectrum. The proxy forwards Range
requests, so seeking still works.

## Status

Working: everything listed above, on Windows.

Planned: a native Rust audio engine (Symphonia + cpal) for ALAC/AIFF and other
formats, gapless playback and DSP in Rust; Linux packaging (AppImage/deb);
auto-updates.
