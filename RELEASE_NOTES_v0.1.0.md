HorriPlayer v0.1.0 — first release.

A music player for local files and Subsonic/Navidrome servers. The Windows app
decodes and mixes audio natively in Rust; the same UI also runs as a standalone
HTML file in a browser.

## Audio

- **Native engine** — Symphonia decoding into a cpal output stream, running off
  the UI thread. Decoder, output callback and analyser sit on separate threads
  with a lock-free ring between them, so the callback never allocates or blocks.
- **Lossless, bit-perfect** — FLAC, ALAC, WAV, AIFF and CAF. Also MP3, AAC,
  Vorbis, Opus and ADPCM.
- **Gapless playback** — the next track starts without draining the buffer.
- **Sample-rate conversion only when needed** — windowed-sinc resampling engages
  only if the output device rate differs from the file's.
- **10-band equalizer** — 31 Hz to 16 kHz with preamp and 8 presets, applied in
  the audio callback. A flat setting is bypassed entirely, so playback stays
  bit-transparent unless a slider is moved.

## Library

- Local files added through a native picker; tags, cover art, sample rate and
  bit depth are read by the same decoder that plays the file — works offline.
- Subsonic/Navidrome: albums, artists, playlists, songs and search, over HTTP
  from Rust with real errors and timeouts.
- Synced lyrics from the server (OpenSubsonic) or LRCLIB, with the current line
  highlighted and click-to-seek.

## Interface

- Dark, OLED and Light themes with six accent colours.
- Spectrum visualiser fed by an FFT in Rust.
- Shuffle, repeat (off/all/one), media keys and lock-screen integration.
- Playing format shown in the player bar, e.g. `FLAC · 24-bit · 96 kHz`.

## Install

Download `HorriPlayer_0.1.0_x64-setup.exe` and run it. Windows 10/11 64-bit;
WebView2 is already part of Windows 11. The installer is unsigned, so
SmartScreen will warn on first run — choose "More info" then "Run anyway".

## Known limits

- Windows only for now; Linux packaging is planned.
- WavPack, Musepack and APE are not supported — Symphonia has no decoders for
  them.
- In the browser build, server streams play without EQ or spectrum (a CORS
  limitation that does not apply to the desktop app).
