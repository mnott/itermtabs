//! itermtabs — Rename iTerm2 tabs via the native WebSocket API.
//!
//! Connects to iTerm2's local WebSocket API (unix socket or TCP) and sets
//! the tab title override — the same as double-click renaming a tab.
//! Zero runtime dependencies beyond the binary itself.

use std::os::unix::net::UnixStream;
use std::process::Command;

use clap::{Parser, Subcommand};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::header::HeaderValue;
use tungstenite::{Message, WebSocket};

// ── Minimal protobuf encoder ──

fn encode_varint(mut v: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    loop {
        if v <= 0x7f {
            buf.push(v as u8);
            break;
        }
        buf.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    buf
}

fn encode_tag(field_number: u32, wire_type: u8) -> Vec<u8> {
    encode_varint(((field_number as u64) << 3) | wire_type as u64)
}

fn encode_string(field_number: u32, value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut buf = encode_tag(field_number, 2);
    buf.extend(encode_varint(bytes.len() as u64));
    buf.extend_from_slice(bytes);
    buf
}

fn encode_double(field_number: u32, value: f64) -> Vec<u8> {
    let mut buf = encode_tag(field_number, 1);
    buf.extend_from_slice(&value.to_le_bytes());
    buf
}

fn encode_length_delimited(field_number: u32, data: &[u8]) -> Vec<u8> {
    let mut buf = encode_tag(field_number, 2);
    buf.extend(encode_varint(data.len() as u64));
    buf.extend_from_slice(data);
    buf
}

// ── Minimal protobuf decoder ──

#[derive(Debug)]
#[allow(dead_code)]
struct DecodedField {
    fn_: u32,
    wt: u8,
    varint_value: u64,
    bytes_value: Vec<u8>,
    end: usize,
}

fn decode_varint_at(buf: &[u8], mut off: usize) -> (u64, usize) {
    let mut value: u64 = 0;
    let mut shift = 0;
    while off < buf.len() {
        let b = buf[off] as u64;
        off += 1;
        value |= (b & 0x7f) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            break;
        }
    }
    (value, off)
}

fn decode_field(buf: &[u8], off: usize) -> Option<DecodedField> {
    if off >= buf.len() {
        return None;
    }
    let (tag, o1) = decode_varint_at(buf, off);
    let fn_ = (tag >> 3) as u32;
    let wt = (tag & 0x7) as u8;
    match wt {
        0 => {
            let (value, o2) = decode_varint_at(buf, o1);
            Some(DecodedField { fn_, wt, varint_value: value, bytes_value: Vec::new(), end: o2 })
        }
        1 => {
            if o1 + 8 > buf.len() { return None; }
            Some(DecodedField { fn_, wt, varint_value: 0, bytes_value: buf[o1..o1+8].to_vec(), end: o1 + 8 })
        }
        2 => {
            let (len, o2) = decode_varint_at(buf, o1);
            let end = o2 + len as usize;
            if end > buf.len() { return None; }
            Some(DecodedField { fn_, wt, varint_value: 0, bytes_value: buf[o2..end].to_vec(), end })
        }
        5 => {
            if o1 + 4 > buf.len() { return None; }
            Some(DecodedField { fn_, wt, varint_value: 0, bytes_value: buf[o1..o1+4].to_vec(), end: o1 + 4 })
        }
        _ => None,
    }
}

fn parse_fields(buf: &[u8]) -> Vec<DecodedField> {
    let mut results = Vec::new();
    let mut off = 0;
    while off < buf.len() {
        match decode_field(buf, off) {
            Some(f) => { off = f.end; results.push(f); }
            None => break,
        }
    }
    results
}

// ── Message builders ──

fn build_list_sessions_request(id: u64) -> Vec<u8> {
    let mut buf = encode_tag(1, 0);
    buf.extend(encode_varint(id));
    buf.extend(encode_length_delimited(106, &[]));
    buf
}

fn build_get_variable_request(id: u64, tab_id: &str, var_name: &str) -> Vec<u8> {
    // variable_request = field 115; tab_id = field 4; get = field 3
    let mut inner = encode_string(4, tab_id);
    inner.extend(encode_string(3, var_name));
    let mut buf = encode_tag(1, 0);
    buf.extend(encode_varint(id));
    buf.extend(encode_length_delimited(115, &inner));
    buf
}

