//! Kumu for iPhone: connect to a Mac running Kumu and drive its terminals.
//!
//! The first screen is pairing; after that the app opens the last pane and
//! stays terminal-first. The panes drawer and desktop settings come next.

mod net;

use std::time::Duration;

use net::{AgentSummary, Feed, PaneSummary, Session, Status, Utf8Stream};
use nsh_base::{
    MenuEvent, SheetEvent, Terminal, TerminalApi, TerminalEvent, alert_dialog, button, input, menu,
    menu_item, menu_separator, sheet, terminal, theme,
};
use nsh_core::prelude::*;
use nsh_core::storage::Storage;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionConfig {
    pub link: String,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq)]
enum DrawerRow {
    Header { project: i64, count: usize },
    Pane(PaneSummary),
}

fn drawer_label(project: &str) -> &str {
    if project.is_empty() {
        "未分组"
    } else {
        project
    }
}

/// Groups panes by project in the order each project first appears. The Mac
/// lists panes by id, so a later pane of an earlier project can sit between
/// two panes of a later one; grouping by runs would split it into two headers.
fn drawer_rows(panes: &[PaneSummary], collapsed: &[i64]) -> Vec<DrawerRow> {
    let mut groups: Vec<(i64, Vec<&PaneSummary>)> = Vec::new();
    for pane in panes {
        match groups
            .iter_mut()
            .find(|(project, ..)| *project == pane.project_id)
        {
            Some((_, members)) => members.push(pane),
            None => groups.push((pane.project_id, vec![pane])),
        }
    }
    let mut rows = Vec::new();
    for (project, members) in groups {
        rows.push(DrawerRow::Header {
            project,
            count: members.len(),
        });
        if !collapsed.contains(&project) {
            rows.extend(members.into_iter().cloned().map(DrawerRow::Pane));
        }
    }
    rows
}

/// The app's UI set (`nsh.json` `"rust"`).
#[native_module("kumu-mobile/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The app's root view: pairing, then the full-screen terminal.
#[component]
pub fn App() -> impl IntoElement {
    // Development hooks: `KUMU_AUTOLINK` and `KUMU_AUTOCODE` skip the form,
    // so simulator checks need no paste.
    let auto = std::env::var("KUMU_AUTOLINK")
        .ok()
        .map(|link| ConnectionConfig {
            link,
            code: std::env::var("KUMU_AUTOCODE").unwrap_or_default(),
        });
    let connected = signal(auto.is_some());
    let config = signal(auto.unwrap_or_default());

    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(
            when(
                move || connected.get(),
                move || terminal_page(config.get(), connected),
            )
            .otherwise(move || connect_page(connected, config)),
        )
}

