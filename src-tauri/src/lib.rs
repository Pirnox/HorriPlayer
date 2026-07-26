mod audio;

use std::sync::Mutex;
use std::time::Duration;
use tauri::http::Response;
use tauri::State;

use audio::probe::{probe_file, TrackInfo};
use audio::{AudioEngine, EngineState};

/// The engine is created lazily: building it opens the output device, and we
/// only want to do that once the UI actually plays something.
#[derive(Default)]
struct AudioState(Mutex<Option<std::sync::Arc<AudioEngine>>>);

fn engine(
    state: &State<'_, AudioState>,
    app: &tauri::AppHandle,
) -> Result<std::sync::Arc<AudioEngine>, String> {
    let mut slot = state.0.lock().map_err(|_| "audio state poisoned")?;
    if let Some(e) = slot.as_ref() {
        return Ok(std::sync::Arc::clone(e));
    }
    let created = std::sync::Arc::new(AudioEngine::new(app.clone())?);
    *slot = Some(std::sync::Arc::clone(&created));
    Ok(created)
}

#[tauri::command]
fn audio_load(
    path: String,
    autoplay: bool,
    state: State<'_, AudioState>,
    app: tauri::AppHandle,
) -> Result<u64, String> {
    engine(&state, &app)?.load(&path, autoplay)
}

#[tauri::command]
fn audio_set_next(
    path: Option<String>,
    state: State<'_, AudioState>,
    app: tauri::AppHandle,
) -> Result<u64, String> {
    engine(&state, &app)?.set_next(path.as_deref())
}

#[tauri::command]
fn audio_play(state: State<'_, AudioState>, app: tauri::AppHandle) -> Result<(), String> {
    engine(&state, &app)?.play()
}

#[tauri::command]
fn audio_pause(state: State<'_, AudioState>, app: tauri::AppHandle) -> Result<(), String> {
    engine(&state, &app)?.pause()
}

#[tauri::command]
fn audio_stop(state: State<'_, AudioState>, app: tauri::AppHandle) -> Result<(), String> {
    engine(&state, &app)?.stop()
}

#[tauri::command]
fn audio_seek(
    position: f64,
    state: State<'_, AudioState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    engine(&state, &app)?.seek(position)
}

#[tauri::command]
fn audio_set_volume(
    volume: f32,
    state: State<'_, AudioState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    engine(&state, &app)?.set_volume(volume);
    Ok(())
}

#[tauri::command]
fn audio_set_eq(
    gains: Vec<f32>,
    preamp: f32,
    enabled: bool,
    state: State<'_, AudioState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    engine(&state, &app)?.set_eq(&gains, preamp, enabled);
    Ok(())
}

#[tauri::command]
fn audio_state(state: State<'_, AudioState>, app: tauri::AppHandle) -> Result<EngineState, String> {
    Ok(engine(&state, &app)?.state())
}

/// Read tags, cover art and stream details for a batch of files. Runs off the
/// UI thread; unreadable files are skipped rather than failing the whole batch.
#[tauri::command]
async fn audio_probe(paths: Vec<String>) -> Result<Vec<TrackInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        paths
            .iter()
            .filter_map(|p| probe_file(p).ok())
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Decode base64url (no padding) without pulling in a crate.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        })
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Proxy a remote audio stream through the app so it becomes same-origin:
/// that is what lets the equalizer and the spectrum analyser see server
/// streams at all — cross-origin media without CORS headers is silenced by
/// the Web Audio API. Range requests are forwarded so seeking still works.
async fn proxy_audio(b64_url: String, range: Option<String>) -> Response<Vec<u8>> {
    let bad = |code: u16, msg: &str| {
        Response::builder()
            .status(code)
            .header("Access-Control-Allow-Origin", "*")
            .body(msg.as_bytes().to_vec())
            .unwrap()
    };
    let url = match b64url_decode(&b64_url).and_then(|b| String::from_utf8(b).ok()) {
        Some(u) => u,
        None => return bad(400, "bad url"),
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return bad(400, "only http/https URLs are allowed");
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(e) => return bad(500, &e.to_string()),
    };
    let mut req = client.get(&url);
    if let Some(r) = &range {
        req = req.header("Range", r.clone());
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return bad(502, &e.to_string()),
    };
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    let content_type = header("content-type").unwrap_or_else(|| "audio/mpeg".into());
    let content_range = header("content-range");
    let bytes = match resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => return bad(502, &e.to_string()),
    };
    let mut builder = Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Accept-Ranges", "bytes")
        .header("Access-Control-Allow-Origin", "*");
    if let Some(cr) = content_range {
        builder = builder.header("Content-Range", cr);
    }
    builder
        .body(bytes)
        .unwrap_or_else(|_| bad(500, "response build failed"))
}

/// HTTP GET returning the response body as text.
/// Used by the UI for Subsonic/Navidrome API calls — replaces the browser
/// JSONP hack with a real request that has timeouts and proper errors.
#[tauri::command]
async fn http_get_text(url: String) -> Result<String, String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("only http/https URLs are allowed".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(text)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .register_asynchronous_uri_scheme_protocol("hpaudio", |_app, request, responder| {
            let b64 = request.uri().path().trim_start_matches('/').to_string();
            let range = request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            tauri::async_runtime::spawn(async move {
                responder.respond(proxy_audio(b64, range).await);
            });
        })
        .plugin(tauri_plugin_dialog::init())
        .manage(AudioState::default())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            http_get_text,
            audio_load,
            audio_set_next,
            audio_play,
            audio_pause,
            audio_stop,
            audio_seek,
            audio_set_volume,
            audio_set_eq,
            audio_state,
            audio_probe
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::b64url_decode;

    #[test]
    fn decodes_base64url_from_the_frontend() {
        // produced by the UI's btoa(...) encoder for the URL asserted below
        let encoded = "aHR0cDovLzE5Mi4xNjguMS4xMDo0NTMzL3Jlc3Qvc3RyZWFtP3U9YWRtaW4mdD1hYmMxMjMmcz14eSZ2PTEuMTYuMSZjPWhvcnJpcGxheWVyJmY9anNvbiZpZD10ci00Mn7EhcSZ";
        let decoded = String::from_utf8(b64url_decode(encoded).expect("decodes")).expect("utf-8");
        assert_eq!(
            decoded,
            "http://192.168.1.10:4533/rest/stream?u=admin&t=abc123&s=xy&v=1.16.1&c=horriplayer&f=json&id=tr-42~ąę"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(b64url_decode("not base64!!").is_none());
    }
}
