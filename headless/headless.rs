//! RustDesk headless client — exposes a local Unix socket (line-delimited
//! JSON-RPC) so an external tool (the MCP server) can: connect to a peer,
//! grab the latest decoded frame as PNG, and inject mouse/keyboard events.
//!
//! Runs inside the rustdesk crate, so it can call internal `pub` APIs
//! (`Session`, `send_mouse`, `send_key_event`, `io_loop`, …) directly.

use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, RwLock};

use hbb_common::{
    anyhow::anyhow,
    base64::{engine::general_purpose::STANDARD as B64, Engine as _},
    env_logger, libc, log,
    message_proto::*,
    rendezvous_proto::ConnType,
    serde_json::{self, json, Value},
    tokio::{
        self,
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{UnixListener, UnixStream},
        sync::oneshot,
    },
    ResultType,
};
use serde::{Deserialize, Serialize};

use librustdesk::client::{send_mouse, QualityStatus};
use librustdesk::ui_session_interface::{io_loop, InvokeUiSession, Session};

// ----- Headless handler: implements InvokeUiSession with mostly no-ops -----

#[derive(Clone, Default)]
pub struct HeadlessHandler {
    state: Arc<HeadlessState>,
}

#[derive(Default)]
pub struct HeadlessState {
    rgba: Mutex<Option<RgbaFrame>>,
    peer_info: RwLock<Option<PeerInfo>>,
    connected: RwLock<bool>,
    last_msg: Mutex<Option<String>>,
}

pub struct RgbaFrame {
    pub data: Vec<u8>,
    pub w: usize,
    pub h: usize,
    pub fmt: scrap::ImageFormat,
}

impl HeadlessHandler {
    fn new_with(state: Arc<HeadlessState>) -> Self {
        Self { state }
    }
}

impl InvokeUiSession for HeadlessHandler {
    fn set_cursor_data(&self, _cd: CursorData) {}
    fn set_cursor_id(&self, _id: String) {}
    fn set_cursor_position(&self, _cp: CursorPosition) {}
    fn set_display(&self, _x: i32, _y: i32, _w: i32, _h: i32, _cursor_embedded: bool, _scale: f64) {}
    fn switch_display(&self, _d: &SwitchDisplay) {}
    fn set_peer_info(&self, pi: &PeerInfo) {
        *self.state.peer_info.write().unwrap() = Some(pi.clone());
    }
    fn set_displays(&self, _displays: &Vec<DisplayInfo>) {}
    fn set_platform_additions(&self, _data: &str) {}
    fn on_connected(&self, _ct: ConnType) {
        *self.state.connected.write().unwrap() = true;
    }
    fn update_privacy_mode(&self) {}
    fn set_permission(&self, _name: &str, _value: bool) {}
    fn close_success(&self) {
        *self.state.connected.write().unwrap() = false;
    }
    fn update_quality_status(&self, _qs: QualityStatus) {}
    fn set_connection_type(&self, _is_secured: bool, _direct: bool, _stream_type: &str) {}
    fn set_fingerprint(&self, _fp: String) {}
    fn job_error(&self, _id: i32, _err: String, _file_num: i32) {}
    fn job_done(&self, _id: i32, _file_num: i32) {}
    fn clear_all_jobs(&self) {}
    fn new_message(&self, msg: String) {
        *self.state.last_msg.lock().unwrap() = Some(msg);
    }
    fn update_transfer_list(&self) {}
    fn load_last_job(&self, _cnt: i32, _job_json: &str, _auto_start: bool) {}
    fn update_folder_files(
        &self,
        _id: i32,
        _entries: &Vec<FileEntry>,
        _path: String,
        _is_local: bool,
        _only_count: bool,
    ) {}
    fn confirm_delete_files(&self, _id: i32, _i: i32, _name: String) {}
    fn override_file_confirm(
        &self,
        _id: i32,
        _file_num: i32,
        _to: String,
        _is_upload: bool,
        _is_identical: bool,
    ) {}
    fn update_block_input_state(&self, _on: bool) {}
    fn job_progress(&self, _id: i32, _file_num: i32, _speed: f64, _finished: f64) {}
    fn adapt_size(&self) {}
    fn on_rgba(&self, _display: usize, rgba: &mut scrap::ImageRgb) {
        *self.state.rgba.lock().unwrap() = Some(RgbaFrame {
            data: rgba.raw.clone(),
            w: rgba.w,
            h: rgba.h,
            fmt: rgba.fmt,
        });
    }
    fn msgbox(&self, msgtype: &str, title: &str, text: &str, _link: &str, _retry: bool) {
        log::info!("[msgbox] {}: {} - {}", msgtype, title, text);
        *self.state.last_msg.lock().unwrap() = Some(format!("{}:{}:{}", msgtype, title, text));
    }
    fn cancel_msgbox(&self, _tag: &str) {}
    fn switch_back(&self, _id: &str) {}
    fn portable_service_running(&self, _running: bool) {}
    fn on_voice_call_started(&self) {}
    fn on_voice_call_closed(&self, _reason: &str) {}
    fn on_voice_call_waiting(&self) {}
    fn on_voice_call_incoming(&self) {}
    fn get_rgba(&self, _display: usize) -> *const u8 {
        std::ptr::null()
    }
    fn next_rgba(&self, _display: usize) {}
    fn set_multiple_windows_session(&self, _sessions: Vec<WindowsSession>) {}
    fn set_current_display(&self, _disp_idx: i32) {}
    fn update_record_status(&self, _start: bool) {}
    fn printer_request(&self, _id: i32, _path: String) {}
    fn handle_screenshot_resp(&self, _sid: String, _msg: String) {}
    fn handle_terminal_response(&self, _response: TerminalResponse) {}
}