fn connect_page(connected: Signal<bool>, config: Signal<ConnectionConfig>) -> impl IntoElement {
    let link = signal(Storage::get_item("link").unwrap_or_default());
    let code = signal(Storage::get_item("pairing_code").unwrap_or_default());
    let error = signal(String::new());

    let connect = move || {
        let link = link.get_untracked().trim().to_owned();
        let code = code.get_untracked().trim().to_owned();
        if link.is_empty() {
            error.set("先粘贴 Mac 上显示的配对链接。".into());
            return;
        }
        let _ = Storage::set_item("link", link.clone());
        let _ = Storage::set_item("pairing_code", code.clone());
        error.set(String::new());
        config.set(ConnectionConfig { link, code });
        connected.set(true);
    };

    view()
        .flex_1()
        .p_safe_area()
        .px(px(24.0))
        .justify_center()
        .gap(px(18.0))
        .child(
            view()
                .gap(px(6.0))
                .child(
                    text("Kumu")
                        .text_size(px(34.0))
                        .text_color(move || theme().foreground),
                )
                .child(
                    text("你的编码 Agent，在手机上继续。")
                        .text_size(px(15.0))
                        .text_color(move || theme().muted_foreground),
                ),
        )
        .child(
            view()
                .gap(px(12.0))
                .p(px(16.0))
                .rounded(px(14.0))
                .bg(move || theme().card)
                .border_1()
                .border_color(move || theme().border)
                .child(field_label("配对链接"))
                .child(
                    input()
                        .value(move || link.get())
                        .on_change(move |value: &str| link.set(value.to_owned()))
                        .on_submit(move |_: &str| connect())
                        .placeholder("kumu://pair/…")
                        .px(px(12.0))
                        .py(px(10.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(move || theme().input)
                        .text_color(move || theme().foreground),
                )
                .child(field_label("配对码（链接里没有时填）"))
                .child(
                    input()
                        .value(move || code.get())
                        .on_change(move |value: &str| code.set(value.to_owned()))
                        .on_submit(move |_: &str| connect())
                        .placeholder("ABCD-EFGH")
                        .px(px(12.0))
                        .py(px(10.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(move || theme().input)
                        .text_color(move || theme().foreground),
                )
                .child(
                    button()
                        .on_press(connect)
                        .items_center()
                        .justify_center()
                        .py(px(12.0))
                        .rounded(px(10.0))
                        .bg(move || theme().primary)
                        .active(|style| style.opacity(0.7))
                        .child(
                            text("连接")
                                .text_size(px(16.0))
                                .text_color(move || theme().primary_foreground),
                        ),
                )
                .child(
                    text(move || error.get())
                        .text_size(px(13.0))
                        .text_color(move || theme().destructive),
                ),
        )
        .child(
            text("在 Mac 上打开 Kumu → 设置 → 远程，复制配对链接。")
                .text_size(px(12.0))
                .text_color(move || theme().muted_foreground),
        )
}

fn field_label(label: &'static str) -> impl IntoElement {
    text(label)
        .text_size(px(12.0))
        .text_color(move || theme().muted_foreground)
}

fn terminal_page(config: ConnectionConfig, connected: Signal<bool>) -> impl IntoElement {
    let terminal_ref = component_ref::<Terminal>();
    let status = signal(Status::Connecting);
    let title = signal("终端".to_owned());
    let panes = signal(Vec::<PaneSummary>::new());
    let agents = signal(Vec::<AgentSummary>::new());
    let notice = signal(Option::<String>::None);
    let notice_version = signal(0u64);
    let sheet_open = signal(std::env::var("KUMU_AUTO_SHEET").is_ok());
    let menu_open = signal(std::env::var("KUMU_AUTO_MENU").is_ok());
    let agent_open = signal(std::env::var("KUMU_AUTO_AGENT").is_ok());
    let ctrl = signal(false);
    let alt = signal(false);
    let font_size = signal(
        Storage::get_item("font_size")
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(13.0)
            .clamp(10.0, 24.0),
    );
    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel::<Feed>();
    let session = net::connect(
        config.link,
        config.code,
        status,
        title,
        panes,
        agents,
        output_tx,
    );

    // The socket runs on a background task; this one only feeds the native
    // terminal, so the UI thread never does network work.
    let mut stream = Utf8Stream::default();
    let view_ref = terminal_ref;
    let credit_session = session.clone();
    spawn(async move {
        while let Some(feed) = output_rx.recv().await {
            match feed {
                Feed::Reset => {
                    if let Some(api) = view_ref.api::<TerminalApi>() {
                        api.reset();
                    }
                }
                Feed::Data(bytes) => {
                    let consumed = bytes.len();
                    if let Some(text) = stream.push(&bytes)
                        && let Some(api) = view_ref.api::<TerminalApi>()
                    {
                        api.write(text);
                    }
                    credit_session.credit(consumed);
                }
                Feed::Notice(message) => show_notice(notice, notice_version, message),
            }
        }
    });

    let input_session = session.clone();
    let resize_session = session.clone();
    let sheet_session = session.clone();
    let menu_session = session.clone();
    let disconnect_session = session.clone();
    let show_session = session.clone();
    let close_session = session.clone();
    let agent_session = session.clone();
    let focus_ref = terminal_ref;

    view()
        .flex_1()
        .child(header(title, status, sheet_open, menu_open))
        .child(
            view().flex_1().bg(move || theme().background).child(
                terminal()
                    .component_ref(&terminal_ref)
                    .scrollback(2_000u32)
                    .font_size(move || px(font_size.get()))
                    .line_height(move || px(font_size.get() * 1.4))
                    .accessibility_label("终端")
                    .on_data(move |data: &SharedString| {
                        let (text, used_ctrl, used_alt) = apply_modifiers(
                            data.to_string(),
                            ctrl.get_untracked(),
                            alt.get_untracked(),
                        );
                        if used_ctrl {
                            ctrl.set(false);
                        }
                        if used_alt {
                            alt.set(false);
                        }
                        input_session.input(text);
                    })
                    .on_resize(move |event: &TerminalEvent| {
                        if let TerminalEvent::Resize(cols, rows) = event {
                            resize_session.resize(*cols, *rows);
                        }
                    })
                    .flex_1(),
            ),
        )
        .child(key_bar(session, focus_ref, ctrl, alt))
        .child(when(
            move || notice.get().is_some(),
            move || notice_overlay(notice),
        ))
        .child(
            sheet()
                .open(move || Some(sheet_open.get()))
                .close_on_swipe(true)
                .on_open_change(move |event: &SheetEvent| {
                    if let SheetEvent::OpenChange(next, _) = event {
                        sheet_open.set(*next);
                    }
                })
                .child(pane_sheet(panes, sheet_session, sheet_open, connected)),
        )
        .child(
            menu()
                .open(move || Some(menu_open.get()))
                .on_open_change(move |event: &MenuEvent| {
                    if let MenuEvent::OpenChange(next, _) = event {
                        menu_open.set(*next);
                    }
                })
                .child(
                    view()
                        .flex_col()
                        .gap(px(2.0))
                        .p(px(8.0))
                        .pb_safe_area_plus(px(8.0))
                        .rounded_t(px(18.0))
                        .bg(move || theme().card)
                        .child(
                            menu_item()
                                .on_select(move || {
                                    show_session.show_on_desktop(show_session.current_pane());
                                    menu_open.set(false);
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("在桌面显示")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().foreground),
                                ),
                        )
                        .child(
                            menu_item()
                                .on_select(move || {
                                    menu_session.new_terminal();
                                    menu_open.set(false);
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("新建终端")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().foreground),
                                ),
                        )
                        .child(
                            menu_item()
                                .on_select(move || {
                                    menu_open.set(false);
                                    agent_open.set(true);
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("新建 Agent")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().foreground),
                                ),
                        )
                        .child(
                            menu_item()
                                .on_select(move || {
                                    adjust_font_size(font_size, 1.0);
                                    menu_open.set(false);
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("字号 +")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().foreground),
                                ),
                        )
                        .child(
                            menu_item()
                                .on_select(move || {
                                    adjust_font_size(font_size, -1.0);
                                    menu_open.set(false);
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("字号 −")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().foreground),
                                ),
                        )
                        .child(
                            menu_item()
                                .on_select(move || {
                                    let close_session = close_session.clone();
                                    let pane = close_session.current_pane();
                                    menu_open.set(false);
                                    let answer = alert_dialog()
                                        .title("关闭窗格？")
                                        .description("这个窗格里的进程会被结束。")
                                        .cancel("cancel", "取消")
                                        .destructive("close", "关闭")
                                        .show();
                                    spawn(async move {
                                        if answer.await.is_ok_and(|result| {
                                            result.action.as_deref() == Some("close")
                                        }) {
                                            close_session.close_pane(pane);
                                        }
                                    });
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("关闭窗格")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().destructive),
                                ),
                        )
                        .child(
                            menu_separator()
                                .my(px(4.0))
                                .h(px(1.0))
                                .bg(move || theme().border),
                        )
                        .child(
                            menu_item()
                                .on_select(move || {
                                    disconnect_session.close();
                                    connected.set(false);
                                    menu_open.set(false);
                                })
                                .px(px(14.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .child(
                                    text("断开连接")
                                        .text_size(px(14.0))
                                        .text_color(move || theme().destructive),
                                ),
                        ),
                ),
        )
        .child(
            sheet()
                .open(move || Some(agent_open.get()))
                .close_on_swipe(true)
                .on_open_change(move |event: &SheetEvent| {
                    if let SheetEvent::OpenChange(next, _) = event {
                        agent_open.set(*next);
                    }
                })
                .child(agent_sheet(agents, agent_session, agent_open)),
        )
}

/// Shows a transient message over the terminal. A newer message replaces the
/// old one and restarts the timer; tapping it dismisses it early.
fn show_notice(notice: Signal<Option<String>>, version: Signal<u64>, message: String) {
    let Some(next) = version.try_get().map(|current| current.wrapping_add(1)) else {
        return;
    };
    if !version.try_set(next) || !notice.try_set(Some(message)) {
        return;
    }
    spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        // Only the newest notice clears: an older timer leaves a newer one
        // alone, even when both say the same thing.
        if version.try_get() == Some(next) {
            notice.try_set(None);
        }
    });
}

fn notice_overlay(notice: Signal<Option<String>>) -> impl IntoElement {
    overlay()
        .absolute()
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .px(px(12.0))
        // Clear the key bar: two rows, their padding, and the home indicator.
        .pb_keyboard_plus(px(132.0))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .px(px(14.0))
                .py(px(10.0))
                .rounded(px(12.0))
                .bg(move || theme().card)
                .border_1()
                .border_color(move || theme().destructive)
                .shadow_lg()
                .on_press(move || notice.set(None))
                .child(
                    text("!")
                        .text_size(px(13.0))
                        .text_color(move || theme().destructive),
                )
                .child(
                    text(move || notice.get().unwrap_or_default())
                        .flex_1()
                        .number_of_lines(2u32)
                        .text_size(px(13.0))
                        .text_color(move || theme().foreground),
                ),
        )
}

fn header(
    title: Signal<String>,
    status: Signal<Status>,
    sheet_open: Signal<bool>,
    menu_open: Signal<bool>,
) -> impl IntoElement {
    view()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .px(px(10.0))
        .py(px(8.0))
        .pt_safe_area()
        .border_b_1()
        .border_b_color(move || theme().border)
        .bg(move || theme().card)
        .child(
            button()
                .on_press(move || sheet_open.set(true))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(8.0))
                .active(|style| style.opacity(0.6))
                .child(
                    text("☰")
                        .text_size(px(16.0))
                        .text_color(move || theme().foreground),
                ),
        )
        .child(
            view()
                .flex_1()
                .flex_col()
                .gap(px(2.0))
                .child(
                    text(move || title.get())
                        .number_of_lines(1u32)
                        .text_size(px(14.0))
                        .text_color(move || theme().foreground),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(5.0))
                        .child(
                            view()
                                .size(px(7.0))
                                .rounded(px(4.0))
                                .bg(move || status_color(status.get())),
                        )
                        .child(
                            text(move || match status.get() {
                                Status::Failed(message) => message,
                                state => state.label().to_owned(),
                            })
                            .number_of_lines(1u32)
                            .text_size(px(11.0))
                            .text_color(move || theme().muted_foreground),
                        ),
                ),
        )
        .child(
            button()
                .on_press(move || menu_open.set(true))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(8.0))
                .active(|style| style.opacity(0.6))
                .child(
                    text("⋯")
                        .text_size(px(16.0))
                        .text_color(move || theme().foreground),
                ),
        )
}