fn build_set_title_request(id: u64, tab_id: &str, title: &str) -> Vec<u8> {
    let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
    let invocation = format!("iterm2.set_title(title: \"{}\")", escaped);
    let method = encode_string(1, tab_id);
    let mut inner = encode_string(5, &invocation);
    inner.extend(encode_double(6, -1.0));
    inner.extend(encode_length_delimited(7, &method));
    let mut buf = encode_tag(1, 0);
    buf.extend(encode_varint(id));
    buf.extend(encode_length_delimited(132, &inner));
    buf
}

// ── Response parsing ──

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct SessionInfo {
    session_id: String,
    tab_id: String,
    name: String,
}

fn extract_sessions(buf: &[u8]) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    for f106 in parse_fields(buf) {
        if f106.fn_ != 106 || f106.wt != 2 { continue; }
        for f_win in parse_fields(&f106.bytes_value) {
            if f_win.fn_ != 1 || f_win.wt != 2 { continue; }
            for f_tab in parse_fields(&f_win.bytes_value) {
                if f_tab.fn_ != 1 || f_tab.wt != 2 { continue; }
                let tab_fields = parse_fields(&f_tab.bytes_value);
                let mut tab_id = String::new();
                for tf in &tab_fields {
                    if tf.fn_ == 2 && tf.wt == 2 {
                        let s = String::from_utf8_lossy(&tf.bytes_value);
                        if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
                            tab_id = s.to_string();
                        }
                    }
                }
                // Dig into root (f3) > split tree (f2) > session (f1)
                for tf in &tab_fields {
                    if tf.fn_ != 3 || tf.wt != 2 { continue; }
                    for root_f in parse_fields(&tf.bytes_value) {
                        if root_f.fn_ != 2 || root_f.wt != 2 { continue; }
                        for sess_f in parse_fields(&root_f.bytes_value) {
                            if sess_f.fn_ != 1 || sess_f.wt != 2 { continue; }
                            let sess_fields = parse_fields(&sess_f.bytes_value);
                            let mut uuid = String::new();
                            let mut name = String::new();
                            for sf in &sess_fields {
                                if sf.fn_ == 1 && sf.wt == 2 {
                                    uuid = String::from_utf8_lossy(&sf.bytes_value).to_string();
                                }
                                if sf.fn_ == 4 && sf.wt == 2 {
                                    let raw = String::from_utf8_lossy(&sf.bytes_value);
                                    // Extract readable portion (skip binary prefix)
                                    name = raw.chars()
                                        .skip_while(|c| !c.is_ascii_graphic() || *c < ' ')
                                        .collect::<String>()
                                        .trim()
                                        .to_string();
                                }
                            }
                            if !uuid.is_empty() {
                                sessions.push(SessionInfo {
                                    session_id: uuid,
                                    tab_id: tab_id.clone(),
                                    name,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    sessions
}

fn find_tab_id_for_session(buf: &[u8], target: &str) -> Option<String> {
    for f106 in parse_fields(buf) {
        if f106.fn_ != 106 || f106.wt != 2 { continue; }
        for f_win in parse_fields(&f106.bytes_value) {
            if f_win.fn_ != 1 || f_win.wt != 2 { continue; }
            for f_tab in parse_fields(&f_win.bytes_value) {
                if f_tab.fn_ != 1 || f_tab.wt != 2 { continue; }
                let tab_fields = parse_fields(&f_tab.bytes_value);
                let mut tab_id = None;
                for tf in &tab_fields {
                    if tf.fn_ == 2 && tf.wt == 2 {
                        let s = String::from_utf8_lossy(&tf.bytes_value);
                        if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
                            tab_id = Some(s.to_string());
                        }
                    }
                }
                if let Some(ref id) = tab_id {
                    let raw = String::from_utf8_lossy(&f_tab.bytes_value);
                    if raw.contains(target) {
                        return Some(id.clone());
                    }
                }
            }
        }
    }
    None
}

fn parse_variable_response(buf: &[u8]) -> Option<String> {
    for f in parse_fields(buf) {
        if f.fn_ == 115 && f.wt == 2 {
            for vf in parse_fields(&f.bytes_value) {
                if vf.fn_ == 2 && vf.wt == 2 {
                    let val = String::from_utf8_lossy(&vf.bytes_value).to_string();
                    // Values are JSON-encoded strings — strip quotes and unescape
                    if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                        let inner = &val[1..val.len()-1];
                        return Some(inner.replace("\\/", "/").replace("\\\"", "\"").replace("\\\\", "\\"));
                    }
                    return Some(val);
                }
            }
        }
    }
    None
}

#[allow(dead_code)]
fn extract_response_id(buf: &[u8]) -> Option<u64> {
    let mut off = 0;
    while off < buf.len() {
        if let Some(f) = decode_field(buf, off) {
            if f.fn_ == 1 && f.wt == 0 {
                return Some(f.varint_value);
            }
            off = f.end;
        } else {
            break;
        }
    }
    None
}

// ── Auth ──

fn get_cookie_and_key() -> Option<(String, String)> {
    let output = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"iTerm2\" to request cookie and key for app named \"itermtabs\"")
        .output()
        .ok()?;
    if !output.status.success() { return None; }
    let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts: Vec<&str> = result.split(' ').collect();
    if parts.len() >= 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

// ── Connection ──

fn connect_iterm2(cookie: &str, key: &str) -> Result<WebSocket<UnixStream>, String> {
    let socket_path = format!(
        "{}/Library/Application Support/iTerm2/private/socket",
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
    );

    let mut request = "ws://localhost/"
        .into_client_request()
        .map_err(|e| format!("Request error: {}", e))?;

    let headers = request.headers_mut();
    headers.insert("origin", HeaderValue::from_static("ws://localhost/"));
    headers.insert("x-iterm2-library-version", HeaderValue::from_static("rust 1.0"));
    headers.insert("x-iterm2-disable-auth-ui", HeaderValue::from_static("true"));
    headers.insert("x-iterm2-cookie", HeaderValue::from_str(cookie).map_err(|e| e.to_string())?);
    headers.insert("x-iterm2-key", HeaderValue::from_str(key).map_err(|e| e.to_string())?);
    headers.insert("x-iterm2-advisory-name", HeaderValue::from_static("itermtabs"));
    headers.insert("sec-websocket-protocol", HeaderValue::from_static("api.iterm2.com"));

    let stream = UnixStream::connect(&socket_path)
        .map_err(|e| format!("Cannot connect to iTerm2 socket at {}: {}", socket_path, e))?;

    let (ws, _response) = tungstenite::client(request, stream)
        .map_err(|e| format!("WebSocket handshake failed: {}", e))?;

    Ok(ws)
}

fn send_and_recv(ws: &mut WebSocket<UnixStream>, data: &[u8]) -> Result<Vec<u8>, String> {
    ws.send(Message::Binary(data.to_vec().into()))
        .map_err(|e| format!("Send error: {}", e))?;
    loop {
        match ws.read() {
            Ok(Message::Binary(data)) => return Ok(data.to_vec()),
            Ok(_) => continue,
            Err(e) => return Err(format!("Read error: {}", e)),
        }
    }
}

// ── Commands ──

fn cmd_list() -> Result<(), String> {
    let (cookie, key) = get_cookie_and_key()
        .ok_or("Failed to get iTerm2 auth cookie. Is iTerm2 running?")?;
    let mut ws = connect_iterm2(&cookie, &key)?;

    // Phase 1: List sessions
    let mut msg_id: u64 = 0;
    msg_id += 1;
    let resp = send_and_recv(&mut ws, &build_list_sessions_request(msg_id))?;
    let sessions = extract_sessions(&resp);

    if sessions.is_empty() {
        println!("No tabs found.");
        return Ok(());
    }

    // Phase 2: Get tab titles
    let mut unique_tabs: Vec<String> = Vec::new();
    for s in &sessions {
        if !unique_tabs.contains(&s.tab_id) {
            unique_tabs.push(s.tab_id.clone());
        }
    }

    let mut tab_titles: Vec<(String, Option<String>)> = Vec::new();
    for tab_id in &unique_tabs {
        msg_id += 1;
        let resp = send_and_recv(&mut ws, &build_get_variable_request(msg_id, tab_id, "title"))?;
        let title = parse_variable_response(&resp);
        tab_titles.push((tab_id.clone(), title));
    }

    let _ = ws.close(None);

    // Display
    println!("iTerm2 Tabs:\n");
    for (tab_id, title) in &tab_titles {
        let display_title = title.as_deref().unwrap_or_else(|| {
            sessions.iter()
                .find(|s| &s.tab_id == tab_id)
                .map(|s| s.name.as_str())
                .unwrap_or("")
        });
        println!("  {:>3}  {}", tab_id, display_title);
    }

    Ok(())
}

fn resolve_target(
    target: &str,
    sessions: &[SessionInfo],
    list_buf: &[u8],
    tab_titles: &[(String, String)],
) -> Result<String, String> {
    // 1. Tab ID (numeric)
    if target.chars().all(|c| c.is_ascii_digit()) {
        if let Some(s) = sessions.iter().find(|s| s.tab_id == target) {
            return Ok(s.tab_id.clone());
        }
    }
    // 2. Session UUID
    if let Some(tab_id) = find_tab_id_for_session(list_buf, target) {
        return Ok(tab_id);
    }
    // 3. Tab title substring (case-insensitive)
    let lower_target = target.to_lowercase();
    let matches: Vec<&(String, String)> = tab_titles.iter()
        .filter(|(_, title)| title.to_lowercase().contains(&lower_target))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].0.clone()),
        0 => Err(format!("No tab matching \"{}\"", target)),
        n => {
            let names: Vec<String> = matches.iter().map(|(_, t)| format!("\"{}\"", t)).collect();
            Err(format!("Ambiguous: \"{}\" matches {} tabs: {}", target, n, names.join(", ")))
        }
    }
}

fn cmd_rename(target: &str, new_title: &str) -> Result<(), String> {
    let (cookie, key) = get_cookie_and_key()
        .ok_or("Failed to get iTerm2 auth cookie. Is iTerm2 running?")?;
    let mut ws = connect_iterm2(&cookie, &key)?;

    let mut msg_id: u64 = 0;

    // Phase 1: List sessions
    msg_id += 1;
    let list_resp = send_and_recv(&mut ws, &build_list_sessions_request(msg_id))?;
    let sessions = extract_sessions(&list_resp);

    // Quick path: if target is numeric tab ID, skip title resolution
    if target.chars().all(|c| c.is_ascii_digit()) {
        if sessions.iter().any(|s| s.tab_id == target) {
            msg_id += 1;
            send_and_recv(&mut ws, &build_set_title_request(msg_id, target, new_title))?;
            let _ = ws.close(None);
            println!("Tab title set to \"{}\"", new_title);
            return Ok(());
        }
    }

    // Quick path: if target looks like a UUID, try direct match
    if target.contains('-') && target.len() > 30 {
        if let Some(tab_id) = find_tab_id_for_session(&list_resp, target) {
            msg_id += 1;
            send_and_recv(&mut ws, &build_set_title_request(msg_id, &tab_id, new_title))?;
            let _ = ws.close(None);
            println!("Tab title set to \"{}\"", new_title);
            return Ok(());
        }
    }

    // Phase 2: Get tab titles for name resolution
    let mut unique_tabs: Vec<String> = Vec::new();
    for s in &sessions {
        if !unique_tabs.contains(&s.tab_id) {
            unique_tabs.push(s.tab_id.clone());
        }
    }

    let mut tab_titles: Vec<(String, String)> = Vec::new();
    for tab_id in &unique_tabs {
        msg_id += 1;
        let resp = send_and_recv(&mut ws, &build_get_variable_request(msg_id, tab_id, "title"))?;
        let title = parse_variable_response(&resp).unwrap_or_default();
        tab_titles.push((tab_id.clone(), title));
    }

    let tab_id = resolve_target(target, &sessions, &list_resp, &tab_titles)?;

    msg_id += 1;
    send_and_recv(&mut ws, &build_set_title_request(msg_id, &tab_id, new_title))?;
    let _ = ws.close(None);
    println!("Tab title set to \"{}\"", new_title);

    Ok(())
}

// ── CLI ──

#[derive(Parser)]
#[command(name = "itermtabs", about = "Rename iTerm2 tabs via the native WebSocket API")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all iTerm2 tabs with their IDs and names
    List,
    /// Rename a tab by ID, name substring, or session UUID
    Rename {
        /// Tab ID, tab name substring, or session UUID
        target: String,
        /// The new tab title
        title: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::List => cmd_list(),
        Commands::Rename { target, title } => cmd_rename(&target, &title),
    };
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
