//! The link to a Mac running Kumu.
//!
//! One reliable iroh stream carries control JSON on channel zero and terminal
//! bytes on a per-pane channel. The phone runs its own VT; the Mac only sends
//! the same PTY stream the desktop already reads.

use std::collections::{HashMap, HashSet, VecDeque};
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
/// One request's whole wait for its answer: terminal output and pings the
/// request does not own must not extend it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// The project's id: two projects can share a name.
    pub project_id: i64,
    pub project: String,
    pub title: String,
    pub status: String,
    pub unread: bool,
}

/// An agent the Mac can start, already filtered to installed ones.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
}

/// What the terminal view consumes from the network.
#[derive(Clone, Debug)]
pub enum Feed {
    /// Drop the old pane's screen before the new attach's replay.
    Reset,
    Data(Vec<u8>),
    /// A one-off message for the UI, such as a failed action.
    Notice(String),
}

/// A workspace action for the Mac, and whether it changes the pane list, so
/// the loop asks for the state again once the Mac has answered.
struct Action {
    json: Value,
    refresh: bool,
}

/// A handle the terminal UI sends input through.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Outgoing>,
    /// The first laid-out grid. The attach waits for it so the replay is
    /// produced at the phone's real width instead of a guessed 80 columns.
    attach_size: mpsc::UnboundedSender<(u16, u16)>,
    switch: mpsc::UnboundedSender<u64>,
    /// Actions run in the connection loop, so their reply is read before the
    /// pane-list refresh that follows it.
    action: mpsc::UnboundedSender<Action>,
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
        self.send(json!({"type": "showOnDesktop", "pane": pane}), false);
    }

    pub fn new_terminal(&self) {
        self.send(json!({"type": "newTerminal"}), true);
    }

    pub fn close_pane(&self, pane: u64) {
        self.send(json!({"type": "closePane", "pane": pane}), true);
    }

    pub fn new_agent(&self, agent: &str) {
        self.send(json!({"type": "newAgent", "agent": agent}), true);
    }

    /// Queues a `workspace.action`; the loop stamps its request id when it
    /// sends it.
    fn send(&self, action: Value, refresh: bool) {
        let _ = self.action.send(Action {
            json: json!({
                "jsonrpc": "2.0",
                "method": "workspace.action",
                "params": action
            }),
            refresh,
        });
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
    agents: Signal<Vec<AgentSummary>>,
    output: mpsc::UnboundedSender<Feed>,
) -> Session {
    let (tx, rx) = mpsc::unbounded_channel();
    let (attach_size, attach_rx) = mpsc::unbounded_channel();
    let (switch, switch_rx) = mpsc::unbounded_channel();
    let (action, action_rx) = mpsc::unbounded_channel();
    let channel = Arc::new(AtomicU32::new(0));
    let current = Arc::new(AtomicU64::new(0));
    let size = Arc::new(Mutex::new((80, 24)));
    let session = Session {
        tx: tx.clone(),
        attach_size,
        switch,
        action,
        size: size.clone(),
        current: current.clone(),
    };
    spawn(async move {
        if let Err(error) = run(
            link, code, tx, status, title, panes, agents, output, rx, attach_rx, switch_rx,
            action_rx, channel, current, size,
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
    agents: Signal<Vec<AgentSummary>>,
    output: mpsc::UnboundedSender<Feed>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    mut attach_rx: mpsc::UnboundedReceiver<(u16, u16)>,
    mut switch_rx: mpsc::UnboundedReceiver<u64>,
    mut action_rx: mpsc::UnboundedReceiver<Action>,
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

    let mut wire = Wire {
        tx: &wire_tx,
        rx: &mut msg_rx,
        pending: VecDeque::new(),
        next_id: 1,
        abandoned: HashSet::new(),
    };
    let result = authenticate_and_attach(
        &mut wire,
        &link,
        &code_text,
        &status,
        &title,
        &panes,
        &agents,
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
        Action(Option<Action>),
        Timeout,
    }
    loop {
        // Messages a request read but did not own come back here before new
        // ones, so a switch or a refresh cannot swallow terminal output, a
        // ping, or a reply the request itself was not waiting for.
        let next = if let Some(message) = wire.pending.pop_front() {
            Next::Message(Some(message))
        } else {
            tokio::select! {
                message = wire.rx.recv() => Next::Message(message),
                pane = switch_rx.recv() => Next::Switch(pane),
                action = action_rx.recv() => Next::Action(action),
                _ = tokio::time::sleep(Duration::from_secs(30)) => Next::Timeout,
            }
        };
        match next {
            Next::Action(Some(action)) => {
                // Read the reply before refreshing: the Mac answers an action
                // on the same stream, and a failure (a closed pane, a missing
                // agent) is worth a notice rather than a silent refresh.
                let done = wire.request(action.json).await;
                if let Err(error) = &done {
                    eprintln!("[kumu] action failed: {error:#}");
                    let _ = output.send(Feed::Notice(error_notice(error)));
                }
                if !action.refresh {
                    continue;
                }
                match refresh_state(&mut wire, &panes, &agents).await {
                    Ok(preferred) => {
                        // A successful action moved the Mac's focus on
                        // purpose (a new pane, a closed pane's neighbour), so
                        // the phone follows it. A failed one moved nothing.
                        let preferred = preferred.filter(|_| done.is_ok());
                        if let Err(error) = ensure_attached(
                            &mut wire, &panes, &channel, &current, &size, &output, &title,
                            preferred,
                        )
                        .await
                        {
                            eprintln!("[kumu] reattach failed: {error:#}");
                            let _ = output.send(Feed::Notice(error_notice(&error)));
                        }
                    }
                    Err(error) => {
                        eprintln!("[kumu] refresh failed: {error:#}");
                        let _ = output.send(Feed::Notice("刷新窗格列表失败".into()));
                    }
                }
            }
            Next::Action(None) => break,
            Next::Switch(Some(pane)) => {
                let switched = switch_pane(
                    &mut wire, pane, &channel, &current, &size, &output, &title, &panes,
                )
                .await;
                if let Err(error) = switched {
                    eprintln!("[kumu] switch failed: {error:#}");
                    let _ = output.send(Feed::Notice(error_notice(&error)));
                }
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
                        // A reply to a request that already timed out: its
                        // result belongs to that failed call, so it neither
                        // titles the terminal nor ends the session.
                        if value["id"]
                            .as_u64()
                            .is_some_and(|id| wire.abandoned.remove(&id))
                        {
                            continue;
                        }
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
                                // The Mac ended this pane. Move to one that
                                // still exists instead of dropping the whole
                                // connection.
                                current.store(0, Ordering::Release);
                                channel.store(0, Ordering::Release);
                                let preferred = match refresh_state(&mut wire, &panes, &agents)
                                    .await
                                {
                                    Ok(preferred) => preferred,
                                    Err(error) => {
                                        eprintln!("[kumu] refresh after close failed: {error:#}");
                                        let _ = output.send(Feed::Notice("窗格已关闭".into()));
                                        break;
                                    }
                                };
                                if let Err(error) = attach_first(
                                    &mut wire, &panes, &channel, &current, &size, &output, &title,
                                    preferred,
                                )
                                .await
                                {
                                    eprintln!("[kumu] reattach after close failed: {error:#}");
                                    let _ = output.send(Feed::Notice(error_notice(&error)));
                                }
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
    wire: &mut Wire<'_>,
    link: &PairingLink,
    code_text: &str,
    status: &Signal<Status>,
    title: &Signal<String>,
    panes: &Signal<Vec<PaneSummary>>,
    agents: &Signal<Vec<AgentSummary>>,
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
    let hello = match wire
        .request(json!({
            "jsonrpc": "2.0",
            "method": "hello",
            "params": hello_params(&token)
        }))
        .await
    {
        Ok(value) => value,
        Err(error) if error.to_string().starts_with("unauthorized") => {
            // A token from an older install or another Mac is not an error
            // worth surfacing before the user has had a chance to pair.
            token.clear();
            let _ = Storage::set_item("token", "");
            wire.request(json!({
                "jsonrpc": "2.0",
                "method": "hello",
                "params": hello_params("")
            }))
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
        let paired = wire
            .request(json!({
                "jsonrpc": "2.0",
                "method": "pair",
                "params": {"code": code, "name": device_name()}
            }))
            .await?;
        let token = paired["result"]["token"]
            .as_str()
            .context("pairing returned no token")?
            .to_owned();
        let _ = Storage::set_item("token", token);
    } else if hello.get("error").is_some() {
        bail!("this phone is not paired with the Mac any more");
    }

    let state = wire
        .request(json!({"jsonrpc": "2.0", "method": "workspace.state"}))
        .await?;
    let summaries = pane_summaries(&state);
    let pane = state["result"]["recentPane"]
        .as_u64()
        .or_else(|| state["result"]["focusedPane"].as_u64())
        .or_else(|| summaries.first().map(|pane| pane.id))
        .context("the Mac has no pane to show")?;
    let project_name = summaries
        .iter()
        .find(|summary| summary.id == pane)
        .map(|summary| summary.project.clone())
        .unwrap_or_default();
    panes.set(summaries);
    agents.set(agent_summaries(&state));
    // Wait for the terminal view's first layout. Attaching at a guessed size
    // would make the replay wrap on the phone and never match the PTY.
    let (cols, rows) = tokio::time::timeout(Duration::from_secs(5), attach_rx.recv())
        .await
        .ok()
        .flatten()
        .unwrap_or((80, 24));
    attach_rx.close();
    let attach = wire
        .request(json!({
            "jsonrpc": "2.0",
            "method": "pane.attach",
            "params": {"pane": pane, "cols": cols, "rows": rows}
        }))
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
    wire: &mut Wire<'_>,
    pane: u64,
    channel: &Arc<AtomicU32>,
    current: &Arc<AtomicU64>,
    size: &Arc<Mutex<(u16, u16)>>,
    output: &mpsc::UnboundedSender<Feed>,
    title: &Signal<String>,
    panes: &Signal<Vec<PaneSummary>>,
) -> Result<()> {
    // Attach the new pane before releasing the old one: a pane the Mac has
    // already closed must leave the phone on the pane it still shows.
    let (cols, rows) = *size.lock().expect("terminal size poisoned");
    let attach = wire
        .request(json!({
            "jsonrpc": "2.0",
            "method": "pane.attach",
            "params": {"pane": pane, "cols": cols, "rows": rows}
        }))
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
    // The old channel is a channel id, not the old pane id.
    let old_channel = channel.load(Ordering::Acquire);
    if old_channel != 0
        && let Err(error) = wire
            .request(json!({
                "jsonrpc": "2.0",
                "method": "pane.detach",
                "params": {"channel": old_channel}
            }))
            .await
    {
        eprintln!("[kumu] detach failed: {error:#}");
    }
    channel.store(next_channel, Ordering::Release);
    current.store(pane, Ordering::Release);
    output
        .send(Feed::Reset)
        .map_err(|_| anyhow::anyhow!("terminal view closed"))?;
    Ok(())
}

/// Follows the pane the Mac focused when it differs from the attached one,
/// keeps the attached pane while the list still has it, and attaches the
/// first pane when none is attached but one appeared.
#[allow(clippy::too_many_arguments)]
async fn ensure_attached(
    wire: &mut Wire<'_>,
    panes: &Signal<Vec<PaneSummary>>,
    channel: &Arc<AtomicU32>,
    current: &Arc<AtomicU64>,
    size: &Arc<Mutex<(u16, u16)>>,
    output: &mpsc::UnboundedSender<Feed>,
    title: &Signal<String>,
    preferred: Option<u64>,
) -> Result<()> {
    let attached = current.load(Ordering::Acquire);
    let list = panes.get_untracked();
    let listed = |id: u64| list.iter().any(|pane| pane.id == id);
    if let Some(pane) = preferred
        && pane != attached
        && listed(pane)
    {
        return switch_pane(wire, pane, channel, current, size, output, title, panes).await;
    }
    if attached != 0 && listed(attached) {
        return Ok(());
    }
    if attached == 0 && list.is_empty() {
        return Ok(());
    }
    attach_first(
        wire, panes, channel, current, size, output, title, preferred,
    )
    .await
}

/// Attaches `preferred` when it is still listed, otherwise the first pane, or
/// clears the terminal when none is left.
#[allow(clippy::too_many_arguments)]
async fn attach_first(
    wire: &mut Wire<'_>,
    panes: &Signal<Vec<PaneSummary>>,
    channel: &Arc<AtomicU32>,
    current: &Arc<AtomicU64>,
    size: &Arc<Mutex<(u16, u16)>>,
    output: &mpsc::UnboundedSender<Feed>,
    title: &Signal<String>,
    preferred: Option<u64>,
) -> Result<()> {
    let list = panes.get_untracked();
    let pane = preferred
        .filter(|id| list.iter().any(|pane| pane.id == *id))
        .or_else(|| list.first().map(|pane| pane.id));
    let Some(pane) = pane else {
        // Nothing left to show: stop the pane stream and say so.
        channel.store(0, Ordering::Release);
        current.store(0, Ordering::Release);
        let _ = output.send(Feed::Reset);
        let _ = output.send(Feed::Notice("没有可用的窗格了".into()));
        return Ok(());
    };
    switch_pane(wire, pane, channel, current, size, output, title, panes).await
}

/// Refreshes the pane list and agents, reporting the pane the Mac has
/// focused so a reattach lands where the desktop is looking.
async fn refresh_state(
    wire: &mut Wire<'_>,
    panes: &Signal<Vec<PaneSummary>>,
    agents: &Signal<Vec<AgentSummary>>,
) -> Result<Option<u64>> {
    let state = wire
        .request(json!({"jsonrpc": "2.0", "method": "workspace.state"}))
        .await?;
    panes.set(pane_summaries(&state));
    agents.set(agent_summaries(&state));
    Ok(state["result"]["focusedPane"]
        .as_u64()
        .or_else(|| state["result"]["recentPane"].as_u64()))
}

fn pane_summaries(state: &Value) -> Vec<PaneSummary> {
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
    state["result"]["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pane| {
            let project_id = pane["project"].as_i64().unwrap_or_default();
            Some(PaneSummary {
                id: pane["id"].as_u64()?,
                project_id,
                project: project_names.get(&project_id).cloned().unwrap_or_default(),
                title: pane["title"].as_str().unwrap_or("终端").to_owned(),
                status: pane["status"].as_str().unwrap_or("unknown").to_owned(),
                unread: pane["unread"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

fn agent_summaries(state: &Value) -> Vec<AgentSummary> {
    state["result"]["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|agent| agent["installed"].as_bool().unwrap_or(false))
        .filter_map(|agent| {
            Some(AgentSummary {
                id: agent["id"].as_str()?.to_owned(),
                name: agent["name"].as_str()?.to_owned(),
            })
        })
        .collect()
}

/// Maps the Mac's error kinds to a message a phone user can act on. Unknown
/// kinds keep the Mac's text after a Chinese prefix.
fn friendly_error(kind: &str, message: &str) -> String {
    match (kind, message) {
        ("pane_gone", _) => "窗格已经关闭".to_owned(),
        ("invalid_params", _) => "这个操作当前不可用".to_owned(),
        ("invalid_state", "no project") => "桌面上没有可用的项目".to_owned(),
        ("invalid_state", "desktop is not available") => "桌面端当前不可用".to_owned(),
        ("invalid_state", "desktop did not answer") => "桌面端没有响应".to_owned(),
        ("invalid_state", _) => "桌面端现在无法执行这个操作".to_owned(),
        _ => format!("操作失败：{message}"),
    }
}

/// Socket-side failures have no Mac error kind in them, so their English
/// context would reach the Chinese UI unchanged; give the common ones text a
/// phone user can read.
fn internal_error_notice(text: &str) -> String {
    match text {
        "the Mac did not answer" => "桌面端没有响应".to_owned(),
        "attach returned no channel" => "桌面端没有返回终端通道".to_owned(),
        "connection closed" => "连接已断开".to_owned(),
        "terminal view closed" => "终端已关闭".to_owned(),
        _ => format!("操作失败：{text}"),
    }
}

fn error_notice(error: &anyhow::Error) -> String {
    let text = error.to_string();
    match text.split_once(": ") {
        Some((kind, message)) => friendly_error(kind, message),
        None => internal_error_notice(&text),
    }
}

/// The control stream. A request reads the stream while it waits for its own
/// answer; anything it does not own (a pong, an action's reply, a terminal
/// frame) goes back to the loop instead of being dropped.
struct Wire<'a> {
    tx: &'a mpsc::UnboundedSender<Outgoing>,
    rx: &'a mut mpsc::Receiver<Result<Message>>,
    pending: VecDeque<Result<Message>>,
    /// The next request's id: never reused, so a reply can only be the answer
    /// to the request that sent it.
    next_id: u64,
    /// Ids whose request timed out: their reply, if it ever comes, answers
    /// nothing and must not end the session.
    abandoned: HashSet<u64>,
}

impl Wire<'_> {
    /// Numbers `request` and returns its id.
    fn stamp(&mut self, request: &mut Value) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        request["id"] = json!(id);
        id
    }

    async fn request(&mut self, mut request: Value) -> Result<Value> {
        let id = self.stamp(&mut request);
        let expected = json!(id);
        self.tx
            .send(Outgoing::Control(request.to_string()))
            .map_err(|_| anyhow::anyhow!("connection closed"))?;
        // One deadline for the whole call: terminal output or a ping must not
        // extend the wait for this request's answer.
        let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
        loop {
            let message = match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(message) => message.context("connection closed")??,
                Err(_) => {
                    self.abandoned.insert(id);
                    bail!("the Mac did not answer");
                }
            };
            match message {
                Message::Text(text) => {
                    let value: Value = serde_json::from_str(&text)?;
                    // Only this request's own id may complete it; pings and
                    // other replies keep their place for the loop.
                    if value.get("id") != Some(&expected) {
                        self.pending.push_back(Ok(Message::Text(text)));
                        continue;
                    }
                    if value.get("error").is_some() {
                        let kind = value["error"]["data"]["kind"].as_str().unwrap_or("unknown");
                        let message = value["error"]["message"].as_str().unwrap_or("Mac 返回错误");
                        bail!("{kind}: {message}");
                    }
                    if value.get("result").is_some() {
                        return Ok(value);
                    }
                }
                other => self.pending.push_back(Ok(other)),
            }
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
    use super::{KIND_DATA, Message, Utf8Stream, Wire, error_notice, frame, friendly_error};
    use serde_json::json;
    use std::collections::{HashSet, VecDeque};
    use tokio::sync::mpsc;

    fn wire<'a>(
        tx: &'a mpsc::UnboundedSender<super::Outgoing>,
        rx: &'a mut mpsc::Receiver<super::Result<Message>>,
    ) -> Wire<'a> {
        Wire {
            tx,
            rx,
            pending: VecDeque::new(),
            next_id: 7,
            abandoned: HashSet::new(),
        }
    }

    #[test]
    fn each_request_gets_its_own_id() {
        let (wire_tx, _wire_rx) = mpsc::unbounded_channel();
        let (_tx, mut rx) = mpsc::channel(1);
        let mut wire = wire(&wire_tx, &mut rx);
        let mut first = json!({"jsonrpc": "2.0", "method": "a"});
        let mut second = json!({"jsonrpc": "2.0", "method": "b"});
        let first_id = wire.stamp(&mut first);
        let second_id = wire.stamp(&mut second);
        assert_ne!(first_id, second_id);
        assert_eq!(first["id"], json!(first_id));
        assert_eq!(second["id"], json!(second_id));
    }

    #[test]
    fn a_character_split_across_chunks_is_written_once() {
        let mut stream = Utf8Stream::default();
        let bytes = "你好".as_bytes();
        assert_eq!(stream.push(&bytes[..2]), None);
        assert_eq!(stream.push(&bytes[2..]), Some("你好".to_owned()));
    }

    #[tokio::test]
    async fn a_request_keeps_the_messages_it_does_not_own() {
        let (wire_tx, _wire_rx) = mpsc::unbounded_channel();
        let (tx, mut rx) = mpsc::channel(8);
        tx.send(Ok(Message::Text(
            r#"{"jsonrpc":"2.0","id":0,"result":{"pong":true}}"#.into(),
        )))
        .await
        .expect("the test channel accepts a pong");
        tx.send(Ok(Message::Binary(frame(1, KIND_DATA, b"hi"))))
            .await
            .expect("the test channel accepts a terminal frame");
        tx.send(Ok(Message::Text(
            r#"{"jsonrpc":"2.0","id":7,"result":{"title":"x"}}"#.into(),
        )))
        .await
        .expect("the test channel accepts the answer");
        drop(tx);
        let mut wire = wire(&wire_tx, &mut rx);
        let value = wire
            .request(json!({"jsonrpc": "2.0", "method": "test"}))
            .await
            .expect("the matching answer completes the request");
        assert_eq!(value["result"]["title"], "x");
        // The pong and the terminal frame go back to the loop, not away.
        assert_eq!(wire.pending.len(), 2);
        assert_eq!(wire.next_id, 8);
    }

    #[test]
    fn action_errors_read_as_ui_text() {
        assert_eq!(
            friendly_error("pane_gone", "the pane is not open"),
            "窗格已经关闭"
        );
        assert_eq!(
            friendly_error("invalid_state", "no project"),
            "桌面上没有可用的项目"
        );
        assert_eq!(
            friendly_error("invalid_state", "desktop did not answer"),
            "桌面端没有响应"
        );
        assert_eq!(
            friendly_error("something_new", "details"),
            "操作失败：details"
        );
    }

    #[test]
    fn a_switch_error_keeps_its_kind() {
        let error = anyhow::anyhow!("pane_gone: the terminal is not available");
        assert_eq!(error_notice(&error), "窗格已经关闭");
        let error = anyhow::anyhow!("socket closed");
        assert_eq!(error_notice(&error), "操作失败：socket closed");
        // The request timeout carries no Mac kind, but must still read as UI
        // text rather than its English context.
        let error = anyhow::anyhow!("the Mac did not answer");
        assert_eq!(error_notice(&error), "桌面端没有响应");
        let error = anyhow::anyhow!("attach returned no channel");
        assert_eq!(error_notice(&error), "桌面端没有返回终端通道");
    }
}
