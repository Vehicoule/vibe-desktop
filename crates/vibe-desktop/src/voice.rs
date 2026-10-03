//! Voice I/O — dictation (mic → Voxtral realtime WS → text deltas) and
//! narration (`narration/summarize` → TTS `audio/speech` → playback).
//! Mirrors upstream `vibe/cli-rust/src/voice` and `vibe/cli/tts`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use vibe_protocol::models::PublicHistoryEntry;

/// Config pulled from `config/read`'s `transcription` view.
pub struct TranscriptionConfig {
    pub api_base: String,
    pub api_key_env_var: String,
    pub model_name: String,
    pub encoding: String,
    pub target_streaming_delay_ms: u64,
}

/// Config pulled from `config/read`'s `speech` view.
pub struct SpeechConfig {
    pub api_base: String,
    pub api_key_env_var: String,
    pub model_name: String,
    pub voice: String,
    pub response_format: String,
}

/// Events a dictation run yields back to the UI (futures channel — the UI
/// pump consumes it on the gpui executor).
pub enum DictationEvent {
    TextDelta(String),
    Notice(String),
    Error(String),
    Done,
}

/// Narration phase events (UI sets Preparing itself before spawning).
pub enum NarrationEvent {
    Speaking,
    Done,
    Error(String),
}

type DictationTx = futures::channel::mpsc::UnboundedSender<DictationEvent>;

/// User + assistant message text for `narration/summarize`, scoped to the
/// completed turn — pairing messages across turns would narrate the wrong
/// response. None when the turn has no assistant text.
pub fn turn_text(history: &[PublicHistoryEntry], turn_id: &str) -> Option<(String, String)> {
    let last_text = |role: &str| -> Option<String> {
        history.iter().rev().find_map(|e| match e {
            PublicHistoryEntry::Message {
                base,
                role: r,
                content,
                ..
            } if r == role && base.turn_id.as_deref() == Some(turn_id) => Some(
                content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<String>(),
            ),
            _ => None,
        })
    };
    let asst = last_text("assistant")?;
    if asst.trim().is_empty() {
        return None;
    }
    let user = last_text("user").unwrap_or_default();
    Some((user, asst))
}

/// The server rejects a flush with no audio; that is a benign empty recording.
const EMPTY_RECORDING_MARKER: &str = "before sending any audio bytes";

fn resolve_key(env_var: &str) -> Option<String> {
    std::env::var(env_var).ok().filter(|k| !k.is_empty())
}

// ── Dictation ──────────────────────────────────────────────────────────

/// Open the default input device and stream mono `pcm_s16le` chunks until
/// `stop`. Returns the chunk receiver and the captured sample rate.
/// (Upstream `audio_recorder.rs`, STREAM mode.)
pub fn start_recording(
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU32>,
) -> Result<(UnboundedReceiver<Vec<u8>>, u32), String> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "No audio input device found.".to_string())?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("Audio backend is unavailable: {e}"))?;
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    // cpal streams are not Send on macOS; build and own it on a dedicated thread.
    std::thread::spawn(move || {
        let stream = match build_stream(&device, &config, channels, sample_format, tx, peak) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        use cpal::traits::StreamTrait;
        if let Err(e) = stream.play() {
            let _ = ready_tx.send(Err(format!("Audio backend is unavailable: {e}")));
            return;
        }
        let _ = ready_tx.send(Ok(()));
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(stream);
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((rx, sample_rate)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("Audio thread exited before start".to_string()),
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    fmt: cpal::SampleFormat,
    tx: UnboundedSender<Vec<u8>>,
    peak: Arc<AtomicU32>,
) -> Result<cpal::Stream, String> {
    match fmt {
        cpal::SampleFormat::F32 => build_typed::<f32>(device, config, channels, tx, peak),
        cpal::SampleFormat::I16 => build_typed::<i16>(device, config, channels, tx, peak),
        cpal::SampleFormat::U16 => build_typed::<u16>(device, config, channels, tx, peak),
        other => Err(format!("Unsupported sample format {other:?}")),
    }
}

fn build_typed<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    tx: UnboundedSender<Vec<u8>>,
    peak: Arc<AtomicU32>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + Send + 'static,
    i16: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait;
    use cpal::Sample as _;
    let err_fn = |e| log::warn!("audio stream error: {e}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut bytes = Vec::with_capacity(data.len() / channels.max(1) * 2);
                let mut block_peak = 0.0_f32;
                for frame in data.chunks(channels.max(1)) {
                    let mut acc = 0_i32;
                    for &sample in frame {
                        acc += i16::from_sample(sample) as i32;
                    }
                    let mono = (acc / channels.max(1) as i32) as i16;
                    let amp = (mono as f32 / 32768.0).abs();
                    if amp > block_peak {
                        block_peak = amp;
                    }
                    bytes.extend_from_slice(&mono.to_le_bytes());
                }
                peak.store(block_peak.to_bits(), Ordering::Relaxed);
                let _ = tx.send(bytes);
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("Audio backend is unavailable: {e}"))
}

/// `{api_base}/v1/audio/transcriptions/realtime?model=…`, forcing ws scheme.
pub fn transcribe_url(api_base: &str, model: &str) -> String {
    let base = api_base.trim_end_matches('/');
    let base = match base.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => match base.strip_prefix("http://") {
            Some(rest) => format!("ws://{rest}"),
            None => base.to_string(),
        },
    };
    format!("{base}/v1/audio/transcriptions/realtime?model={model}")
}