fn agent_sheet(
    agents: Signal<Vec<AgentSummary>>,
    session: Session,
    open: Signal<bool>,
) -> impl IntoElement {
    let row_session = session.clone();
    let list = view().flex_col().gap(px(2.0)).child(each(
        move || agents.get(),
        |agent| agent.id.clone(),
        move |agent, _| {
            let agent = agent.get_untracked();
            let session = row_session.clone();
            view()
                .flex_row()
                .items_center()
                .p(px(12.0))
                .rounded(px(10.0))
                .bg(move || theme().card)
                .active(|style| style.opacity(0.6))
                .on_press(move || {
                    session.new_agent(&agent.id);
                    open.set(false);
                })
                .child(
                    text(agent.name.clone())
                        .text_size(px(14.0))
                        .text_color(move || theme().foreground),
                )
        },
    ));

    view()
        .flex_col()
        .gap(px(10.0))
        .p(px(16.0))
        .pb_safe_area_plus(px(10.0))
        .rounded_t(px(18.0))
        .bg(move || theme().card)
        .border_t_1()
        .border_t_color(move || theme().border)
        .child(
            view()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .child(
                    view()
                        .w(px(36.0))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(move || theme().border),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .w_full()
                        .child(
                            text("新建 Agent")
                                .flex_1()
                                .text_size(px(15.0))
                                .text_color(move || theme().foreground),
                        )
                        .child(
                            button()
                                .on_press(move || open.set(false))
                                .px(px(8.0))
                                .py(px(4.0))
                                .active(|style| style.opacity(0.6))
                                .child(
                                    text("完成")
                                        .text_size(px(13.0))
                                        .text_color(move || theme().primary),
                                ),
                        ),
                ),
        )
        .child(scroll().max_h(px(380.0)).child(list))
        .child(when(
            move || agents.get().is_empty(),
            move || {
                view().py(px(24.0)).items_center().child(
                    text("没有检测到已安装的 Agent")
                        .text_size(px(13.0))
                        .text_color(move || theme().muted_foreground),
                )
            },
        ))
}