// ----- JSON-RPC schema -----

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct Response {
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ----- Session registry: one active session at a time (keep it simple) -----

struct Active {
    session: Session<HeadlessHandler>,
    state: Arc<HeadlessState>,
}

// ----- RPC handlers -----

fn encode_png(frame: &RgbaFrame) -> ResultType<Vec<u8>> {
    use image::{ImageBuffer, Rgba};
    // RustDesk's ImageRgb is BGRA on most platforms; convert to RGBA.
    let mut rgba = frame.data.clone();
    if matches!(frame.fmt, scrap::ImageFormat::ABGR) {
        // ABGR → RGBA: swap B and R (i.e. indices 0 and 2)
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    let img: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_raw(frame.w as u32, frame.h as u32, rgba)
            .ok_or_else(|| anyhow!("bad rgba buffer"))?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

async fn handle_request(
    req: Request,
    active: &Arc<RwLock<Option<Active>>>,
) -> Response {
    let rid = req.id.clone();
    let result: Result<Value, String> = match req.method.as_str() {
        "connect" => connect_rpc(req.params, active).await,
        "disconnect" => {
            *active.write().unwrap() = None;
            Ok(json!({}))
        }
        "status" => {
            let guard = active.read().unwrap();
            if let Some(a) = guard.as_ref() {
                let peer = a.state.peer_info.read().unwrap();
                let connected = *a.state.connected.read().unwrap();
                let (w, h) = peer
                    .as_ref()
                    .and_then(|p| p.displays.first())
                    .map(|d| (d.width, d.height))
                    .unwrap_or((0, 0));
                Ok(json!({
                    "connected": connected,
                    "width": w,
                    "height": h,
                    "peer_id": peer.as_ref().map(|p| p.username.clone()).unwrap_or_default(),
                }))
            } else {
                Ok(json!({"connected": false}))
            }
        }
        "screenshot" => {
            let guard = active.read().unwrap();
            let Some(a) = guard.as_ref() else {
                return err(rid, "not connected".into());
            };
            // Clone the latest frame instead of taking it, so successive
            // screenshot calls between two on_rgba pushes still work.
            let frame_opt: Option<RgbaFrame> = a.state.rgba.lock().unwrap().as_ref().map(|f| RgbaFrame {
                data: f.data.clone(),
                w: f.w,
                h: f.h,
                fmt: f.fmt,
            });
            drop(guard);
            match frame_opt {
                None => Err("no frame yet".to_string()),
                Some(f) => match encode_png(&f) {
                    Ok(png) => Ok(json!({
                        "png_b64": base64_encode(&png),
                        "width": f.w,
                        "height": f.h,
                    })),
                    Err(e) => Err(e.to_string()),
                },
            }
        }
        "mouse" => {
            let guard = active.read().unwrap();
            let Some(a) = guard.as_ref() else {
                return err(rid, "not connected".into());
            };
            let mask = req.params["mask"].as_i64().unwrap_or(0) as i32;
            let x = req.params["x"].as_i64().unwrap_or(0) as i32;
            let y = req.params["y"].as_i64().unwrap_or(0) as i32;
            let alt = req.params["alt"].as_bool().unwrap_or(false);
            let ctrl = req.params["ctrl"].as_bool().unwrap_or(false);
            let shift = req.params["shift"].as_bool().unwrap_or(false);
            let cmd = req.params["cmd"].as_bool().unwrap_or(false);
            send_mouse(mask, x, y, alt, ctrl, shift, cmd, &a.session);
            Ok(json!({}))
        }
        "key" => {
            let guard = active.read().unwrap();
            let Some(a) = guard.as_ref() else {
                return err(rid, "not connected".into());
            };
            let key = req.params["key"].as_str().unwrap_or("").to_string();
            let modifiers: Vec<String> = req.params["modifiers"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            match build_key_event(&key, &modifiers) {
                Some(evt) => {
                    a.session.send_key_event(&evt);
                    Ok(json!({}))
                }
                None => Err(format!("unknown key: {key}")),
            }
        }
        "type" => {
            let guard = active.read().unwrap();
            let Some(a) = guard.as_ref() else {
                return err(rid, "not connected".into());
            };
            let text = req.params["text"].as_str().unwrap_or("").to_string();
            for ch in text.chars() {
                let mut evt = KeyEvent::new();
                evt.set_seq(ch.to_string());
                evt.down = true;
                evt.press = true;
                evt.mode = KeyboardMode::Legacy.into();
                a.session.send_key_event(&evt);
            }
            Ok(json!({}))
        }
        other => Err(format!("unknown method: {other}")),
    };

    match result {
        Ok(v) => Response { id: rid, result: Some(v), error: None },
        Err(e) => err_resp(rid, e),
    }
}

fn err(rid: Value, msg: String) -> Response {
    err_resp(rid, msg)
}

fn err_resp(rid: Value, msg: String) -> Response {
    Response { id: rid, result: None, error: Some(msg) }
}

fn build_key_event(name: &str, modifiers: &[String]) -> Option<KeyEvent> {
    use hbb_common::message_proto::key_event::Union as KU;
    let mut evt = KeyEvent::new();
    evt.down = true;
    evt.press = true;
    evt.mode = KeyboardMode::Legacy.into();

    for m in modifiers {
        let mk = match m.as_str() {
            "Ctrl" | "Control" => ControlKey::Control,
            "Alt" => ControlKey::Alt,
            "Shift" => ControlKey::Shift,
            "Meta" | "Super" | "Win" | "Cmd" => ControlKey::Meta,
            _ => continue,
        };
        evt.modifiers.push(mk.into());
    }

    let ck = match name {
        "Return" | "Enter" => ControlKey::Return,
        "Escape" | "Esc" => ControlKey::Escape,
        "Tab" => ControlKey::Tab,
        "Backspace" => ControlKey::Backspace,
        "Delete" => ControlKey::Delete,
        "Space" => ControlKey::Space,
        "ArrowLeft" | "Left" => ControlKey::LeftArrow,
        "ArrowRight" | "Right" => ControlKey::RightArrow,
        "ArrowUp" | "Up" => ControlKey::UpArrow,
        "ArrowDown" | "Down" => ControlKey::DownArrow,
        "Home" => ControlKey::Home,
        "End" => ControlKey::End,
        "PageUp" => ControlKey::PageUp,
        "PageDown" => ControlKey::PageDown,
        "Insert" => ControlKey::Insert,
        "CapsLock" => ControlKey::CapsLock,
        "Meta" | "Super" | "Win" | "Cmd" => ControlKey::Meta,
        "Ctrl" | "Control" => ControlKey::Control,
        "Alt" => ControlKey::Alt,
        "Shift" => ControlKey::Shift,
        "F1" => ControlKey::F1,
        "F2" => ControlKey::F2,
        "F3" => ControlKey::F3,
        "F4" => ControlKey::F4,
        "F5" => ControlKey::F5,
        "F6" => ControlKey::F6,
        "F7" => ControlKey::F7,
        "F8" => ControlKey::F8,
        "F9" => ControlKey::F9,
        "F10" => ControlKey::F10,
        "F11" => ControlKey::F11,
        "F12" => ControlKey::F12,
        _ => {
            if name.chars().count() == 1 {
                evt.union = Some(KU::Seq(name.to_string()));
                return Some(evt);
            }
            return None;
        }
    };
    evt.union = Some(KU::ControlKey(ck.into()));
    Some(evt)
}

fn base64_encode(data: &[u8]) -> String {
    B64.encode(data)
}

async fn connect_rpc(
    params: Value,
    active: &Arc<RwLock<Option<Active>>>,
) -> Result<Value, String> {
    let peer_id = params["peer_id"].as_str().ok_or("missing peer_id")?.to_string();
    let password = params["password"].as_str().unwrap_or("").to_string();
    let key = params["key"].as_str().unwrap_or("").to_string();
    let rendezvous = params["rendezvous_server"]
        .as_str()
        .or_else(|| params["relay_server"].as_str())
        .unwrap_or("");

    // RustDesk encodes a per-connection rendezvous override in the peer_id
    // itself: `<id>@<server>?key=<base64>`. Build it if the caller supplied
    // overrides; otherwise the daemon falls back to its standard RustDesk
    // config (~/.config/rustdesk/RustDesk2.toml) or the public servers.
    let composite_id = if !rendezvous.is_empty() {
        let key_suffix = if key.is_empty() { String::new() } else { format!("?key={key}") };
        format!("{peer_id}@{rendezvous}{key_suffix}")
    } else {
        peer_id.clone()
    };

    let state = Arc::new(HeadlessState::default());
    let handler = HeadlessHandler::new_with(state.clone());
    let session = Session::<HeadlessHandler> {
        ui_handler: handler,
        password: password.clone(),
        ..Default::default()
    };
    session.lc.write().unwrap().initialize(
        composite_id,
        ConnType::DEFAULT_CONN,
        None,
        false,
        None,
        if password.is_empty() { None } else { Some(password) },
        None,
    );

    let session_clone = session.clone();
    std::thread::spawn(move || {
        io_loop(session_clone, 1);
    });

    // Wait up to 60s for a frame to arrive (true signal of a working session).
    // RustDesk does its own retry dance (direct → rendezvous → relay); transient
    // msgbox "error:" entries happen mid-flow and are NOT terminal, so we only
    // watch for either (a) the first rgba frame, or (b) an auth/credentials
    // failure that won't be retried.
    let (tx, rx) = oneshot::channel::<Result<(), String>>();
    let state_check = state.clone();
    tokio::spawn(async move {
        for _ in 0..600 {
            if state_check.rgba.lock().unwrap().is_some()
                || *state_check.connected.read().unwrap()
            {
                let _ = tx.send(Ok(()));
                return;
            }
            if let Some(msg) = state_check.last_msg.lock().unwrap().as_ref() {
                let ml = msg.to_ascii_lowercase();
                if ml.contains("wrong password")
                    || ml.contains("access denied")
                    || ml.contains("key mismatch")
                    || ml.contains("id does not exist")
                {
                    let _ = tx.send(Err(msg.clone()));
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let _ = tx.send(Err("timeout waiting for first frame".into()));
    });

    match rx.await {
        Ok(Ok(())) => {
            *active.write().unwrap() = Some(Active { session, state });
            Ok(json!({"peer_id": peer_id}))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("connection watcher dropped".into()),
    }
}

// ----- socket server -----

async fn handle_client(
    stream: UnixStream,
    active: Arc<RwLock<Option<Active>>>,
) -> ResultType<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = err_resp(Value::Null, format!("bad json: {e}"));
                let mut s = serde_json::to_string(&resp)?;
                s.push('\n');
                wr.write_all(s.as_bytes()).await?;
                continue;
            }
        };
        let resp = handle_request(req, &active).await;
        let mut s = serde_json::to_string(&resp)?;
        s.push('\n');
        wr.write_all(s.as_bytes()).await?;
    }
    Ok(())
}

fn default_socket_path() -> String {
    if let Ok(p) = std::env::var("RUSTDESK_SOCKET") {
        return p;
    }
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        let p = std::path::Path::new(&rt).join("rustdesk-headless.sock");
        return p.to_string_lossy().into_owned();
    }
    format!("/tmp/rustdesk-headless-{}.sock", unsafe { libc::geteuid() })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ResultType<()> {
    env_logger::init();
    let socket_path = default_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    log::info!("rustdesk-headless listening on {}", socket_path);

    let active: Arc<RwLock<Option<Active>>> = Arc::new(RwLock::new(None));

    loop {
        let (stream, _) = listener.accept().await?;
        let active = active.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, active).await {
                log::error!("client error: {e}");
            }
        });
    }
}