fn session_update(cfg: &TranscriptionConfig, sample_rate: u32) -> Value {
    json!({
        "type": "session.update",
        "session": {
            "audio_format": {"encoding": cfg.encoding, "sample_rate": sample_rate},
            "target_streaming_delay_ms": cfg.target_streaming_delay_ms,
        }
    })
}

fn append_msg(chunk: &[u8]) -> Value {
    json!({"type": "input_audio.append", "audio": base64::engine::general_purpose::STANDARD.encode(chunk)})
}

/// One realtime event's type discriminator (text frames only).
fn event_kind(value: &Value) -> &str {
    value.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Stream `chunks` to the realtime endpoint; forward text deltas on `events`.
pub async fn transcribe(
    cfg: TranscriptionConfig,
    sample_rate: u32,
    chunks: UnboundedReceiver<Vec<u8>>,
    events: DictationTx,
) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let api_key = resolve_key(&cfg.api_key_env_var)
        .ok_or_else(|| format!("${} is not set", cfg.api_key_env_var))?;
    let mut req = transcribe_url(&cfg.api_base, &cfg.model_name)
        .as_str()
        .into_client_request()
        .map_err(|e| format!("Bad transcription URL: {e}"))?;
    let auth = format!("Bearer {api_key}")
        .parse()
        .map_err(|_| "Invalid API key".to_string())?;
    req.headers_mut().insert("authorization", auth);

    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("Transcription connection failed: {e}"))?;
    let (mut write, mut read) = ws.split();

    write
        .send(Message::text(session_update(&cfg, sample_rate).to_string()))
        .await
        .map_err(|e| format!("Transcription send failed: {e}"))?;

    let send = tokio::spawn(async move {
        let mut chunks = chunks;
        while let Some(chunk) = chunks.recv().await {
            let msg = append_msg(&chunk).to_string();
            if write.send(Message::text(msg)).await.is_err() {
                return;
            }
        }
        let _ = write
            .send(Message::text(
                json!({"type": "input_audio.flush"}).to_string(),
            ))
            .await;
        let _ = write
            .send(Message::text(
                json!({"type": "input_audio.end"}).to_string(),
            ))
            .await;
    });

    let mut got_text = false;
    while let Some(Ok(msg)) = read.next().await {
        let Message::Text(text) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        match event_kind(&value) {
            "transcription.text.delta" => {
                if let Some(delta) = value.get("text").and_then(Value::as_str) {
                    got_text = true;
                    let _ = events.unbounded_send(DictationEvent::TextDelta(delta.to_string()));
                }
            }
            "transcription.done" => break,
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Transcription error");
                if message.contains(EMPTY_RECORDING_MARKER) {
                    break;
                }
                let _ = events.unbounded_send(DictationEvent::Error(message.to_string()));
            }
            _ => {}
        }
    }
    send.abort();
    if !got_text {
        let _ = events.unbounded_send(DictationEvent::Notice("No speech detected".to_string()));
    }
    Ok(())
}

// ── Narration / TTS ────────────────────────────────────────────────────

fn speech_url(api_base: &str) -> String {
    format!("{}/v1/audio/speech", api_base.trim_end_matches('/'))
}

fn speech_body(cfg: &SpeechConfig, text: &str) -> Value {
    json!({
        "model": cfg.model_name,
        "input": text,
        "voice_id": cfg.voice,
        "response_format": cfg.response_format,
    })
}