fn adjust_font_size(font_size: Signal<f32>, delta: f32) {
    let next = (font_size.get_untracked() + delta).clamp(10.0, 24.0);
    font_size.set(next);
    let _ = Storage::set_item("font_size", next.to_string());
}

fn pane_sheet(
    panes: Signal<Vec<PaneSummary>>,
    session: Session,
    open: Signal<bool>,
    connected: Signal<bool>,
) -> impl IntoElement {
    let current = session.current_pane();
    let row_session = session.clone();
    let collapsed = signal(Vec::<i64>::new());
    let rows = computed(move || drawer_rows(&panes.get(), &collapsed.get()));
    let list = view().flex_col().gap(px(2.0)).child(each(
        rows,
        |row| match row {
            DrawerRow::Header { project, .. } => format!("h:{project}"),
            DrawerRow::Pane(pane) => format!("p:{}", pane.id),
        },
        move |row, _| match row.get_untracked() {
            DrawerRow::Header { project, .. } => {
                let project_open = project;
                let project_press = project;
                let project_count = project;
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(8.0))
                    .active(|style| style.opacity(0.6))
                    .on_press(move || {
                        let mut next = collapsed.get_untracked();
                        if let Some(index) = next.iter().position(|id| *id == project_press) {
                            next.remove(index);
                        } else {
                            next.push(project_press);
                        }
                        collapsed.set(next);
                    })
                    .child(
                        // The chevron and the count follow their signals so a
                        // row that stays mounted still shows the new state.
                        text(move || {
                            if collapsed.get().contains(&project_open) {
                                "▸"
                            } else {
                                "▾"
                            }
                        })
                        .text_size(px(10.0))
                        .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        // The label follows the signal like the chevron and
                        // the count: a refresh can rename a project while the
                        // row stays mounted.
                        text(move || {
                            let list = panes.get();
                            let name = list
                                .iter()
                                .find(|pane| pane.project_id == project_open)
                                .map(|pane| pane.project.as_str())
                                .unwrap_or_default();
                            drawer_label(name).to_owned()
                        })
                        .flex_1()
                        .text_size(px(11.0))
                        .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        text(move || {
                            panes
                                .get()
                                .iter()
                                .filter(|pane| pane.project_id == project_count)
                                .count()
                                .to_string()
                        })
                        .text_size(px(11.0))
                        .text_color(move || theme().muted_foreground),
                    )
            }
            DrawerRow::Pane(pane) => {
                let id = pane.id;
                let active = id == current;
                let status_color = pane_status_color(&pane.status);
                let status_label = pane_status_label(&pane.status);
                let title = pane.title.clone();
                let unread = pane.unread;
                let session = row_session.clone();
                view()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .p(px(12.0))
                    .rounded(px(10.0))
                    .bg(move || if active { theme().muted } else { theme().card })
                    .active(|style| style.opacity(0.6))
                    .on_press(move || {
                        session.switch(id);
                        open.set(false);
                    })
                    .child(
                        view()
                            .size(px(7.0))
                            .rounded(px(4.0))
                            .bg(move || status_color),
                    )
                    .child(
                        view()
                            .flex_1()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                text(title)
                                    .number_of_lines(1u32)
                                    .text_size(px(14.0))
                                    .text_color(move || theme().foreground),
                            )
                            .child(
                                text(status_label)
                                    .number_of_lines(1u32)
                                    .text_size(px(11.0))
                                    .text_color(move || theme().muted_foreground),
                            ),
                    )
                    .when(unread, |row| {
                        row.child(
                            view()
                                .size(px(7.0))
                                .rounded(px(4.0))
                                .bg(move || theme().primary),
                        )
                    })
            }
        },
    ));

    let disconnect = move || {
        session.close();
        connected.set(false);
        open.set(false);
    };

    view()
        .flex_col()
        .gap(px(10.0))
        .p(px(16.0))
        .pb_safe_area_plus(px(10.0))
        .rounded_t(px(18.0))
        .bg(move || theme().card)
        .border_t_1()
        .border_t_color(move || theme().border)
        .child(
            view()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .child(
                    view()
                        .w(px(36.0))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(move || theme().border),
                )
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .w_full()
                        .child(
                            text("所有窗格")
                                .flex_1()
                                .text_size(px(15.0))
                                .text_color(move || theme().foreground),
                        )
                        .child(
                            button()
                                .on_press(move || open.set(false))
                                .px(px(8.0))
                                .py(px(4.0))
                                .active(|style| style.opacity(0.6))
                                .child(
                                    text("完成")
                                        .text_size(px(13.0))
                                        .text_color(move || theme().primary),
                                ),
                        ),
                ),
        )
        .child(scroll().max_h(px(380.0)).child(list))
        .child(when(
            move || panes.get().is_empty(),
            move || {
                view().py(px(24.0)).items_center().child(
                    text("还没有可以切换的窗格")
                        .text_size(px(13.0))
                        .text_color(move || theme().muted_foreground),
                )
            },
        ))
        .child(
            button()
                .on_press(disconnect)
                .w_full()
                .items_center()
                .justify_center()
                .py(px(10.0))
                .rounded(px(10.0))
                .bg(move || theme().muted)
                .active(|style| style.opacity(0.6))
                .child(
                    text("断开连接")
                        .text_size(px(13.0))
                        .text_color(move || theme().destructive),
                ),
        )
}

