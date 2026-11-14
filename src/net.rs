//! The link to a Mac running Kumu.
//!
//! One reliable iroh stream carries control JSON on channel zero and terminal
//! bytes on a per-pane channel. The phone runs its own VT; the Mac only sends
//! the same PTY stream the desktop already reads.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use nsh_core::prelude::*;
use nsh_core::storage::Storage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const ALPN: &[u8] = b"kumu/remote/1";
const PROTOCOL: u64 = 1;
const LINK_PREFIX: &str = "kumu://pair/";
const CONTROL: u32 = 0;
const KIND_DATA: u8 = 0;
const KIND_CREDIT: u8 = 1;
const KIND_CLOSE: u8 = 2;
const KIND_RESIZE: u8 = 3;
const KIND_REPLAY: u8 = 4;
const MAX_CONTROL: usize = 256 * 1024;
const MAX_BINARY: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connecting,
    Pairing,
    Connected,
    Closed,
    Failed(String),
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connecting => "正在连接",
            Self::Pairing => "正在配对",
            Self::Connected => "已连接",
            Self::Closed => "已断开",
            Self::Failed(_) => "连接失败",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PairingLink {
    version: u64,
    address: iroh::EndpointAddr,
    #[serde(default)]
    discovery: bool,
    #[serde(default)]
    code: Option<String>,
}

impl PairingLink {
    fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let link: Self = if let Some(encoded) = text.strip_prefix(LINK_PREFIX) {
            serde_json::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(encoded)
                    .context("invalid pairing link")?,
            )?
        } else {
            serde_json::from_str(text).context("paste the kumu://pair/ link")?
        };
        if link.version != PROTOCOL || link.address.is_empty() {
            bail!("unsupported or incomplete pairing link");
        }
        Ok(link)
    }
}

enum Outgoing {
    Control(String),
    Data(Vec<u8>),
    Credit(u32),
    Resize { cols: u16, rows: u16 },
    Close,
}

/// One row in the pane drawer.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneSummary {
    pub id: u64,
    pub project: String,
    pub title: String,
    pub status: String,
    pub unread: bool,
}

/// What the terminal view consumes from the network.
#[derive(Clone, Debug)]
pub enum Feed {
    /// Drop the old pane's screen before the new attach's replay.
    Reset,
    Data(Vec<u8>),
}

/// A handle the terminal UI sends input through.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Outgoing>,
    /// The first laid-out grid. The attach waits for it so the replay is
    /// produced at the phone's real width instead of a guessed 80 columns.
    attach_size: mpsc::UnboundedSender<(u16, u16)>,
    switch: mpsc::UnboundedSender<u64>,
    size: Arc<Mutex<(u16, u16)>>,
    current: Arc<AtomicU64>,
}

impl Session {
    pub fn input(&self, data: impl Into<String>) {
        let bytes = data.into().into_bytes();
        if !bytes.is_empty() {
            let _ = self.tx.send(Outgoing::Data(bytes));
        }
    }

    pub fn credit(&self, bytes: usize) {
        let _ = self.tx.send(Outgoing::Credit(bytes as u32));
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        let cols = cols.clamp(1, u16::MAX as u32) as u16;
        let rows = rows.clamp(1, u16::MAX as u32) as u16;
        if let Ok(mut size) = self.size.lock() {
            *size = (cols, rows);
        }
        let _ = self.attach_size.send((cols, rows));
        let _ = self.tx.send(Outgoing::Resize { cols, rows });
    }

    pub fn switch(&self, pane: u64) {
        if pane != 0 && pane != self.current.load(Ordering::Acquire) {
            let _ = self.switch.send(pane);
        }
    }

    pub fn current_pane(&self) -> u64 {
        self.current.load(Ordering::Acquire)
    }

    pub fn show_on_desktop(&self, pane: u64) {
        self.action(json!({"type": "showOnDesktop", "pane": pane}));
    }

    pub fn new_terminal(&self) {
        self.action(json!({"type": "newTerminal"}));
    }

    fn action(&self, action: Value) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "workspace.action",
            "params": action
        });
        let _ = self.tx.send(Outgoing::Control(request.to_string()));
    }

    pub fn close(&self) {
        let _ = self.tx.send(Outgoing::Close);
    }
}

