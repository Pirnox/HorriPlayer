//! Reading tags, cover art and stream details straight from the file, so the
//! native library does not depend on the WebView being able to see the file.

use std::fs::File;
use std::path::Path;

use serde::Serialize;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTagKey, Visual};
use symphonia::core::probe::Hint;

/// Cover art larger than this is skipped — it would only bloat the IPC payload.
const MAX_COVER_BYTES: usize = 3 * 1024 * 1024;

#[derive(Serialize, Default, Clone)]
pub struct TrackInfo {
    pub path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: f64,
    pub sample_rate: u32,
    pub bit_depth: u32,
    pub channels: u32,
    pub codec: String,
    pub lossless: bool,
    pub cover: Option<String>,
}

pub fn probe_file(path: &str) -> Result<TrackInfo, String> {
    let p = Path::new(path);
    let file = File::open(p).map_err(|e| format!("cannot open file: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
        hint.with_extension(&ext.to_ascii_lowercase());
    }

    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported or corrupt audio: {e}"))?;

    let mut info = TrackInfo {
        path: path.to_string(),
        ..Default::default()
    };

    if let Some(track) = probed.format.default_track() {
        let cp = &track.codec_params;
        info.sample_rate = cp.sample_rate.unwrap_or(0);
        info.bit_depth = cp.bits_per_sample.or(cp.bits_per_coded_sample).unwrap_or(0);
        info.channels = cp.channels.map(|c| c.count() as u32).unwrap_or(0);
        info.codec = super::codec_name(cp.codec);
        info.lossless = super::codec_is_lossless(cp.codec);
        if let (Some(frames), Some(sr)) = (cp.n_frames, cp.sample_rate) {
            info.duration = frames as f64 / sr as f64;
        }
    }

    // Tags can sit in the container metadata or in a leading ID3 block.
    if let Some(rev) = probed.format.metadata().current() {
        apply_metadata(&mut info, rev);
    }
    if let Some(rev) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        apply_metadata(&mut info, rev);
    }

    if info.title.is_none() {
        info.title = p
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());
    }
    Ok(info)
}

fn apply_metadata(info: &mut TrackInfo, rev: &MetadataRevision) {
    for tag in rev.tags() {
        let value = tag.value.to_string();
        if value.trim().is_empty() {
            continue;
        }
        match tag.std_key {
            Some(StandardTagKey::TrackTitle) if info.title.is_none() => info.title = Some(value),
            Some(StandardTagKey::Artist) if info.artist.is_none() => info.artist = Some(value),
            Some(StandardTagKey::AlbumArtist) if info.artist.is_none() => info.artist = Some(value),
            Some(StandardTagKey::Album) if info.album.is_none() => info.album = Some(value),
            _ => {}
        }
    }
    if info.cover.is_none() {
        if let Some(v) = rev.visuals().iter().find(|v| !v.data.is_empty()) {
            info.cover = visual_to_data_url(v);
        }
    }
}

fn visual_to_data_url(v: &Visual) -> Option<String> {
    if v.data.len() > MAX_COVER_BYTES {
        return None;
    }
    let mime = if v.media_type.is_empty() {
        "image/jpeg"
    } else {
        v.media_type.as_str()
    };
    Some(format!("data:{};base64,{}", mime, base64_encode(&v.data)))
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_binary_bytes() {
        assert_eq!(base64_encode(&[0xff, 0xd8, 0xff]), "/9j/");
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        assert!(super::probe_file("Z:\\definitely\\missing.flac").is_err());
    }

    /// Write a 16-bit stereo WAV so the probe runs against a real container.
    fn write_test_wav(path: &std::path::Path, secs: u32, rate: u32) {
        use std::io::Write;
        let frames = secs * rate;
        let data_len = frames * 2 * 2; // stereo, 2 bytes per sample
        let mut f = std::fs::File::create(path).expect("create wav");
        let mut hdr = Vec::new();
        hdr.extend(b"RIFF");
        hdr.extend(&(36 + data_len).to_le_bytes());
        hdr.extend(b"WAVEfmt ");
        hdr.extend(&16u32.to_le_bytes());
        hdr.extend(&1u16.to_le_bytes()); // PCM
        hdr.extend(&2u16.to_le_bytes()); // channels
        hdr.extend(&rate.to_le_bytes());
        hdr.extend(&(rate * 4).to_le_bytes()); // byte rate
        hdr.extend(&4u16.to_le_bytes()); // block align
        hdr.extend(&16u16.to_le_bytes()); // bits
        hdr.extend(b"data");
        hdr.extend(&data_len.to_le_bytes());
        f.write_all(&hdr).expect("write header");
        let mut body = Vec::with_capacity(data_len as usize);
        for i in 0..frames {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 12000.0) as i16;
            body.extend(&v.to_le_bytes());
            body.extend(&v.to_le_bytes());
        }
        f.write_all(&body).expect("write body");
    }

    #[test]
    fn probes_a_real_wav_file() {
        let dir = std::env::temp_dir().join("horriplayer-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("probe-sample.wav");
        write_test_wav(&path, 2, 44_100);

        let info = super::probe_file(path.to_str().unwrap()).expect("probe succeeds");
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bit_depth, 16);
        assert_eq!(info.codec, "PCM");
        assert!(info.lossless, "PCM must be reported as lossless");
        assert!(
            (info.duration - 2.0).abs() < 0.05,
            "expected ~2 s, got {}",
            info.duration
        );
        // No tags in the file, so the filename stands in for the title.
        assert_eq!(info.title.as_deref(), Some("probe-sample"));

        let _ = std::fs::remove_file(&path);
    }
}