fn pane_status_label(status: &str) -> &'static str {
    match status {
        "waiting" => "等待确认",
        "working" => "运行中",
        "exited" => "已退出",
        _ => "空闲",
    }
}

fn pane_status_color(status: &str) -> Rgba {
    match status {
        "waiting" => rgb(0xeab308),
        "working" => rgb(0x22c55e),
        "exited" => rgb(0xef4444),
        _ => rgb(0x94a3b8),
    }
}

fn key_bar(
    session: Session,
    terminal_ref: ComponentRef<Terminal>,
    ctrl: Signal<bool>,
    alt: Signal<bool>,
) -> impl IntoElement {
    view()
        .bg(move || theme().card)
        .border_t_1()
        .border_t_color(move || theme().border)
        // The outer inset keeps the rows above the software keyboard; the
        // inner inset keeps them above the home indicator when it is hidden.
        .pb_keyboard()
        .child(
            view()
                .flex_col()
                .gap(px(6.0))
                .px(px(6.0))
                .py(px(6.0))
                .pb_safe_area_plus(px(4.0))
                .child(
                    view()
                        .flex_row()
                        .gap(px(6.0))
                        .child(key_button("esc", "\u{1b}", session.clone()))
                        .child(key_button("tab", "\t", session.clone()))
                        .child(key_button("^C", "\u{03}", session.clone()))
                        .child(key_button("^D", "\u{04}", session.clone()))
                        .child(key_toggle("ctrl", ctrl))
                        .child(key_toggle("alt", alt))
                        .child(key_button("del", "\u{7f}", session.clone()))
                        .child(
                            button()
                                .on_press(move || {
                                    if let Some(api) = terminal_ref.api::<TerminalApi>() {
                                        api.focus();
                                    }
                                })
                                .flex_1()
                                .py(px(8.0))
                                .rounded(px(8.0))
                                .bg(move || theme().primary)
                                .active(|style| style.opacity(0.6))
                                .child(
                                    text("⌨")
                                        .text_size(px(13.0))
                                        .text_color(move || theme().primary_foreground),
                                ),
                        ),
                )
                .child(
                    view()
                        .flex_row()
                        .gap(px(6.0))
                        .child(key_button("←", "\u{1b}[D", session.clone()))
                        .child(key_button("↑", "\u{1b}[A", session.clone()))
                        .child(key_button("↓", "\u{1b}[B", session.clone()))
                        .child(key_button("→", "\u{1b}[C", session.clone()))
                        .child(key_button("/", "/", session.clone()))
                        .child(key_button("-", "-", session.clone()))
                        .child(key_button("|", "|", session.clone()))
                        .child(key_button("~", "~", session)),
                ),
        )
}