/// Connect to a Mac, pair if needed, attach the most recent pane, and stream
/// terminal bytes into `output`.
pub fn connect(
    link: String,
    code: String,
    status: Signal<Status>,
    title: Signal<String>,
    panes: Signal<Vec<PaneSummary>>,
    output: mpsc::UnboundedSender<Feed>,
) -> Session {
    let (tx, rx) = mpsc::unbounded_channel();
    let (attach_size, attach_rx) = mpsc::unbounded_channel();
    let (switch, switch_rx) = mpsc::unbounded_channel();
    let channel = Arc::new(AtomicU32::new(0));
    let current = Arc::new(AtomicU64::new(0));
    let size = Arc::new(Mutex::new((80, 24)));
    let session = Session {
        tx: tx.clone(),
        attach_size,
        switch,
        size: size.clone(),
        current: current.clone(),
    };
    spawn(async move {
        if let Err(error) = run(
            link, code, tx, status, title, panes, output, rx, attach_rx, switch_rx, channel,
            current, size,
        )
        .await
        {
            eprintln!("[kumu] remote connection failed: {error:#}");
            status.set(Status::Failed(error.to_string()));
        }
    });
    session
}

#[allow(clippy::too_many_arguments)]
async fn run(
    link_text: String,
    code_text: String,
    tx: mpsc::UnboundedSender<Outgoing>,
    status: Signal<Status>,
    title: Signal<String>,
    panes: Signal<Vec<PaneSummary>>,
    output: mpsc::UnboundedSender<Feed>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    mut attach_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    mut switch_rx: mpsc::UnboundedReceiver<u64>,
    channel: Arc<AtomicU32>,
    current: Arc<AtomicU64>,
    size: Arc<Mutex<(u16, u16)>>,
) -> Result<()> {
    status.set(Status::Connecting);
    let link = PairingLink::parse(&link_text)?;
    let secret = client_identity();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("cannot start the network endpoint")?;
    let connection = endpoint
        .connect(link.address.clone(), ALPN)
        .await
        .context("cannot reach the Mac")?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("cannot open the stream")?;

    status.set(Status::Pairing);
    let wire_tx = tx.clone();
    let (msg_tx, mut msg_rx) = mpsc::channel::<Result<Message>>(64);
    let reader = tokio::spawn(async move {
        loop {
            match read_message(&mut recv).await {
                Ok(Some(message)) => {
                    if msg_tx.send(Ok(message)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = msg_tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    let writer_channel = channel.clone();
    let writer = tokio::spawn(async move {
        while let Some(outgoing) = rx.recv().await {
            let sent = match outgoing {
                Outgoing::Close => break,
                Outgoing::Control(text) => write_message(&mut send, &Message::Text(text)).await,
                Outgoing::Data(bytes) => {
                    let channel = writer_channel.load(Ordering::Acquire);
                    if channel == 0 {
                        continue;
                    }
                    write_message(
                        &mut send,
                        &Message::Binary(frame(channel, KIND_DATA, &bytes)),
                    )
                    .await
                }
                Outgoing::Credit(bytes) => {
                    let channel = writer_channel.load(Ordering::Acquire);
                    if channel == 0 {
                        continue;
                    }
                    write_message(
                        &mut send,
                        &Message::Binary(frame(channel, KIND_CREDIT, &bytes.to_le_bytes())),
                    )
                    .await
                }
                Outgoing::Resize { cols, rows } => {
                    let channel = writer_channel.load(Ordering::Acquire);
                    if channel == 0 {
                        continue;
                    }
                    let mut payload = Vec::with_capacity(4);
                    payload.extend_from_slice(&cols.to_le_bytes());
                    payload.extend_from_slice(&rows.to_le_bytes());
                    write_message(
                        &mut send,
                        &Message::Binary(frame(channel, KIND_RESIZE, &payload)),
                    )
                    .await
                }
            };
            if sent.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });

    // The Mac releases a disconnected phone's size lease after a quiet
    // period. A killed or suspended app sends nothing, so keep the session
    // visibly alive while it is in the foreground.
    let ping_tx = tx.clone();
    let pinger = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let ping = json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}).to_string();
            if ping_tx.send(Outgoing::Control(ping)).is_err() {
                break;
            }
        }
    });

    let result = authenticate_and_attach(
        &wire_tx,
        &mut msg_rx,
        &link,
        &code_text,
        &status,
        &title,
        &panes,
        &channel,
        &current,
        &mut attach_rx,
    )
    .await;

    if result.is_err() {
        reader.abort();
        writer.abort();
        pinger.abort();
        endpoint.close().await;
        return result.map(|_| ());
    }

    status.set(Status::Connected);
    enum Next {
        Message(Option<Result<Message>>),
        Switch(Option<u64>),
        Timeout,
    }
    loop {
        let next = tokio::select! {
            message = msg_rx.recv() => Next::Message(message),
            pane = switch_rx.recv() => Next::Switch(pane),
            _ = tokio::time::sleep(Duration::from_secs(30)) => Next::Timeout,
        };
        match next {
            Next::Switch(Some(pane)) => {
                switch_pane(
                    &wire_tx,
                    &mut msg_rx,
                    pane,
                    &channel,
                    &current,
                    &size,
                    &output,
                    &title,
                    &panes,
                )
                .await?;
            }
            Next::Switch(None) => break,
            Next::Timeout => {
                status.set(Status::Failed("连接超时".into()));
                break;
            }
            Next::Message(message) => {
                let Some(message) = message else { break };
                match message? {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        if let Some(error) = value.get("error") {
                            let kind = error["data"]["kind"].as_str().unwrap_or("unknown");
                            let message = error["message"].as_str().unwrap_or("Mac 返回错误");
                            status.set(Status::Failed(format!("{kind}: {message}")));
                            break;
                        }
                        if let Some(next) = value["result"]["title"].as_str() {
                            title.set(next.to_owned());
                        }
                    }
                    Message::Binary(bytes) => {
                        let (kind, channel_id, payload) = split_frame(&bytes)?;
                        if channel_id != channel.load(Ordering::Acquire) {
                            continue;
                        }
                        match kind {
                            KIND_REPLAY | KIND_DATA => {
                                if output.send(Feed::Data(payload.to_vec())).is_err() {
                                    break;
                                }
                            }
                            KIND_CLOSE => {
                                status.set(Status::Closed);
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    status.set(Status::Closed);
    reader.abort();
    writer.abort();
    pinger.abort();
    let _ = wire_tx.send(Outgoing::Control(
        json!({"jsonrpc":"2.0","id":999,"method":"ping"}).to_string(),
    ));
    endpoint.close().await;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn authenticate_and_attach(
    wire_tx: &mpsc::UnboundedSender<Outgoing>,
    msg_rx: &mut mpsc::Receiver<Result<Message>>,
    link: &PairingLink,
    code_text: &str,
    status: &Signal<Status>,
    title: &Signal<String>,
    panes: &Signal<Vec<PaneSummary>>,
    channel: &Arc<AtomicU32>,
    current: &Arc<AtomicU64>,
    attach_rx: &mut mpsc::UnboundedReceiver<(u16, u16)>,
) -> Result<()> {
    let mut token = Storage::get_item("token").unwrap_or_default();
    let hello_params = |token: &str| {
        if token.is_empty() {
            json!({"protocol": PROTOCOL, "deviceName": "iPhone"})
        } else {
            json!({"protocol": PROTOCOL, "deviceName": "iPhone", "token": token})
        }
    };
    let hello = match request(
        wire_tx,
        msg_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "hello",
            "params": hello_params(&token)
        }),
    )
    .await
    {
        Ok(value) => value,
        Err(error) if error.to_string().starts_with("unauthorized") => {
            // A token from an older install or another Mac is not an error
            // worth surfacing before the user has had a chance to pair.
            token.clear();
            let _ = Storage::set_item("token", "");
            request(
                wire_tx,
                msg_rx,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "hello",
                    "params": hello_params("")
                }),
            )
            .await?
        }
        Err(error) => return Err(error),
    };
    if hello["result"]["needsPairing"].as_bool() == Some(true) {
        let code = link
            .code
            .clone()
            .filter(|code| !code.is_empty())
            .unwrap_or_else(|| code_text.trim().to_owned());
        if code.is_empty() {
            bail!("this Mac needs a pairing code");
        }
        status.set(Status::Pairing);
        let paired = request(
            wire_tx,
            msg_rx,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "pair",
                "params": {"code": code, "name": device_name()}
            }),
        )
        .await?;
        let token = paired["result"]["token"]
            .as_str()
            .context("pairing returned no token")?
            .to_owned();
        let _ = Storage::set_item("token", token);
    } else if hello.get("error").is_some() {
        bail!("this phone is not paired with the Mac any more");
    }

    let state = request(
        wire_tx,
        msg_rx,
        json!({"jsonrpc": "2.0", "id": 3, "method": "workspace.state"}),
    )
    .await?;
    let project_names: HashMap<i64, String> = state["result"]["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|project| {
            Some((
                project["id"].as_i64()?,
                project["name"].as_str().unwrap_or_default().to_owned(),
            ))
        })
        .collect();
    let pane_values = state["result"]["panes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let summaries = pane_values
        .iter()
        .filter_map(|pane| {
            Some(PaneSummary {
                id: pane["id"].as_u64()?,
                project: project_names
                    .get(&pane["project"].as_i64().unwrap_or_default())
                    .cloned()
                    .unwrap_or_default(),
                title: pane["title"].as_str().unwrap_or("终端").to_owned(),
                status: pane["status"].as_str().unwrap_or("unknown").to_owned(),
                unread: pane["unread"].as_bool().unwrap_or(false),
            })
        })
        .collect::<Vec<_>>();
    let pane = state["result"]["recentPane"]
        .as_u64()
        .or_else(|| state["result"]["focusedPane"].as_u64())
        .or_else(|| pane_values.first().and_then(|pane| pane["id"].as_u64()))
        .context("the Mac has no pane to show")?;
    let project_name = summaries
        .iter()
        .find(|summary| summary.id == pane)
        .map(|summary| summary.project.clone())
        .unwrap_or_default();
    panes.set(summaries);
    // Wait for the terminal view's first layout. Attaching at a guessed size
    // would make the replay wrap on the phone and never match the PTY.
    let (cols, rows) = tokio::time::timeout(Duration::from_secs(5), attach_rx.recv())
        .await
        .ok()
        .flatten()
        .unwrap_or((80, 24));
    attach_rx.close();
    let attach = request(
        wire_tx,
        msg_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "pane.attach",
            "params": {"pane": pane, "cols": cols, "rows": rows}
        }),
    )
    .await?;
    let channel_id = attach["result"]["channel"]
        .as_u64()
        .context("attach returned no channel")? as u32;
    channel.store(channel_id, Ordering::Release);
    current.store(pane, Ordering::Release);
    if let Some(next) = attach["result"]["title"].as_str() {
        title.set(if project_name.is_empty() {
            next.to_owned()
        } else {
            format!("{project_name} › {next}")
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn switch_pane(
    wire_tx: &mpsc::UnboundedSender<Outgoing>,
    msg_rx: &mut mpsc::Receiver<Result<Message>>,
    pane: u64,
    channel: &Arc<AtomicU32>,
    current: &Arc<AtomicU64>,
    size: &Arc<Mutex<(u16, u16)>>,
    output: &mpsc::UnboundedSender<Feed>,
    title: &Signal<String>,
    panes: &Signal<Vec<PaneSummary>>,
) -> Result<()> {
    let old = current.load(Ordering::Acquire);
    if old != 0 {
        let _ = request(
            wire_tx,
            msg_rx,
            json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "pane.detach",
                "params": {"channel": old}
            }),
        )
        .await;
    }
    current.store(0, Ordering::Release);
    let (cols, rows) = *size.lock().expect("terminal size poisoned");
    let attach = request(
        wire_tx,
        msg_rx,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "pane.attach",
            "params": {"pane": pane, "cols": cols, "rows": rows}
        }),
    )
    .await?;
    let next_channel = attach["result"]["channel"]
        .as_u64()
        .context("attach returned no channel")? as u32;
    if let Some(next) = attach["result"]["title"].as_str() {
        let project = panes
            .get_untracked()
            .into_iter()
            .find(|summary| summary.id == pane)
            .map(|summary| summary.project)
            .unwrap_or_default();
        title.set(if project.is_empty() {
            next.to_owned()
        } else {
            format!("{project} › {next}")
        });
    }
    channel.store(next_channel, Ordering::Release);
    current.store(pane, Ordering::Release);
    output
        .send(Feed::Reset)
        .map_err(|_| anyhow::anyhow!("terminal view closed"))?;
    Ok(())
}

async fn request(
    wire_tx: &mpsc::UnboundedSender<Outgoing>,
    msg_rx: &mut mpsc::Receiver<Result<Message>>,
    request: Value,
) -> Result<Value> {
    wire_tx
        .send(Outgoing::Control(request.to_string()))
        .map_err(|_| anyhow::anyhow!("connection closed"))?;
    loop {
        let message = tokio::time::timeout(Duration::from_secs(15), msg_rx.recv())
            .await
            .context("the Mac did not answer")?
            .context("connection closed")??;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)?;
        if value.get("error").is_some() {
            let kind = value["error"]["data"]["kind"].as_str().unwrap_or("unknown");
            let message = value["error"]["message"].as_str().unwrap_or("Mac 返回错误");
            bail!("{kind}: {message}");
        }
        if value.get("result").is_some() {
            return Ok(value);
        }
    }
}

