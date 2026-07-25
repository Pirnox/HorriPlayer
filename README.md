# HorriPlayer

Music player for local files and Subsonic/Navidrome servers. Runs as a Windows
desktop app (Tauri + Rust) and, unchanged, as a single HTML file in a browser.

## Features

- **Native audio engine (desktop)** — Symphonia decodes, cpal plays, and the
  whole path runs in Rust off the UI thread. Lossless FLAC, ALAC, WAV, AIFF and
  CAF decode bit-perfect; MP3, AAC, Vorbis, Opus and ADPCM are supported too.
  Gapless track changes, sample-rate conversion only when the device needs it,
  and the format of what is playing is shown in the player bar.
- **Local files** — added through a native file picker; tags, cover art, sample
  rate and bit depth come from the same decoder that plays the file. The browser
  build falls back to `<audio>` with built-in ID3v2/FLAC tag parsers.
- **Servers** — Subsonic/Navidrome libraries: albums, artists, playlists, songs,
  search. In the desktop app requests go through Rust (`reqwest`) with real HTTP
  errors and timeouts; the browser build falls back to JSONP.
- **10-band equalizer** — 31 Hz to 16 kHz, preamp, 8 presets, settings persist.
  RBJ biquads, running in the audio callback in the desktop build and in Web
  Audio in the browser. A flat setting is bypassed entirely, so it stays
  bit-transparent unless you actually move a slider.
- **Spectrum visualizer** — Canvas2D, follows the accent colour. Bars come from
  an FFT in Rust on the desktop, from an AnalyserNode in the browser.
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
| `src-tauri/src/lib.rs` | Tauri commands: Subsonic HTTP, `hpaudio` proxy, audio transport. |
| `src-tauri/src/audio/mod.rs` | Engine: decoder, output and monitor threads. |
| `src-tauri/src/audio/eq.rs` | Ten-band biquad equalizer. |
| `src-tauri/src/audio/probe.rs` | Tags, cover art and stream details. |
| `src-tauri/src/audio/http_source.rs` | Seekable HTTP source for server streams. |
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

## Two playback paths

The desktop build never uses `<audio>`: `Player` in the UI forwards transport
calls to the Rust engine and renders state from the `audio:tick` event it emits
~30×/s. The browser build keeps the older path, where cross-origin media piped
through `createMediaElementSource` is silenced without CORS headers — so there,
local files play on an element wired into the Web Audio graph while server
streams play on a plain element, audible but without EQ or spectrum. The
`hpaudio` proxy exists for that browser-shaped limitation and still serves as
the fallback if the native engine cannot open a stream.

Playback position survives gapless changes because the decoder pushes markers
onto the same frame timeline the output callback counts on, rather than relying
on a counter that a track change would invalidate.

## Status

Working on Windows: everything listed above.

Planned: Linux packaging (AppImage/deb) and auto-updates. WavPack, Musepack and
APE are not supported — Symphonia has no decoders for them.