fn key_toggle(label: &'static str, active: Signal<bool>) -> impl IntoElement {
    button()
        .on_press(move || active.set(!active.get_untracked()))
        .flex_1()
        .py(px(8.0))
        .rounded(px(8.0))
        .bg(move || {
            if active.get() {
                theme().primary
            } else {
                theme().muted
            }
        })
        .active(|style| style.opacity(0.6))
        .child(text(label).text_size(px(13.0)).text_color(move || {
            if active.get() {
                theme().primary_foreground
            } else {
                theme().muted_foreground
            }
        }))
}

fn key_button(label: &'static str, bytes: &'static str, session: Session) -> impl IntoElement {
    button()
        .on_press(move || session.input(bytes))
        .flex_1()
        .py(px(8.0))
        .rounded(px(8.0))
        .bg(move || theme().muted)
        .active(|style| style.opacity(0.6))
        .child(
            text(label)
                .text_size(px(13.0))
                .text_color(move || theme().muted_foreground),
        )
}

/// Applies a sticky Ctrl/Alt to a single printable character. Escape
/// sequences, paste and IME text pass through unchanged.
fn apply_modifiers(data: String, ctrl: bool, alt: bool) -> (String, bool, bool) {
    let mut chars = data.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return (data, false, false);
    };
    if ctrl && let Some(byte) = ctrl_byte(ch) {
        return (
            String::from_utf8(vec![byte]).expect("a control byte is ASCII"),
            true,
            false,
        );
    }
    if alt && ch.is_ascii() && !ch.is_control() {
        return (format!("\u{1b}{ch}"), false, true);
    }
    (data, false, false)
}