fn split_frame(bytes: &[u8]) -> Result<(u8, u32, &[u8])> {
    if bytes.len() < 9 {
        bail!("short terminal frame");
    }
    let channel = u32::from_le_bytes(bytes[..4].try_into().expect("four bytes"));
    let kind = bytes[4];
    let length = u32::from_le_bytes(bytes[5..9].try_into().expect("four bytes")) as usize;
    if bytes.len() != 9 + length {
        bail!("invalid terminal frame length");
    }
    Ok((kind, channel, &bytes[9..]))
}

fn frame(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(9 + payload.len());
    bytes.extend_from_slice(&channel.to_le_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

enum Message {
    Text(String),
    Binary(Vec<u8>),
}

async fn read_message<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Message>> {
    let mut header = [0u8; 9];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let channel = u32::from_le_bytes(header[..4].try_into().expect("four bytes"));
    let length = u32::from_le_bytes(header[5..].try_into().expect("four bytes")) as usize;
    let limit = if channel == CONTROL {
        MAX_CONTROL
    } else {
        MAX_BINARY.saturating_sub(9)
    };
    if length > limit || (channel == CONTROL && header[4] != 0) {
        bail!("invalid frame");
    }
    let mut bytes = vec![0; 9 + length];
    bytes[..9].copy_from_slice(&header);
    reader.read_exact(&mut bytes[9..]).await?;
    if channel == CONTROL {
        Ok(Some(Message::Text(String::from_utf8(bytes.split_off(9))?)))
    } else {
        Ok(Some(Message::Binary(bytes)))
    }
}

async fn write_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> Result<()> {
    match message {
        Message::Text(text) => {
            writer
                .write_all(&frame(CONTROL, 0, text.as_bytes()))
                .await?;
        }
        Message::Binary(bytes) => {
            writer.write_all(bytes).await?;
        }
    }
    writer.flush().await?;
    Ok(())
}

fn client_identity() -> SecretKey {
    if let Some(saved) = Storage::get_item("identity")
        && let Ok(bytes) = URL_SAFE_NO_PAD.decode(saved)
        && bytes.len() == 32
    {
        return SecretKey::from_bytes(bytes.as_slice().try_into().expect("32 bytes"));
    }
    let key = SecretKey::generate();
    let _ = Storage::set_item("identity", URL_SAFE_NO_PAD.encode(key.to_bytes()));
    key
}

fn device_name() -> String {
    std::env::var("KUMU_DEVICE_NAME").unwrap_or_else(|_| "iPhone".into())
}

/// Turns a byte stream into strings without splitting a UTF-8 character
/// across two writes, which a PTY can do at any byte boundary.
#[derive(Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.pending.extend_from_slice(bytes);
        match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_owned();
                self.pending.clear();
                Some(text)
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                if valid == 0 {
                    return None;
                }
                let text = std::str::from_utf8(&self.pending[..valid])
                    .expect("the prefix before an incomplete character is valid")
                    .to_owned();
                self.pending.drain(..valid);
                Some(text)
            }
            Err(_) => {
                let text = String::from_utf8_lossy(&self.pending).into_owned();
                self.pending.clear();
                Some(text)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Utf8Stream;

    #[test]
    fn a_character_split_across_chunks_is_written_once() {
        let mut stream = Utf8Stream::default();
        let bytes = "你好".as_bytes();
        assert_eq!(stream.push(&bytes[..2]), None);
        assert_eq!(stream.push(&bytes[2..]), Some("你好".to_owned()));
    }
}