/// Fetch audio bytes for `text` from the TTS endpoint.
pub async fn speak(cfg: &SpeechConfig, text: &str) -> Result<Vec<u8>, String> {
    let api_key = resolve_key(&cfg.api_key_env_var)
        .ok_or_else(|| format!("${} is not set", cfg.api_key_env_var))?;
    let client = reqwest::Client::new();
    let resp = client
        .post(speech_url(&cfg.api_base))
        .bearer_auth(api_key)
        .json(&speech_body(cfg, text))
        .send()
        .await
        .map_err(|e| format!("TTS request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("TTS failed: {}", resp.status()));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("TTS response unreadable: {e}"))?;
    let b64 = body
        .get("audio_data")
        .or_else(|| body.get("audioData"))
        .and_then(Value::as_str)
        .ok_or_else(|| "TTS response has no audio_data".to_string())?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("TTS audio decode failed: {e}"))
}

/// Play decoded audio bytes (wav/mp3/flac per `response_format`) on the
/// default output device; blocks until finished or `stop` flips.
pub async fn play(bytes: Vec<u8>, stop: Arc<AtomicBool>) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let (_stream, handle) = rodio::OutputStream::try_default()
            .map_err(|e| format!("Audio output unavailable: {e}"))?;
        let sink =
            rodio::Sink::try_new(&handle).map_err(|e| format!("Audio output unavailable: {e}"))?;
        let cursor = std::io::Cursor::new(bytes);
        let source =
            rodio::Decoder::new(cursor).map_err(|e| format!("Audio decode failed: {e}"))?;
        sink.append(source);
        while !stop.load(Ordering::Relaxed) && !sink.empty() {
            std::thread::sleep(Duration::from_millis(30));
        }
        sink.stop();
        Ok(())
    })
    .await
    .map_err(|e| format!("Playback task failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibe_protocol::models::{ContentBlock, HistoryEntryBase};

    fn msg(id: &str, turn: &str, role: &str, text: &str) -> PublicHistoryEntry {
        PublicHistoryEntry::Message {
            base: HistoryEntryBase {
                id: id.into(),
                session_id: "s".into(),
                turn_id: Some(turn.into()),
                created_at: 0,
                updated_at: 0,
                generation_status: "completed".into(),
                related_entry_id: None,
            },
            role: role.into(),
            content: vec![ContentBlock::text(text)],
            source: None,
            user_display_content: None,
        }
    }

    #[test]
    fn turn_text_stays_within_the_completed_turn() {
        let history = vec![
            msg("u1", "t-a", "user", "fix tests"),
            msg("a1", "t-a", "assistant", "fixed"),
            msg("u2", "t-b", "user", "explain logs"),
        ];
        // Turn B completed with no assistant message — must not borrow
        // turn A's answer.
        assert_eq!(turn_text(&history, "t-b"), None);
        assert_eq!(
            turn_text(&history, "t-a"),
            Some(("fix tests".to_string(), "fixed".to_string()))
        );
    }

    #[test]
    fn transcribe_url_forces_ws_scheme() {
        assert_eq!(
            transcribe_url("https://api.mistral.ai", "vox"),
            "wss://api.mistral.ai/v1/audio/transcriptions/realtime?model=vox"
        );
        assert_eq!(
            transcribe_url("http://localhost:9999/", "m"),
            "ws://localhost:9999/v1/audio/transcriptions/realtime?model=m"
        );
    }

    #[test]
    fn session_update_carries_format_and_delay() {
        let cfg = TranscriptionConfig {
            api_base: String::new(),
            api_key_env_var: String::new(),
            model_name: String::new(),
            encoding: "pcm_s16le".into(),
            target_streaming_delay_ms: 240,
        };
        let v = session_update(&cfg, 16000);
        assert_eq!(v["session"]["audio_format"]["encoding"], "pcm_s16le");
        assert_eq!(v["session"]["audio_format"]["sample_rate"], 16000);
        assert_eq!(v["session"]["target_streaming_delay_ms"], 240);
    }

    #[test]
    fn append_msg_base64s_audio() {
        let v = append_msg(&[1, 2, 3]);
        assert_eq!(v["type"], "input_audio.append");
        assert_eq!(v["audio"], "AQID");
    }

    #[test]
    fn speech_body_matches_upstream() {
        let cfg = SpeechConfig {
            api_base: String::new(),
            api_key_env_var: String::new(),
            model_name: "voxtral-mini-tts-latest".into(),
            voice: "alex".into(),
            response_format: "wav".into(),
        };
        let v = speech_body(&cfg, "hello");
        assert_eq!(v["model"], "voxtral-mini-tts-latest");
        assert_eq!(v["input"], "hello");
        assert_eq!(v["voice_id"], "alex");
        assert_eq!(v["response_format"], "wav");
    }
}