fn ctrl_byte(ch: char) -> Option<u8> {
    let lower = ch.to_ascii_lowercase();
    match lower {
        'a'..='z' => Some(lower as u8 - b'a' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn status_color(status: Status) -> Rgba {
    match status {
        Status::Connecting | Status::Pairing => rgb(0xeab308),
        Status::Connected => rgb(0x22c55e),
        Status::Closed => rgb(0x94a3b8),
        Status::Failed(_) => rgb(0xef4444),
    }
}

#[cfg(test)]
mod tests {
    use super::{DrawerRow, PaneSummary, apply_modifiers, drawer_label, drawer_rows};

    fn pane(id: u64, project_id: i64, project: &str) -> PaneSummary {
        PaneSummary {
            id,
            project_id,
            project: project.to_owned(),
            title: format!("pane {id}"),
            status: "idle".to_owned(),
            unread: false,
        }
    }

    #[test]
    fn sticky_ctrl_turns_a_letter_into_a_control_byte() {
        assert_eq!(apply_modifiers("c".into(), true, false).0, "\u{03}");
        assert_eq!(apply_modifiers("C".into(), true, false).0, "\u{03}");
        assert_eq!(apply_modifiers("[".into(), true, false).0, "\u{1b}");
    }

    #[test]
    fn sticky_alt_prefixes_escape_but_leaves_sequences_alone() {
        assert_eq!(apply_modifiers("b".into(), false, true).0, "\u{1b}b");
        assert_eq!(
            apply_modifiers("\u{1b}[A".into(), false, true).0,
            "\u{1b}[A"
        );
        assert_eq!(apply_modifiers("你".into(), true, true).0, "你");
    }

    #[test]
    fn panes_are_grouped_under_one_header_per_project() {
        let panes = [pane(1, 1, "kumu"), pane(2, 1, "kumu"), pane(3, 2, "tokens")];
        let rows = drawer_rows(&panes, &[]);
        assert_eq!(
            rows,
            vec![
                DrawerRow::Header {
                    project: 1,
                    count: 2,
                },
                DrawerRow::Pane(pane(1, 1, "kumu")),
                DrawerRow::Pane(pane(2, 1, "kumu")),
                DrawerRow::Header {
                    project: 2,
                    count: 1,
                },
                DrawerRow::Pane(pane(3, 2, "tokens")),
            ]
        );
    }

    #[test]
    fn a_project_without_a_name_reads_as_ungrouped() {
        assert_eq!(drawer_label(""), "未分组");
        assert_eq!(drawer_label("kumu"), "kumu");
    }

    #[test]
    fn interleaved_pane_ids_keep_one_header_per_project() {
        // The Mac sorts panes by id, so a later pane of the first project can
        // sit after one of the second.
        let panes = [pane(1, 1, "kumu"), pane(2, 2, "tokens"), pane(3, 1, "kumu")];
        let rows = drawer_rows(&panes, &[]);
        assert_eq!(
            rows,
            vec![
                DrawerRow::Header {
                    project: 1,
                    count: 2,
                },
                DrawerRow::Pane(pane(1, 1, "kumu")),
                DrawerRow::Pane(pane(3, 1, "kumu")),
                DrawerRow::Header {
                    project: 2,
                    count: 1,
                },
                DrawerRow::Pane(pane(2, 2, "tokens")),
            ]
        );
    }

    #[test]
    fn projects_with_the_same_name_keep_their_own_headers() {
        let panes = [pane(1, 1, "kumu"), pane(2, 2, "kumu")];
        let rows = drawer_rows(&panes, &[]);
        assert_eq!(
            rows,
            vec![
                DrawerRow::Header {
                    project: 1,
                    count: 1,
                },
                DrawerRow::Pane(pane(1, 1, "kumu")),
                DrawerRow::Header {
                    project: 2,
                    count: 1,
                },
                DrawerRow::Pane(pane(2, 2, "kumu")),
            ]
        );
    }

    #[test]
    fn a_collapsed_project_keeps_a_header_with_its_count() {
        let panes = [pane(1, 1, "kumu"), pane(2, 1, "kumu"), pane(3, 0, "")];
        let rows = drawer_rows(&panes, &[1]);
        assert_eq!(
            rows,
            vec![
                DrawerRow::Header {
                    project: 1,
                    count: 2,
                },
                DrawerRow::Header {
                    project: 0,
                    count: 1,
                },
                DrawerRow::Pane(pane(3, 0, "")),
            ]
        );
    }
}
