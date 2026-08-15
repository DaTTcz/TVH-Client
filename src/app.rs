//! Application UI: top menu (TV / EPG / Nahrávky / Nastavení), channel
//! list (with cached logos, laid out as a 3-column grid) + embedded video
//! playback, and settings sub-tabs (server profiles, update check,
//! about).
//!
//! Note on egui/eframe 0.33+: `eframe::App` no longer has an `update(&self,
//! ctx: &Context, ...)` method - it now has `ui(&mut self, ui: &mut Ui,
//! ...)`, and the old `TopBottomPanel`/`SidePanel` types were unified into
//! a single `egui::Panel` with `Panel::top()`/`::bottom()`/`::left()`/
//! `::right()` constructors, whose `.show()` takes the parent `Ui` instead
//! of the `Context`. Panel order matters: `CentralPanel` must always be
//! added last.
//!
//! Startup behavior: if there's a primary server saved, the app tries to
//! connect immediately (before the first frame is even drawn - see
//! `TvhApp::new`); on success it lands on the TV tab, on failure (or if
//! nothing's saved) it lands on Nastavení > Připojení so the user can fix
//! things.
//!
//! Multiple TVHeadend servers can be saved (`Settings::servers`), one
//! marked primary (auto-connected at startup), each optionally restricted
//! to a subset of channel tags (`ServerProfile::selected_tags`, picked
//! via the "Test" button in the server form). The top bar shows which
//! server is currently connected and doubles as a switcher.
//!
//! Video playback embeds mpv directly into the TV tab's `CentralPanel`
//! via an `egui_glow::CallbackFn` / `egui::PaintCallback` - see
//! `player/mpv.rs` for the render-context/self-referential-struct
//! details. Channel logos are fetched/cached/decoded by `src/logos.rs`
//! and shown as plain egui textures (`TvhApp::logo_textures`).

use crate::epg;
use crate::logos;
use crate::player::MpvPlayer;
use crate::recordings::{self, DownloadControl, DownloadUpdate, RecordingsData};
use crate::settings::{ServerProfile, Settings};
use crate::tvh::{Channel, ChannelTag, DvrEntry, EpgEvent, ServerInfo, TvhClient};
use crate::update;
use eframe::egui;
use eframe::egui_glow::CallbackFn;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the mouse can sit still over the video before the overlay
/// controls (currently just the fullscreen icon) and the cursor itself
/// hide - standard video-player "auto-hide" behavior.
const VIDEO_CONTROLS_IDLE_TIMEOUT: Duration = Duration::from_secs(3);

// EPG grid tab layout - shared between `TvhApp::epg_tab` (rendering) and
// the global keyboard-shortcut handler (Up/Down/Left/Right scroll the
// grid while that tab is active), so they can't drift out of sync.
const EPG_ROW_HEIGHT: f32 = 30.0;
const EPG_HEADER_HEIGHT: f32 = 22.0;
const EPG_PIXELS_PER_MIN: f32 = 3.0;

enum ConnectMsg {
    Success(ServerInfo, Vec<Channel>),
    Error(String),
}

/// Whether the OS window is currently fullscreen. Queried from
/// `ViewportInfo` (set by the windowing backend) each time rather than
/// tracked as our own bool, so it stays correct even if fullscreen gets
/// toggled by something outside our own button (OS shortcut, window
/// manager double-click on the title bar, ...).
fn is_fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

/// Small "Načítání bufferu... NN%" text in the top-left corner of a video
/// area, painted whenever `pct` is `Some` (i.e. mpv reports it's actually
/// stalled waiting on its network cache - see `MpvPlayer::
/// buffering_percent`) - always visible, not gated by hover, so a slow
/// connection looks like "something is happening" instead of looking
/// exactly like a silent failure.
fn draw_buffering_indicator(ui: &egui::Ui, rect: egui::Rect, pct: Option<i64>) {
    let Some(pct) = pct else {
        return;
    };
    ui.painter().text(
        rect.left_top() + egui::vec2(10.0, 8.0),
        egui::Align2::LEFT_TOP,
        format!("Načítání bufferu... {pct}%"),
        egui::FontId::proportional(13.0),
        egui::Color32::WHITE,
    );
}

/// Ovládání hlasitosti (ikona podle úrovně + posuvník 0-100 s vypsaným
/// procentem) - sdílené mezi TV a Nahrávky video overlayem
/// (`draw_video_overlay`/`draw_recording_overlay`). `MpvPlayer::volume`/
/// `set_volume` berou `&self`, takže na tohle stačí jen mpv handle, ne
/// `&mut TvhApp` - vrací `egui::Response`, aby volající mohl po
/// `response.drag_stopped()` uložit novou hodnotu do `Settings::volume`
/// (viz volací místa) bez zapisování na disk při každém tiku tažení.
fn draw_volume_control(ui: &mut egui::Ui, player: &MpvPlayer, slider_width: f32) -> egui::Response {
    let mut volume = player.volume() as f32;
    let icon = if volume <= 0.0 {
        "🔇"
    } else if volume < 50.0 {
        "🔉"
    } else {
        "🔊"
    };
    ui.label(icon);
    // `Slider` v téhle verzi egui nemá builder metodu pro šířku - nastaví
    // se přes `spacing.slider_width` (starší, ale spolehlivější způsob,
    // funguje napříč verzemi).
    ui.spacing_mut().slider_width = slider_width;
    let response = ui.add(
        egui::Slider::new(&mut volume, 0.0..=100.0)
            .suffix("%")
            .fixed_decimals(0),
    );
    if response.changed() {
        player.set_volume(volume as f64);
    }
    response
}

#[derive(PartialEq, Clone, Copy)]
enum TopTab {
    Tv,
    Epg,
    Recordings,
    Settings,
}

#[derive(PartialEq, Clone, Copy)]
enum SettingsTab {
    Connection,
    Downloads,
    UpdateCheck,
    About,
}

#[derive(PartialEq, Clone, Copy)]
enum RecordingsTab {
    Finished,
    Upcoming,
    Autorec,
}

/// Monday(0)..Sunday(6), matching TVHeadend's `weekdays` values (1-7).
const WEEKDAY_LABELS: [&str; 7] = ["Po", "Út", "St", "Čt", "Pá", "So", "Ne"];

/// State for the Nastavení > Kontrola verze tab.
#[derive(Default)]
struct UpdateState {
    checking: bool,
    result: Option<Result<update::ReleaseInfo, String>>,
    rx: Option<Receiver<Result<update::ReleaseInfo, String>>>,

    installing: bool,
    // Only ever gets a message on *failure* - a successful install exits
    // the whole process from the background thread, so there's nothing
    // left to update the UI with.
    install_message: Option<String>,
    install_rx: Option<Receiver<String>>,
}

/// Result of the "Test" button in the server add/edit form.
struct TestOk {
    server_label: String,
    tags: Vec<ChannelTag>,
}

/// The add/edit-server form, when open (`TvhApp::server_edit`).
/// `id: None` means "new profile, not saved yet".
struct ServerEditState {
    id: Option<String>,
    name: String,
    url: String,
    user: String,
    password: String,
    selected_tags: Vec<String>,

    testing: bool,
    test_result: Option<Result<TestOk, String>>,
    test_rx: Option<Receiver<Result<TestOk, String>>>,
}

impl ServerEditState {
    fn blank() -> Self {
        Self {
            id: None,
            name: String::new(),
            url: String::new(),
            user: String::new(),
            password: String::new(),
            selected_tags: Vec::new(),
            testing: false,
            test_result: None,
            test_rx: None,
        }
    }

    fn from_profile(p: &ServerProfile) -> Self {
        Self {
            id: Some(p.id.clone()),
            name: p.name.clone(),
            url: p.url.clone(),
            user: p.user.clone(),
            password: p.password.clone(),
            selected_tags: p.selected_tags.clone(),
            testing: false,
            test_result: None,
            test_rx: None,
        }
    }
}

/// The autorec (recurring recording) edit form, when open
/// (`TvhApp::autorec_edit`). Edits are applied onto a full clone of the
/// original `serde_json::Value` from `TvhClient::autorec_list` and the
/// whole object is round-tripped back through `TvhClient::autorec_save` -
/// `api/idnode/save` replaces fields wholesale rather than merging, so a
/// form that only knew about a handful of fields would silently wipe out
/// everything else the server has stored for this rule (priority,
/// retention, content type, ...). See that method's doc comment.
struct AutorecEditState {
    node: serde_json::Value,
    title: String,
    enabled: bool,
    /// Channel uuid, empty = "any channel".
    channel: String,
    weekdays: [bool; 7],
    saving: bool,
    error: Option<String>,
    rx: Option<Receiver<Result<(), String>>>,
}

impl AutorecEditState {
    fn from_value(value: &serde_json::Value) -> Self {
        let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let enabled = value.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let channel = value.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let weekdays = match value.get("weekdays").and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => {
                let mut days = [false; 7];
                for d in arr {
                    if let Some(n) = d.as_i64() {
                        if (1..=7).contains(&n) {
                            days[(n - 1) as usize] = true;
                        }
                    }
                }
                days
            }
            // No (or empty) weekdays usually means "every day" server-side
            // - reflect that in the form instead of showing every box
            // unchecked, which would look like "never".
            _ => [true; 7],
        };
        Self {
            node: value.clone(),
            title,
            enabled,
            channel,
            weekdays,
            saving: false,
            error: None,
            rx: None,
        }
    }
}

/// The recording currently loaded into mpv from the Nahrávky tab's
/// "Přehrát" button, if any - mutually exclusive with `TvhApp::playing`
/// (a live channel index): there's exactly one embedded mpv instance, so
/// starting one kind of playback always stops/replaces the other (see
/// `TvhApp::select_channel` and `recordings_finished_list`'s Přehrát
/// handler).
#[derive(Clone)]
struct PlayingRecording {
    title: String,
}

/// A recording being downloaded to a local temp file before mpv ever
/// sees it (`TvhApp::recording_buffer`) - see that field's doc comment
/// for why. Reuses the exact same background-download machinery as the
/// Nahrávky tab's "⬇ Stáhnout" button (`recordings::spawn_download`/
/// `DownloadControl`), just writing to a throwaway cache path instead of
/// the user's configured downloads folder.
struct RecordingBuffer {
    path: PathBuf,
    downloaded: u64,
    total: Option<u64>,
    paused: bool,
    // Set once `downloaded` first crosses `RECORDING_START_PLAYBACK_BYTES`
    // (or the file finishes before that, whichever's first) and mpv has
    // been `load()`ed - from then on the download just keeps filling in
    // the rest in the background while playback has already started, see
    // `draw_recording_video`.
    loaded_into_player: bool,
    control: Arc<DownloadControl>,
    rx: Receiver<DownloadUpdate>,
}

/// How much of a recording to have buffered locally before handing it to
/// mpv, rather than waiting for the whole (possibly several-gigabyte)
/// file - enough for reliable MPEG-TS format detection, small enough
/// that playback starts quickly even on a slow connection. The rest
/// keeps downloading in the background afterward.
const RECORDING_START_PLAYBACK_BYTES: u64 = 2 * 1024 * 1024;

/// How long the "Načítání bufferu..." indicator stays up after a
/// recording starts playing, before it hides itself (see
/// `recording_playback_started_at`) - it reappears on its own if mpv
/// later reports a genuine stall (`MpvPlayer::buffering_percent`), so
/// this is just about not permanently cluttering the corner with "still
/// downloading in the background" info nobody needs once playback is
/// smoothly running.
const RECORDING_BUFFER_INDICATOR_DURATION: Duration = Duration::from_secs(3);

/// One recording currently downloading (Nahrávky tab's "⬇ Stáhnout") -
/// keyed by `DvrEntry::uuid` in `TvhApp::downloads`. `downloaded`/`total`
/// are updated from `DownloadUpdate::Progress` messages on `rx`;
/// `control` is the same `Arc` the background thread (`recordings::
/// spawn_download`) is checking, so toggling `paused`/calling `.cancel()`
/// here takes effect on its next chunk-read check.
struct DownloadState {
    downloaded: u64,
    total: Option<u64>,
    paused: bool,
    control: Arc<DownloadControl>,
    rx: Receiver<DownloadUpdate>,
}

pub struct TvhApp {
    top_tab: TopTab,
    settings_tab: SettingsTab,

    settings: Settings,
    // The server whose channels/server_info are currently loaded, if any.
    active_server_id: Option<String>,
    // Set right before a connect attempt starts, consumed by
    // `poll_connect` on success to become `active_server_id`.
    pending_server_id: Option<String>,
    server_edit: Option<ServerEditState>,

    connecting: bool,
    error: Option<String>,
    settings_message: Option<String>,

    server_info: Option<ServerInfo>,
    channels: Vec<Channel>,
    filter: String,
    selected: Option<usize>,

    rx: Option<Receiver<ConnectMsg>>,

    // Channel logos, keyed by `logos::cache_key(&channel.logo_url)`.
    logo_textures: HashMap<String, egui::TextureHandle>,
    logo_rx: Option<Receiver<(String, egui::TextureHandle)>>,

    // EPG events, keyed by channel uuid (`Channel::channel_id`), each
    // channel's list sorted by start time. See `src/epg.rs`.
    epg: HashMap<String, Vec<EpgEvent>>,
    // `(loaded, total)` from the most recent progress update - `None`
    // until the very first message arrives. `loaded >= total` means the
    // fetch (or cache load) is complete. Lets the EPG tab distinguish
    // "haven't started" / "still paging in more" / "done" without ever
    // just spinning forever.
    epg_progress: Option<(usize, usize)>,
    epg_error: Option<String>,
    epg_rx: Option<Receiver<Result<epg::EpgProgress, String>>>,
    // EPG grid tab: the fixed left channel-name column and the scrollable
    // timeline body are two separate `ScrollArea`s (needed so the channel
    // names stay put while you scroll sideways through time) - this is
    // the vertical offset read back from whichever one moved last frame
    // and re-applied to the other, keeping their rows visually aligned.
    epg_grid_scroll_y: f32,
    // Horizontal offset of the timeline, forced into the `ScrollArea`
    // every frame (same reasoning as `epg_grid_scroll_y`) so the
    // Left/Right keyboard shortcuts have something to adjust.
    epg_grid_scroll_x: f32,
    // When true, the next frame overwrites `epg_grid_scroll_x` to bring
    // "now" to the left edge, then clears itself. Set on first EPG data
    // and by the tab's own "Nyní" button.
    epg_center_on_now: bool,

    // `None` if mpv/glow init failed - `player_error` then explains why.
    // Wrapped in `Arc` so the paint-callback closure (which egui requires
    // to be `'static`) can hold its own cheap clone.
    player: Option<Arc<MpvPlayer>>,
    player_error: Option<String>,
    // Which channel index mpv currently has loaded, so we only call
    // `player.load()` when the selection actually changes.
    playing: Option<usize>,
    // Set instead of `playing` when a recording (not a live channel) is
    // loaded from the Nahrávky tab - see `PlayingRecording`.
    playing_recording: Option<PlayingRecording>,
    // Local playback buffer for a recording, from the moment "▶ Přehrát"
    // starts downloading it until the whole file has arrived - `None`
    // once fully downloaded (or nothing's buffering). Root cause this
    // works around: pointing mpv directly at TVHeadend's `dvrfile/<uuid>`
    // URL was observed to just hang - permanently black, no mpv error
    // event, no `paused-for-cache` buffering state either - on at least
    // one real connection, even though the exact same URL downloads fine
    // via our own `reqwest`-based `recordings::spawn_download`. So
    // recordings are instead buffered to a local temp file with that
    // same, already-proven download path, and mpv only ever opens a
    // local file - but (recordings are often several GB) mpv is handed
    // that file as soon as `RECORDING_START_PLAYBACK_BYTES` has arrived,
    // not after the whole thing - the rest keeps downloading in the
    // background while playback has already started (`loaded_into_player`
    // tracks which phase it's in). See `RecordingBuffer` and
    // `draw_recording_video`.
    recording_buffer: Option<RecordingBuffer>,
    // Local temp file mpv is currently playing a recording from (once
    // buffering finished) - kept so it can be deleted once playback
    // stops or switches to something else, see `clear_recording_playback`.
    recording_buffer_path: Option<PathBuf>,
    // When mpv was `load()`ed for the recording currently playing - used
    // to show the "Načítání bufferu..." indicator only briefly (see
    // `RECORDING_BUFFER_INDICATOR_DURATION`) rather than for as long as
    // the background download keeps running, which on a slow connection
    // could otherwise be the entire runtime. `None` once nothing's
    // playing this way (see `clear_recording_playback`).
    recording_playback_started_at: Option<Instant>,
    paused: bool,
    // Deadline until which the video overlay controls + cursor stay
    // visible - pushed forward whenever the mouse moves over the video;
    // `None`/expired means "hidden". See `VIDEO_CONTROLS_IDLE_TIMEOUT`.
    video_controls_until: Option<Instant>,
    // Deadline until which the overlay is forced visible after a
    // keyboard volume change (+/-, arrow keys), *regardless* of mouse
    // position - without this, changing volume by keyboard while the
    // mouse isn't sitting over the video would silently do nothing
    // visible (see `adjust_volume`). OR'd into the same `show_controls`
    // checks that use `video_controls_until`.
    volume_osd_until: Option<Instant>,
    // Current mpv playback error (bad URL, unsupported format, network
    // timeout, ...), if any - see `MpvPlayer::poll_errors`. Shown as a
    // banner over the video instead of a silently-black area; cleared
    // automatically once a later `load()` starts cleanly.
    player_playback_error: Option<String>,

    update: UpdateState,

    // App icon, decoded once at startup for the "O programu" tab -
    // reuses the same PNG embedded as the window/exe icon (`main.rs`).
    about_logo: Option<egui::TextureHandle>,

    // ---- recordings (Nahrávky tab) ------------------------------------
    recordings_tab: RecordingsTab,
    recordings: Option<RecordingsData>,
    recordings_loading: bool,
    recordings_error: Option<String>,
    recordings_rx: Option<Receiver<Result<RecordingsData, String>>>,
    // Transient feedback ("Staženo do ...", an error, ...) shown at the
    // top of the tab until the next action replaces or dismisses it.
    recordings_message: Option<String>,
    // Recordings currently downloading, keyed by `DvrEntry::uuid`, each
    // with its own background thread + receiver so several downloads can
    // run at once - see `DownloadState`.
    downloads: HashMap<String, DownloadState>,
    // Result of the most recent Zrušit/Smazat/autorec-Smazat action -
    // shared across all three since they're all "fire and refresh the
    // list", nothing else needs the specific outcome.
    action_rx: Option<Receiver<Result<(), String>>>,
    autorec_edit: Option<AutorecEditState>,
}

impl TvhApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let settings = Settings::load();
        let primary = settings.primary().cloned();

        let (player, player_error) = match MpvPlayer::new(cc) {
            Ok(p) => (Some(Arc::new(p)), None),
            Err(e) => (None, Some(e)),
        };
        // Restore the volume the user left it at last time (see
        // `Settings::volume`) - mpv itself always starts fresh at 100.
        if let Some(player) = &player {
            player.set_volume(settings.volume);
        }

        let about_logo = logos::decode_and_load(
            &cc.egui_ctx,
            "about_logo",
            include_bytes!("../assets/icon-128.png"),
        );

        let mut app = Self {
            top_tab: TopTab::Tv,
            settings_tab: SettingsTab::Connection,
            settings,
            active_server_id: None,
            pending_server_id: None,
            server_edit: None,
            connecting: false,
            error: None,
            settings_message: None,
            server_info: None,
            channels: Vec::new(),
            filter: String::new(),
            selected: None,
            rx: None,
            logo_textures: HashMap::new(),
            logo_rx: None,
            epg: HashMap::new(),
            epg_progress: None,
            epg_error: None,
            epg_rx: None,
            epg_grid_scroll_y: 0.0,
            epg_grid_scroll_x: 0.0,
            epg_center_on_now: true,
            player,
            player_error,
            playing: None,
            playing_recording: None,
            recording_buffer: None,
            recording_buffer_path: None,
            recording_playback_started_at: None,
            paused: false,
            video_controls_until: None,
            volume_osd_until: None,
            player_playback_error: None,
            update: UpdateState::default(),
            about_logo,
            recordings_tab: RecordingsTab::Finished,
            recordings: None,
            recordings_loading: false,
            recordings_error: None,
            recordings_rx: None,
            recordings_message: None,
            downloads: HashMap::new(),
            action_rx: None,
            autorec_edit: None,
        };

        if let Some(server) = primary {
            app.start_connect(cc.egui_ctx.clone(), server);
        } else {
            app.top_tab = TopTab::Settings;
        }

        // Automatická kontrola nové verze na pozadí - nic se sama od sebe
        // nestahuje/neinstaluje, jen naplní `self.update.result`, které si
        // pak přečte jak Nastavení > Kontrola verze (viz `settings_update_tab`),
        // tak novinkový odznak v `menu_bar` - viz `poll_update_check`.
        app.start_update_check(cc.egui_ctx.clone());

        app
    }

    // ---- connect ----------------------------------------------------

    fn start_connect(&mut self, ctx: egui::Context, server: ServerProfile) {
        self.connecting = true;
        self.error = None;
        self.pending_server_id = Some(server.id.clone());

        // Drop whatever the previously-connected server had loaded, so we
        // don't show stale channels/logos while switching or retrying.
        // Dropping `logo_rx` also makes any still-running logo-sync
        // thread for the old server stop on its next send (its `tx`'s
        // matching `rx` is gone).
        self.stop_playback();
        self.channels.clear();
        self.server_info = None;
        self.selected = None;
        self.logo_textures.clear();
        self.logo_rx = None;
        self.epg.clear();
        self.epg_progress = None;
        self.epg_error = None;
        self.epg_rx = None;

        let (tx, rx): (Sender<ConnectMsg>, Receiver<ConnectMsg>) = std::sync::mpsc::channel();
        self.rx = Some(rx);

        std::thread::spawn(move || {
            let result = (|| -> Result<(ServerInfo, Vec<Channel>), String> {
                let client = TvhClient::new(&server.url, &server.user, &server.password)
                    .map_err(|e| e.to_string())?;
                let info = client.server_info().map_err(|e| e.to_string())?;
                let channels = client
                    .channels_for_tags(&server.selected_tags)
                    .map_err(|e| e.to_string())?;
                Ok((info, channels))
            })();

            let msg = match result {
                Ok((info, channels)) => ConnectMsg::Success(info, channels),
                Err(e) => ConnectMsg::Error(e),
            };
            // Ignore send errors: if the receiver is gone the app is
            // shutting down / the user navigated away.
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn poll_connect(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else {
            return;
        };
        match rx.try_recv() {
            Ok(ConnectMsg::Success(info, channels)) => {
                self.server_info = Some(info);
                self.channels = channels;
                self.connecting = false;
                self.rx = None;
                self.active_server_id = self.pending_server_id.take();
                self.top_tab = TopTab::Tv;

                if let Some(server_id) = self.active_server_id.clone() {
                    let pairs: Vec<(String, String)> = self
                        .channels
                        .iter()
                        .filter_map(|ch| {
                            logos::cache_key(&ch.logo_url).map(|key| (key, ch.logo_url.clone()))
                        })
                        .collect();
                    if !pairs.is_empty() {
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.logo_rx = Some(rx);
                        logos::spawn_logo_sync(ctx.clone(), server_id.clone(), pairs, tx);
                    }

                    if let Some(server) =
                        self.settings.servers.iter().find(|s| s.id == server_id).cloned()
                    {
                        let (tx, rx) = std::sync::mpsc::channel();
                        self.epg_rx = Some(rx);
                        epg::spawn_epg_sync(
                            ctx.clone(),
                            server_id,
                            server.url,
                            server.user,
                            server.password,
                            tx,
                        );
                    }
                }
            }
            Ok(ConnectMsg::Error(e)) => {
                self.error = Some(e);
                self.connecting = false;
                self.rx = None;
                self.pending_server_id = None;
                // Send the user somewhere they can fix the problem.
                self.top_tab = TopTab::Settings;
                self.settings_tab = SettingsTab::Connection;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.connecting = false;
                self.rx = None;
                self.pending_server_id = None;
            }
        }
    }

    fn poll_logos(&mut self) {
        let Some(rx) = &self.logo_rx else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok((key, handle)) => {
                    self.logo_textures.insert(key, handle);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.logo_rx = None;
                    break;
                }
            }
        }
    }

    fn poll_epg(&mut self) {
        let Some(rx) = &self.epg_rx else {
            return;
        };
        match rx.try_recv() {
            // One message per page (each already the full accumulated
            // list so far, not just the new page) - just replace what we
            // had each time, progress keeps growing towards `total`.
            Ok(Ok(progress)) => {
                self.epg = epg::group_by_channel(progress.events);
                self.epg_progress = Some((progress.loaded, progress.total));
                self.epg_error = None;
            }
            Ok(Err(e)) => {
                // Keep whatever's already in `self.epg` (e.g. a cached
                // copy, or however many pages made it through before
                // this one failed) - only the "still loading" spinner
                // should ever be replaced by this.
                if self.epg_progress.is_none() {
                    self.epg_progress = Some((0, 0));
                }
                self.epg_error = Some(e);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.epg_rx = None;
            }
        }
    }

    /// Manual retry for the "Obnovit" button in the EPG tab - re-runs the
    /// same background sync as after connecting, for whichever server is
    /// currently active.
    fn start_epg_refresh(&mut self, ctx: egui::Context) {
        let Some(server) = self
            .active_server_id
            .as_ref()
            .and_then(|id| self.settings.servers.iter().find(|s| &s.id == id))
            .cloned()
        else {
            return;
        };
        self.epg_progress = None;
        self.epg_error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.epg_rx = Some(rx);
        epg::spawn_epg_sync(ctx, server.id, server.url, server.user, server.password, tx);
    }

    // ---- recordings (Nahrávky tab) ------------------------------------

    /// (Re)fetches everything the Nahrávky tab shows - triggered lazily on
    /// first visit to the tab, by its own "Obnovit" button, and after any
    /// action (cancel/delete/autorec save) that should be reflected in the
    /// lists right away.
    fn start_recordings_refresh(&mut self, ctx: egui::Context) {
        let Some(server) = self
            .active_server_id
            .as_ref()
            .and_then(|id| self.settings.servers.iter().find(|s| &s.id == id))
            .cloned()
        else {
            return;
        };
        self.recordings_loading = true;
        self.recordings_error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.recordings_rx = Some(rx);
        recordings::spawn_fetch(ctx, server.url, server.user, server.password, tx);
    }

    fn poll_recordings(&mut self) {
        if let Some(rx) = &self.recordings_rx {
            match rx.try_recv() {
                Ok(Ok(data)) => {
                    self.recordings = Some(data);
                    self.recordings_loading = false;
                    self.recordings_error = None;
                    self.recordings_rx = None;
                }
                Ok(Err(e)) => {
                    self.recordings_loading = false;
                    self.recordings_error = Some(e);
                    self.recordings_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.recordings_loading = false;
                    self.recordings_rx = None;
                }
            }
        }

        // Background downloads - one receiver per in-flight recording.
        // Drained in a loop (not just one `try_recv` per frame) since
        // `Progress` messages can queue up faster than frames render on
        // a fast local-network transfer.
        let mut done = Vec::new();
        let mut messages = Vec::new();
        for (uuid, state) in self.downloads.iter_mut() {
            loop {
                match state.rx.try_recv() {
                    Ok(DownloadUpdate::Progress { downloaded, total }) => {
                        state.downloaded = downloaded;
                        state.total = total;
                    }
                    Ok(DownloadUpdate::Done(path)) => {
                        messages.push(format!("Staženo do {}", path.display()));
                        recordings::open_in_file_manager(&path);
                        done.push(uuid.clone());
                        break;
                    }
                    Ok(DownloadUpdate::Cancelled) => {
                        messages.push("Stahování zrušeno.".to_string());
                        done.push(uuid.clone());
                        break;
                    }
                    Ok(DownloadUpdate::Error(e)) => {
                        messages.push(format!("Stahování selhalo: {e}"));
                        done.push(uuid.clone());
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        done.push(uuid.clone());
                        break;
                    }
                }
            }
        }
        if let Some(msg) = messages.pop() {
            self.recordings_message = Some(msg);
        }
        for uuid in done {
            self.downloads.remove(&uuid);
        }

        // Local playback buffer for "▶ Přehrát" (see `recording_buffer`
        // doc comment) - mpv gets `load()`ed as soon as
        // `RECORDING_START_PLAYBACK_BYTES` has arrived (`start_playback`),
        // not only once the whole (possibly several-gigabyte) file is
        // done; the download keeps running in the background either way
        // until `Done`. `buffer_finished` carries along whether playback
        // had already started, so an error/cancel *after* that point
        // (background fill interrupted) doesn't yank away something the
        // user is already watching - only a failure before that point
        // does.
        let mut start_playback: Option<PathBuf> = None;
        let mut buffer_finished: Option<(PathBuf, bool, Result<(), String>)> = None;
        if let Some(buf) = &mut self.recording_buffer {
            loop {
                match buf.rx.try_recv() {
                    Ok(DownloadUpdate::Progress { downloaded, total }) => {
                        buf.downloaded = downloaded;
                        buf.total = total;
                        if !buf.loaded_into_player && downloaded >= RECORDING_START_PLAYBACK_BYTES {
                            buf.loaded_into_player = true;
                            start_playback = Some(buf.path.clone());
                        }
                    }
                    Ok(DownloadUpdate::Done(_)) => {
                        if !buf.loaded_into_player {
                            // A recording shorter than the threshold -
                            // finished before ever crossing it.
                            buf.loaded_into_player = true;
                            start_playback = Some(buf.path.clone());
                        }
                        buffer_finished = Some((buf.path.clone(), true, Ok(())));
                        break;
                    }
                    Ok(DownloadUpdate::Cancelled) => {
                        buffer_finished =
                            Some((buf.path.clone(), buf.loaded_into_player, Err("Zrušeno.".to_string())));
                        break;
                    }
                    Ok(DownloadUpdate::Error(e)) => {
                        buffer_finished = Some((buf.path.clone(), buf.loaded_into_player, Err(e)));
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        buffer_finished = Some((
                            buf.path.clone(),
                            buf.loaded_into_player,
                            Err("Spojení s vláknem bylo přerušeno.".to_string()),
                        ));
                        break;
                    }
                }
            }
        }
        if let Some(path) = start_playback {
            if let Some(player) = &self.player {
                match player.load(&path.to_string_lossy()) {
                    Ok(()) => self.recording_playback_started_at = Some(Instant::now()),
                    Err(e) => self.player_playback_error = Some(e),
                }
            }
        }
        if let Some((path, was_playing, result)) = buffer_finished {
            self.recording_buffer = None;
            match result {
                Ok(()) => {
                    self.recording_buffer_path = Some(path);
                }
                Err(e) => {
                    if was_playing {
                        // Already watching the buffered part - report it
                        // but don't yank playback away. `spawn_download`
                        // does try to delete its temp file even on this
                        // kind of error; on Windows that quietly fails
                        // while mpv still has the file open for reading,
                        // so what's already buffered normally keeps
                        // playing regardless (not a guarantee, just how
                        // NTFS file locking tends to behave here).
                        self.recordings_message =
                            Some(format!("Dostahování na pozadí přerušeno: {e}"));
                        self.recording_buffer_path = Some(path);
                    } else {
                        // `spawn_download` already deletes the partial/
                        // cancelled temp file itself here - no separate
                        // cleanup needed.
                        self.recordings_message = Some(format!("Načtení nahrávky selhalo: {e}"));
                        self.playing_recording = None;
                    }
                }
            }
        }

        // Zrušit/Smazat (upcoming cancel, finished/failed remove, autorec
        // delete) - all share `action_rx` since they only need a
        // success/failure signal before reloading the lists.
        if let Some(rx) = &self.action_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.recordings = None; // triggers a reload, see `recordings_tab`
                    self.action_rx = None;
                }
                Ok(Err(e)) => {
                    self.recordings_message = Some(format!("Akce selhala: {e}"));
                    self.action_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.action_rx = None;
                }
            }
        }

        // Autorec edit-form save.
        let mut autorec_done: Option<Result<(), String>> = None;
        if let Some(state) = &self.autorec_edit {
            if let Some(rx) = &state.rx {
                match rx.try_recv() {
                    Ok(result) => autorec_done = Some(result),
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        autorec_done = Some(Err("Spojení s vláknem bylo přerušeno.".to_string()));
                    }
                }
            }
        }
        if let Some(result) = autorec_done {
            match result {
                Ok(()) => {
                    self.autorec_edit = None;
                    self.recordings_message = Some("Pravidlo bylo uloženo.".to_string());
                    self.recordings = None; // triggers a reload
                }
                Err(e) => {
                    if let Some(state) = &mut self.autorec_edit {
                        state.saving = false;
                        state.error = Some(e);
                        state.rx = None;
                    }
                }
            }
        }
    }

    /// Fires a quick write-only DVR action (cancel/remove/autorec-delete)
    /// on a background thread against the currently active server, then
    /// (via `poll_recordings` clearing `self.recordings`) reloads the
    /// whole Nahrávky tab once it succeeds - avoids blocking the UI thread
    /// on the network round trip for a single button click.
    fn spawn_recording_action(
        &mut self,
        ctx: egui::Context,
        action: impl FnOnce(&TvhClient) -> Result<(), crate::tvh::TvhError> + Send + 'static,
    ) {
        let Some(server) = self
            .active_server_id
            .as_ref()
            .and_then(|id| self.settings.servers.iter().find(|s| &s.id == id))
            .cloned()
        else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.action_rx = Some(rx);
        std::thread::spawn(move || {
            let result = TvhClient::new(&server.url, &server.user, &server.password)
                .map_err(|e| e.to_string())
                .and_then(|client| action(&client).map_err(|e| e.to_string()));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    /// Refreshes `player_playback_error` from mpv's own event queue - see
    /// `MpvPlayer::poll_errors` for why this is needed at all (an async
    /// load failure otherwise has nowhere to surface). Always overwrites
    /// with the *current* state, so it clears itself back to `None` a
    /// frame or two after a later, successful `load()`.
    fn poll_player_events(&mut self) {
        self.player_playback_error = self.player.as_ref().and_then(|p| p.poll_errors());
    }

    /// Select a channel and (if mpv is available) start streaming it -
    /// also stops/replaces any recording currently playing from the
    /// Nahrávky tab, since there's only the one embedded mpv instance.
    fn select_channel(&mut self, i: usize) {
        self.selected = Some(i);
        let Some(ch) = self.channels.get(i).cloned() else {
            return;
        };
        let Some(player) = self.player.clone() else {
            return;
        };
        match player.load(&ch.stream_url) {
            Ok(()) => {
                self.playing = Some(i);
                self.playing_recording = None;
                self.paused = false;
                self.clear_recording_playback();
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Stops a recording started from the Nahrávky tab's "Přehrát" button
    /// - the "✕ Zavřít"/"✕ Zrušit" buttons in `draw_recording_overlay`
    /// and the buffering box in `draw_recording_video`. Unlike the TV
    /// tab (whose video area, and thus its fullscreen-shrink button,
    /// stays on screen regardless of whether a channel is playing), the
    /// Nahrávky tab's whole video area only exists while
    /// `playing_recording` is `Some` - so without this, stopping while
    /// fullscreen would leave the window stuck fullscreen with no way
    /// back to it in the UI. If the window is currently fullscreen, this
    /// always exits fullscreen too, on the assumption that "stop" means
    /// "I'm done watching this", not "shrink the window but keep
    /// everything else about this cinema session".
    fn stop_recording_playback(&mut self, ctx: &egui::Context) {
        if let Some(player) = &self.player {
            let _ = player.stop();
        }
        self.playing_recording = None;
        self.paused = false;
        self.clear_recording_playback();
        if is_fullscreen(ctx) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    /// Cancels a recording's local playback buffer if one's still
    /// downloading, and deletes its temp file whether it finished or
    /// not - shared cleanup between starting a different recording,
    /// switching to a live channel, and "✕ Zavřít". See
    /// `recording_buffer`/`recording_buffer_path`.
    fn clear_recording_playback(&mut self) {
        if let Some(buf) = self.recording_buffer.take() {
            buf.control.cancel();
        }
        if let Some(path) = self.recording_buffer_path.take() {
            let _ = std::fs::remove_file(path);
        }
        self.recording_playback_started_at = None;
    }

    fn stop_playback(&mut self) {
        if let Some(player) = &self.player {
            let _ = player.stop();
        }
        self.playing = None;
        self.paused = false;
    }

    /// Move the selection by `delta` positions in `self.channels`
    /// (wrapping around at either end) and play it - the "Další"/
    /// "Předchozí" buttons and Page Down/Page Up shortcuts.
    fn select_relative_channel(&mut self, delta: isize) {
        if self.channels.is_empty() {
            return;
        }
        let len = self.channels.len() as isize;
        let current = self.selected.unwrap_or(0) as isize;
        let next = ((current + delta) % len + len) % len;
        self.select_channel(next as usize);
    }

    /// Nudges mpv's volume (0-100 scale) by `delta` - no-op if mpv isn't
    /// available. Also forces the video overlay (which now shows the
    /// volume slider, see `draw_volume_control`) briefly visible via
    /// `volume_osd_until`, so a keyboard volume change is actually visible
    /// even when the mouse isn't sitting over the video.
    fn adjust_volume(&mut self, delta: f64) {
        if let Some(player) = &self.player {
            let current = player.volume();
            player.set_volume(current + delta);
            self.volume_osd_until = Some(Instant::now() + VIDEO_CONTROLS_IDLE_TIMEOUT);
            // Persisted so the next launch restores it - see
            // `Settings::volume`. One keypress = one save, unlike
            // dragging the overlay's slider (see `draw_volume_control`'s
            // call sites), which only save once the drag ends.
            self.settings.volume = player.volume();
            let _ = self.settings.save();
        }
    }

    // ---- server test (Test button in the edit form) ------------------

    fn start_server_test(&mut self, ctx: egui::Context) {
        let Some(edit) = self.server_edit.as_mut() else {
            return;
        };
        edit.testing = true;
        edit.test_result = None;

        let (tx, rx) = std::sync::mpsc::channel();
        edit.test_rx = Some(rx);

        let url = edit.url.clone();
        let user = edit.user.clone();
        let password = edit.password.clone();

        std::thread::spawn(move || {
            let result = (|| -> Result<TestOk, String> {
                let client = TvhClient::new(&url, &user, &password).map_err(|e| e.to_string())?;
                let info = client.server_info().map_err(|e| e.to_string())?;
                // Best-effort: if the channel-tags endpoint doesn't exist
                // on this server (or answers unexpectedly), just show no
                // tags instead of failing the whole test - see
                // `ChannelTag` docs in tvh/mod.rs.
                let tags = client.channel_tags().unwrap_or_default();
                let name = info.name.unwrap_or_else(|| "TVHeadend".to_string());
                let version = info.sw_version.unwrap_or_default();
                Ok(TestOk {
                    server_label: format!("{name} {version}"),
                    tags,
                })
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn poll_server_test(&mut self) {
        let Some(edit) = self.server_edit.as_mut() else {
            return;
        };
        let Some(rx) = &edit.test_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                edit.testing = false;
                edit.test_result = Some(result);
                edit.test_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                edit.testing = false;
                edit.test_rx = None;
            }
        }
    }

    // ---- update check / install --------------------------------------

    fn start_update_check(&mut self, ctx: egui::Context) {
        self.update.checking = true;
        self.update.result = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.update.rx = Some(rx);
        std::thread::spawn(move || {
            let result = update::check_latest();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn poll_update_check(&mut self) {
        let Some(rx) = &self.update.rx else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.update.checking = false;
                self.update.result = Some(result);
                self.update.rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.update.checking = false;
                self.update.rx = None;
            }
        }
    }

    fn start_update_install(&mut self, ctx: egui::Context, info: update::ReleaseInfo) {
        self.update.installing = true;
        self.update.install_message = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.update.install_rx = Some(rx);
        std::thread::spawn(move || {
            // On success this never returns - it exits the whole process
            // itself once the update helper is safely launched. Only the
            // failure path ever sends anything back.
            if let Err(e) = update::download_and_apply(&info.download_url) {
                let _ = tx.send(e);
                ctx.request_repaint();
            }
        });
    }

    fn poll_update_install(&mut self) {
        let Some(rx) = &self.update.install_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(e) => {
                self.update.installing = false;
                self.update.install_message = Some(e);
                self.update.install_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.update.installing = false;
                self.update.install_rx = None;
            }
        }
    }

    // ---- top menu ------------------------------------------------------

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menu_bar").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.top_tab, TopTab::Tv, "📺 TV");
                ui.selectable_value(&mut self.top_tab, TopTab::Epg, "🗓 EPG");
                ui.selectable_value(&mut self.top_tab, TopTab::Recordings, "⏺ Nahrávky");
                ui.selectable_value(&mut self.top_tab, TopTab::Settings, "⚙ Nastavení");

                ui.separator();

                if self.connecting {
                    ui.spinner();
                    ui.label("Připojuji se...");
                } else if self.settings.servers.is_empty() {
                    ui.colored_label(egui::Color32::GRAY, "Nepřipojeno");
                } else {
                    let current_label = self
                        .active_server_id
                        .as_ref()
                        .and_then(|id| self.settings.servers.iter().find(|s| &s.id == id))
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| "Nepřipojeno".to_string());

                    let servers = self.settings.servers.clone();
                    egui::ComboBox::from_id_salt("server_switch")
                        .selected_text(current_label)
                        .show_ui(ui, |ui| {
                            for server in servers {
                                let is_active =
                                    self.active_server_id.as_deref() == Some(server.id.as_str());
                                if ui.selectable_label(is_active, &server.name).clicked() && !is_active {
                                    let ctx = ui.ctx().clone();
                                    self.start_connect(ctx, server);
                                }
                            }
                        });
                }

                // Odznak nové verze - vyplní ho tichá kontrola na pozadí
                // spuštěná při startu appky (viz `TvhApp::new`), nebo
                // ruční "Zkontrolovat aktualizace" v Nastavení. Klik
                // přepne rovnou na tu záložku.
                if let Some(Ok(info)) = &self.update.result {
                    if info.is_newer {
                        ui.separator();
                        if ui
                            .button(format!("🆕 Nová verze {} k dispozici", info.version))
                            .clicked()
                        {
                            self.top_tab = TopTab::Settings;
                            self.settings_tab = SettingsTab::UpdateCheck;
                        }
                    }
                }
            });

            if self.top_tab == TopTab::Settings {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Connection, "Připojení");
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Downloads, "Stahování");
                    ui.selectable_value(
                        &mut self.settings_tab,
                        SettingsTab::UpdateCheck,
                        "Kontrola verze",
                    );
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::About, "O programu");
                });
            }
            ui.add_space(2.0);
        });
    }

    // ---- TV tab ----------------------------------------------------

    fn tv_tab(&mut self, ui: &mut egui::Ui) {
        let fullscreen = is_fullscreen(ui.ctx());

        if self.channels.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    if self.connecting {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Připojuji se...");
                        });
                    } else {
                        ui.label("Nejsi připojený - přejdi do Nastavení > Připojení.");
                    }
                });
            });
            return;
        }

        // Cinema mode: fullscreen shows just the video (+ its own shrink
        // icon, below) - no channel list, no now-playing bar.
        if !fullscreen {
        egui::Panel::left("channel_list")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Hledat:");
                    ui.text_edit_singleline(&mut self.filter);
                });
                ui.label(format!("{} kanálů", self.channels.len()));
                ui.separator();

                let filter = self.filter.to_lowercase();
                let now = epg::now_unix();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("channel_grid")
                            .num_columns(3)
                            .spacing([8.0, 4.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for i in 0..self.channels.len() {
                                    let ch = &self.channels[i];
                                    if !filter.is_empty()
                                        && !ch.name.to_lowercase().contains(&filter)
                                        && !ch.number.to_lowercase().contains(&filter)
                                    {
                                        continue;
                                    }
                                    let selected = self.selected == Some(i);
                                    let row_id = ui.id().with(("channel_row", i));

                                    let number_resp = if selected {
                                        ui.strong(ch.number.clone())
                                    } else {
                                        ui.label(ch.number.clone())
                                    };

                                    let logo_key = logos::cache_key(&ch.logo_url);
                                    let texture =
                                        logo_key.as_ref().and_then(|k| self.logo_textures.get(k));
                                    let image_resp = if let Some(tex) = texture {
                                        ui.add(
                                            egui::Image::new(tex).max_height(20.0).max_width(32.0),
                                        )
                                    } else {
                                        ui.allocate_exact_size(
                                            egui::vec2(32.0, 20.0),
                                            egui::Sense::hover(),
                                        )
                                        .1
                                    };

                                    // Name cell also carries what's currently
                                    // playing (title + a thin progress bar) when
                                    // EPG data is available for this channel.
                                    let name_resp = ui
                                        .vertical(|ui| {
                                            if selected {
                                                ui.strong(ch.name.clone());
                                            } else {
                                                ui.label(ch.name.clone());
                                            }
                                            if let Some(events) = self.epg.get(&ch.channel_id) {
                                                let (current, next) =
                                                    epg::current_and_next(events, now);
                                                if let Some(ev) = current {
                                                    let epg_resp = ui
                                                        .vertical(|ui| {
                                                            ui.colored_label(
                                                                egui::Color32::GRAY,
                                                                ev.title.clone(),
                                                            );
                                                            ui.add(
                                                                egui::ProgressBar::new(
                                                                    epg::progress_fraction(
                                                                        ev, now,
                                                                    ),
                                                                )
                                                                .desired_height(4.0),
                                                            );
                                                        })
                                                        .response;

                                                    // Hover for the full picture: exact
                                                    // start-stop, the longer EPG
                                                    // description (if the server sent
                                                    // one - not every source provides
                                                    // it), and what's on next.
                                                    //
                                                    // Deliberately *not* `.on_hover_ui`:
                                                    // the whole-row click interact zone
                                                    // below is registered after this
                                                    // widget and covers the same area,
                                                    // so it wins egui's hover
                                                    // arbitration and `epg_resp` never
                                                    // reports itself as hovered (same
                                                    // root cause as the cursor-icon fix
                                                    // above). `rect_contains_pointer` is
                                                    // order-independent, so gate the
                                                    // always-show `show_tooltip_ui` on
                                                    // that instead.
                                                    if ui.rect_contains_pointer(epg_resp.rect) {
                                                        epg_resp.show_tooltip_ui(|ui| {
                                                        ui.set_max_width(320.0);
                                                        ui.strong(ev.title.clone());
                                                        ui.label(format!(
                                                            "{}–{}",
                                                            epg::format_time(ev.start),
                                                            epg::format_time(ev.stop)
                                                        ));
                                                        // `summary` alone is often
                                                        // hard-truncated mid-sentence
                                                        // by the broadcaster (DVB's
                                                        // short-event-descriptor length
                                                        // limit) - `epg::synopsis`
                                                        // glues on the continuation
                                                        // from `description` and stops
                                                        // before the junk tags/second
                                                        // synopsis some sources
                                                        // append. See its doc comment.
                                                        let desc = epg::synopsis(ev);
                                                        if let Some(desc) = desc {
                                                            ui.add_space(4.0);
                                                            ui.label(desc);
                                                        }
                                                        if let Some(next_ev) = next {
                                                            ui.add_space(6.0);
                                                            ui.separator();
                                                            ui.label(
                                                                egui::RichText::new("Další")
                                                                    .strong(),
                                                            );
                                                            ui.label(format!(
                                                                "{}  ({}–{})",
                                                                next_ev.title,
                                                                epg::format_time(
                                                                    next_ev.start
                                                                ),
                                                                epg::format_time(
                                                                    next_ev.stop
                                                                )
                                                            ));
                                                        }
                                                        });
                                                    }
                                                }
                                            }
                                        })
                                        .response;

                                    // Extend to the full row width (not just the union of
                                    // the three cells) so the whitespace after the name
                                    // column is clickable/hoverable too - a "whole row"
                                    // should mean the whole row, not just its content.
                                    let mut row_rect = number_resp
                                        .rect
                                        .union(image_resp.rect)
                                        .union(name_resp.rect);
                                    row_rect.max.x = ui.max_rect().right();

                                    // `rect_contains_pointer` (unlike a widget's own
                                    // `.hovered()`) doesn't depend on interaction-order
                                    // arbitration with the individual cell widgets above,
                                    // so forcing the cursor from it here is what actually
                                    // makes the whole row show one consistent cursor
                                    // instead of flickering between pointer/arrow as the
                                    // mouse crosses cell boundaries.
                                    if ui.rect_contains_pointer(row_rect) {
                                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    }
                                    let row_response =
                                        ui.interact(row_rect, row_id, egui::Sense::click());
                                    let clicked = row_response.clicked();
                                    // TODO(EPG): `row_response.secondary_clicked()` is the
                                    // hook point for a future right-click context menu
                                    // (e.g. "show EPG for this channel").

                                    ui.end_row();

                                    if clicked {
                                        self.select_channel(i);
                                    }
                                }
                            });
                    });
            });
        } // !fullscreen
          //
          // No separate "now playing" bar below the video anymore - all
          // of that (channel/logo/name, EPG now+next+progress, play/
          // pause/stop/prev/next) lives in the video's own hover overlay
          // now, see `draw_video_overlay`. Redundant to show it twice as
          // plain text underneath as well.

        egui::CentralPanel::default().show(ui, |ui| {
            let available = ui.available_size();
            let (rect, _response) = ui.allocate_exact_size(available, egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);

            // Auto-hide the overlay controls (and the cursor itself)
            // after a few seconds of the mouse sitting still over the
            // video - standard video-player behavior. Any movement while
            // over the video pushes the deadline forward again.
            let pointer_in_video = ui.rect_contains_pointer(rect);
            let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
            if pointer_in_video && pointer_moved {
                self.video_controls_until = Some(Instant::now() + VIDEO_CONTROLS_IDLE_TIMEOUT);
            }
            let volume_osd_active = self
                .volume_osd_until
                .is_some_and(|deadline| Instant::now() < deadline);
            let show_controls = volume_osd_active
                || (pointer_in_video
                    && self
                        .video_controls_until
                        .is_some_and(|deadline| Instant::now() < deadline));
            if pointer_in_video {
                if !show_controls {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                }
            }
            if pointer_in_video || volume_osd_active {
                // Keep re-checking the deadline even with no video
                // playing (which would otherwise not repaint on its
                // own once the pointer stops generating input events) -
                // also needed while `volume_osd_active` so the overlay a
                // keyboard volume change forced open actually hides
                // itself again once its deadline passes.
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }

            let have_video = self.player.is_some() && self.playing.is_some();
            if have_video {
                // Read before the `.clone()` below moves a copy into the
                // paint callback closure.
                let buffering_pct = self.player.as_ref().and_then(|p| p.buffering_percent());
                let player = self.player.clone().unwrap();
                let callback = egui::PaintCallback {
                    rect,
                    callback: Arc::new(CallbackFn::new(move |info, _painter| {
                        // Bottom-left-origin rect, matching what mpv's
                        // render/blit needs - see player/mpv.rs docs for
                        // why we can't just render straight into whatever
                        // framebuffer is currently bound.
                        let vp = info.viewport_in_pixels();
                        player.render(vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px);
                    })),
                };
                ui.painter().add(callback);
                // Keep redrawing while a channel is loaded so mpv's
                // render() gets called roughly every frame (~60 FPS). A
                // future pass could instead react to mpv's own update
                // callback for exact frame-driven repaints.
                ui.ctx().request_repaint_after(Duration::from_millis(16));
                draw_buffering_indicator(ui, rect, buffering_pct);
            } else {
                let text = if self.player.is_none() {
                    "Přehrávání videa není dostupné."
                } else {
                    "Vyber kanál vlevo."
                };
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(16.0),
                    egui::Color32::GRAY,
                );
            }

            // mpv playback error (bad stream, network problem, ...) -
            // painted on top of whatever's above so a failure is never
            // just a silent black rectangle. See `MpvPlayer::poll_errors`.
            if let Some(err) = &self.player_playback_error {
                ui.painter()
                    .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(190));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("Přehrávání selhalo:\n{err}"),
                    egui::FontId::proportional(16.0),
                    egui::Color32::from_rgb(230, 110, 110),
                );
            }

            // Overlay controls (fullscreen toggle + the info panel) -
            // only while `show_controls` (hovering + recently moved, see
            // above) - added last so they paint on top of the video
            // callback above.
            if show_controls {
                self.draw_video_overlay(ui, rect, fullscreen);
            }
        });
    }

    /// Everything drawn on top of the video while `show_controls` is
    /// true: the fullscreen toggle (top-right) and the info panel
    /// (bottom) - channel number/logo/name, current program + progress,
    /// what's on next, and the play/pause/stop/prev/next-channel
    /// buttons. This replaces having any of that as plain text in a
    /// separate bar below the video.
    fn draw_video_overlay(&mut self, ui: &mut egui::Ui, rect: egui::Rect, fullscreen: bool) {
        // Fullscreen toggle - top-right corner.
        {
            let icon = if fullscreen { "🗗" } else { "⛶" };
            let tooltip = if fullscreen { "Zmenšit" } else { "Celá obrazovka" };
            let margin = 8.0;
            let button_size = egui::vec2(28.0, 28.0);
            let button_rect = egui::Rect::from_min_size(
                egui::pos2(rect.right() - margin - button_size.x, rect.top() + margin),
                button_size,
            );
            if ui
                .put(button_rect, egui::Button::new(icon))
                .on_hover_text(tooltip)
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
            }
        }

        let Some(i) = self.selected else {
            return;
        };
        let Some(ch) = self.channels.get(i).cloned() else {
            return;
        };
        let is_playing_this = self.playing == Some(i);
        let paused = self.paused;
        let now = epg::now_unix();
        // Owned copy, not a borrow of `self.epg` - keeps the borrow from
        // living into the button-click handling below, which needs
        // `&mut self`.
        let events = self.epg.get(&ch.channel_id).cloned();
        let (current, next) = events
            .as_deref()
            .map(|events| epg::current_and_next(events, now))
            .unwrap_or((None, None));

        // See `epg::synopsis` doc comment: glues `summary`'s often
        // mid-sentence-truncated text onto the start of `description`,
        // stopping before the junk tags / second synopsis some sources
        // append there. No length cap here - the panel below sizes
        // itself to fit the whole thing, wrapped over as many lines as
        // it needs.
        let description = current.and_then(epg::synopsis);

        const SYNOPSIS_FONT: f32 = 14.0;

        let margin = 12.0;
        // Left-aligned (not centered) since a channel with a long name/
        // synopsis needs more room than a short one - centering would
        // make the panel visibly jump left/right as you switch channels.
        let panel_width = (rect.width() - margin * 2.0).min(560.0);
        let content_width = panel_width - 20.0; // minus the 10px inset on each side

        // Estimate how many lines the synopsis will wrap to at
        // `content_width`, so the panel can be sized to fit it fully
        // instead of guessing one fixed height. Rough character-width
        // heuristic rather than an exact text-layout measurement (egui's
        // precise-measurement API isn't worth chasing here) - rounded up
        // and padded a bit on both axes so an off-by-a-line estimate
        // still comfortably fits inside the painted background.
        let synopsis_extra_height = description.as_ref().map_or(0.0, |d| {
            let avg_char_width = SYNOPSIS_FONT * 0.5;
            let chars_per_line = (content_width / avg_char_width).floor().max(1.0);
            let num_lines = (d.chars().count() as f32 / chars_per_line).ceil().max(1.0);
            let line_height = SYNOPSIS_FONT * 1.4;
            // +4 for the spacer line between the title and the synopsis,
            // +10 general padding/safety margin.
            num_lines * line_height + 14.0
        });
        // Řádek s hlasitostí navíc, jen pokud vůbec máme mpv handle, co
        // ovládat (`player_for_volume` - vlastní `Arc` klon, ať closure
        // níž nemusí znovu sahat do `self.player` a řešit borrow).
        let player_for_volume = self.player.clone();
        const VOLUME_ROW_HEIGHT: f32 = 30.0;
        let volume_extra = if player_for_volume.is_some() { VOLUME_ROW_HEIGHT } else { 0.0 };
        let panel_height = if current.is_some() {
            118.0 + synopsis_extra_height + volume_extra
        } else {
            74.0 + volume_extra
        };
        let panel_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + margin, rect.bottom() - margin - panel_height),
            egui::vec2(panel_width, panel_height),
        );
        ui.painter()
            .rect_filled(panel_rect, 8.0, egui::Color32::from_black_alpha(200));

        let mut go_next = false;
        let mut go_prev = false;
        let mut toggle_pause = false;
        let mut do_stop = false;

        ui.scope_builder(egui::UiBuilder::new().max_rect(panel_rect), |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                let logo_key = logos::cache_key(&ch.logo_url);
                if let Some(tex) = logo_key.as_ref().and_then(|k| self.logo_textures.get(k)) {
                    ui.add(egui::Image::new(tex).max_height(28.0).max_width(44.0));
                }
                ui.colored_label(
                    egui::Color32::WHITE,
                    egui::RichText::new(format!("{}  {}", ch.number, ch.name))
                        .size(16.0)
                        .strong(),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    if ui.button("Další ▶").on_hover_text("Další kanál (Page Down)").clicked() {
                        go_next = true;
                    }
                    if ui
                        .button("◀ Předchozí")
                        .on_hover_text("Předchozí kanál (Page Up)")
                        .clicked()
                    {
                        go_prev = true;
                    }
                    if ui.button("⏹").on_hover_text("Zastavit").clicked() {
                        do_stop = true;
                    }
                    if is_playing_this {
                        let label = if paused { "▶" } else { "⏸" };
                        if ui.button(label).on_hover_text("Pauza").clicked() {
                            toggle_pause = true;
                        }
                    }
                });
            });

            if let Some(player) = &player_for_volume {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    // Uložit až po puštění tažení, ne při každém `changed()`
                    // tiku (viz `draw_volume_control` doc) - jinak by
                    // jedno přetažení posuvníku zapsalo settings.json
                    // desetkrát za sekundu.
                    if draw_volume_control(ui, player, panel_width - 90.0).drag_stopped() {
                        self.settings.volume = player.volume();
                        let _ = self.settings.save();
                    }
                });
            }

            ui.add_space(4.0);
            match current {
                Some(ev) => {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.colored_label(
                            egui::Color32::WHITE,
                            egui::RichText::new(format!(
                                "{}  ({}–{})",
                                ev.title,
                                epg::format_time(ev.start),
                                epg::format_time(ev.stop)
                            ))
                            .size(15.0)
                            .strong(),
                        );
                    });
                    if let Some(desc) = &description {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            // A `horizontal` layout never wraps text (its
                            // main axis is horizontal, so a label inside
                            // it just keeps extending sideways) - nest a
                            // `vertical` sub-Ui so the label wraps at
                            // `content_width` like `panel_height` above
                            // assumed it would.
                            ui.vertical(|ui| {
                                ui.set_max_width(content_width);
                                ui.add(egui::Label::new(
                                    egui::RichText::new(desc.clone())
                                        .color(egui::Color32::LIGHT_GRAY)
                                        .size(SYNOPSIS_FONT),
                                ));
                            });
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.add(
                            egui::ProgressBar::new(epg::progress_fraction(ev, now))
                                .desired_width(panel_width - 20.0)
                                .show_percentage(),
                        );
                    });
                    if let Some(ev) = next {
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.colored_label(
                                egui::Color32::LIGHT_GRAY,
                                format!(
                                    "Další: {}  ({}–{})",
                                    ev.title,
                                    epg::format_time(ev.start),
                                    epg::format_time(ev.stop)
                                ),
                            );
                        });
                    }
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.add_space(10.0);
                        ui.colored_label(egui::Color32::GRAY, "Žádná EPG data pro tento kanál.");
                    });
                }
            }
        });

        if go_next {
            self.select_relative_channel(1);
        }
        if go_prev {
            self.select_relative_channel(-1);
        }
        if toggle_pause {
            self.paused = !self.paused;
            if let Some(player) = &self.player {
                player.set_paused(self.paused);
            }
        }
        if do_stop {
            self.stop_playback();
        }
    }

    /// Video area for a recording playing from the Nahrávky tab - the
    /// same building blocks as the TV tab's video panel (mpv
    /// `PaintCallback`, fullscreen toggle), just without the
    /// channel/EPG-specific info a recording doesn't have. Outside
    /// fullscreen the panel is already a small, deliberately-opened area
    /// (only shown while something's playing - see `recordings_tab`), so
    /// its controls stay always visible instead of the TV tab's
    /// hover-to-reveal/auto-hide-cursor dance, which only kicks in once
    /// fullscreen makes the video take up the whole window.
    fn draw_recording_video(&mut self, ui: &mut egui::Ui, rect: egui::Rect, fullscreen: bool) {
        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);

        // Still filling the *initial* local playback buffer (below
        // `RECORDING_START_PLAYBACK_BYTES`) - show real, known-accurate
        // progress (same idea as the Nahrávky list's own download
        // progress bar) instead of anything video-shaped; mpv hasn't
        // been given anything to play yet. Once past that point,
        // `loaded_into_player` is set and this falls through to normal
        // video rendering below - the rest keeps downloading in the
        // background (see the small indicator inside the `have_video`
        // branch further down). See `recording_buffer`.
        let still_filling_initial_buffer =
            self.recording_buffer.as_ref().is_some_and(|b| !b.loaded_into_player);
        if still_filling_initial_buffer {
            let buf = self.recording_buffer.as_ref().expect("checked above");
            let downloaded = buf.downloaded;
            let total = buf.total;
            let paused = buf.paused;
            let fraction = total
                .filter(|&t| t > 0)
                .map(|t| (downloaded as f32 / t as f32).clamp(0.0, 1.0));
            let text = match total {
                Some(t) => format!(
                    "{} / {}",
                    recordings::human_size(downloaded as i64),
                    recordings::human_size(t as i64)
                ),
                None => recordings::human_size(downloaded as i64),
            };
            let box_width = (rect.width() - 40.0).min(300.0);
            let box_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(box_width, 92.0));
            let mut toggle_pause = false;
            let mut cancel = false;
            ui.scope_builder(egui::UiBuilder::new().max_rect(box_rect), |ui| {
                ui.vertical_centered(|ui| {
                    ui.colored_label(egui::Color32::WHITE, "Ukládám do vyrovnávací paměti...");
                    ui.add_space(6.0);
                    ui.add(
                        egui::ProgressBar::new(fraction.unwrap_or(0.0))
                            .desired_width(box_width)
                            .text(text),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let label = if paused { "▶ Pokračovat" } else { "⏸ Pauza" };
                        if ui.small_button(label).clicked() {
                            toggle_pause = true;
                        }
                        if ui.small_button("✕ Zrušit").clicked() {
                            cancel = true;
                        }
                    });
                });
            });
            if toggle_pause {
                if let Some(buf) = &mut self.recording_buffer {
                    buf.paused = !buf.paused;
                    buf.control.set_paused(buf.paused);
                }
            }
            if cancel {
                self.stop_recording_playback(ui.ctx());
            }
            return;
        }

        let show_controls = if fullscreen {
            let pointer_in_video = ui.rect_contains_pointer(rect);
            let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
            if pointer_in_video && pointer_moved {
                self.video_controls_until = Some(Instant::now() + VIDEO_CONTROLS_IDLE_TIMEOUT);
            }
            let volume_osd_active = self
                .volume_osd_until
                .is_some_and(|deadline| Instant::now() < deadline);
            let show = volume_osd_active
                || (pointer_in_video
                    && self
                        .video_controls_until
                        .is_some_and(|deadline| Instant::now() < deadline));
            if pointer_in_video {
                if !show {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::None);
                }
            }
            if pointer_in_video || volume_osd_active {
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            show
        } else {
            true
        };

        let have_video = self.player.is_some() && self.playing_recording.is_some();
        if have_video {
            // While the background download (past the initial threshold,
            // see `RECORDING_START_PLAYBACK_BYTES`) is still filling in
            // the rest of the file, show *our own* accurate percentage
            // rather than mpv's - once mpv is reading a local file, its
            // own `paused-for-cache` essentially never fires the way it
            // does for a real network stream. Read before the `.clone()`
            // below moves a copy into the paint callback closure.
            //
            // Our own background-fill percentage is only shown briefly
            // after playback starts (`RECORDING_BUFFER_INDICATOR_DURATION`)
            // - past that it'd otherwise just sit there for as long as
            // the background download keeps running, which on a slow
            // connection could be the whole runtime. A genuine mpv stall
            // (`buffering_percent`) always takes priority and always
            // shows, however - that's an actual "playback caught up to
            // what's downloaded" signal, not just informational.
            let own_fill_pct = self.recording_buffer.as_ref().and_then(|b| {
                b.total
                    .filter(|&t| t > 0)
                    .map(|t| ((b.downloaded as f64 / t as f64) * 100.0).clamp(0.0, 100.0) as i64)
            });
            let recently_started = self
                .recording_playback_started_at
                .is_some_and(|t| t.elapsed() < RECORDING_BUFFER_INDICATOR_DURATION);
            let buffering_pct = self
                .player
                .as_ref()
                .and_then(|p| p.buffering_percent())
                .or_else(|| own_fill_pct.filter(|_| recently_started));
            let player = self.player.clone().unwrap();
            let callback = egui::PaintCallback {
                rect,
                callback: Arc::new(CallbackFn::new(move |info, _painter| {
                    let vp = info.viewport_in_pixels();
                    player.render(vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px);
                })),
            };
            ui.painter().add(callback);
            // Keep redrawing while a recording is loaded, same reasoning
            // as the TV tab's video panel.
            ui.ctx().request_repaint_after(Duration::from_millis(16));
            draw_buffering_indicator(ui, rect, buffering_pct);
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Přehrávání videa není dostupné.",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
        }

        // mpv playback error (bad URL, unsupported format, network
        // problem, ...) - see `MpvPlayer::poll_errors`.
        if let Some(err) = &self.player_playback_error {
            ui.painter()
                .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(190));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("Přehrávání selhalo:\n{err}"),
                egui::FontId::proportional(16.0),
                egui::Color32::from_rgb(230, 110, 110),
            );
        }

        if show_controls {
            self.draw_recording_overlay(ui, rect, fullscreen);
        }
    }

    /// Fullscreen toggle (top-right) + a small bottom bar with the
    /// recording's title, pause/resume and "✕ Zavřít" (stop) - the
    /// Nahrávky-tab equivalent of `draw_video_overlay`, minus everything
    /// that only makes sense for a live channel (EPG now/next, prev/next
    /// channel).
    fn draw_recording_overlay(&mut self, ui: &mut egui::Ui, rect: egui::Rect, fullscreen: bool) {
        {
            let icon = if fullscreen { "🗗" } else { "⛶" };
            let tooltip = if fullscreen { "Zmenšit" } else { "Celá obrazovka" };
            let margin = 8.0;
            let button_size = egui::vec2(28.0, 28.0);
            let button_rect = egui::Rect::from_min_size(
                egui::pos2(rect.right() - margin - button_size.x, rect.top() + margin),
                button_size,
            );
            if ui
                .put(button_rect, egui::Button::new(icon))
                .on_hover_text(tooltip)
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
            }
        }

        let Some(rec) = self.playing_recording.clone() else {
            return;
        };
        let paused = self.paused;

        let player_for_volume = self.player.clone();
        const VOLUME_ROW_HEIGHT: f32 = 30.0;
        let margin = 12.0;
        let panel_height = 46.0 + if player_for_volume.is_some() { VOLUME_ROW_HEIGHT } else { 0.0 };
        let panel_width = (rect.width() - margin * 2.0).min(560.0);
        let panel_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left() + margin, rect.bottom() - margin - panel_height),
            egui::vec2(panel_width, panel_height),
        );
        ui.painter()
            .rect_filled(panel_rect, 8.0, egui::Color32::from_black_alpha(200));

        let mut toggle_pause = false;
        let mut do_stop = false;

        ui.scope_builder(egui::UiBuilder::new().max_rect(panel_rect), |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.colored_label(
                    egui::Color32::WHITE,
                    egui::RichText::new(rec.title.clone()).size(15.0).strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(10.0);
                    if ui.button("✕ Zavřít").on_hover_text("Zastavit přehrávání").clicked() {
                        do_stop = true;
                    }
                    let label = if paused { "▶" } else { "⏸" };
                    if ui.button(label).on_hover_text("Pauza").clicked() {
                        toggle_pause = true;
                    }
                });
            });

            if let Some(player) = &player_for_volume {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    // Uložit až po puštění tažení, ne při každém `changed()`
                    // tiku (viz `draw_volume_control` doc) - jinak by
                    // jedno přetažení posuvníku zapsalo settings.json
                    // desetkrát za sekundu.
                    if draw_volume_control(ui, player, panel_width - 90.0).drag_stopped() {
                        self.settings.volume = player.volume();
                        let _ = self.settings.save();
                    }
                });
            }
        });

        if toggle_pause {
            self.paused = !self.paused;
            if let Some(player) = &self.player {
                player.set_paused(self.paused);
            }
        }
        if do_stop {
            self.stop_recording_playback(ui.ctx());
        }
    }

    // ---- EPG tab ------------------------------------------------------

    /// Classic TV-guide grid: channels down the left (fixed column, always
    /// visible), time across the top (hour ruler, scrolls horizontally),
    /// programme blocks positioned/sized by their actual start/stop time.
    /// See `epg_grid_scroll_y`/`epg_center_on_now` doc comments for how the
    /// two independently-scrolled panes (channel names + timeline body)
    /// stay vertically in sync.
    fn epg_tab(&mut self, ui: &mut egui::Ui) {
        if self.channels.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label("Nejsi připojený - přejdi do Nastavení > Připojení.");
                });
            });
            return;
        }
        if self.epg.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    if let Some(err) = self.epg_error.clone() {
                        ui.vertical_centered(|ui| {
                            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                            ui.add_space(8.0);
                            if ui.button("Obnovit").clicked() {
                                let ctx = ui.ctx().clone();
                                self.start_epg_refresh(ctx);
                            }
                        });
                    } else if self.epg_progress.is_some_and(|(loaded, total)| loaded >= total) {
                        ui.label("Server nevrátil žádná EPG data.");
                    } else {
                        let label = match self.epg_progress {
                            Some((loaded, total)) if total > 0 => {
                                format!("Načítám EPG... ({loaded} z {total})")
                            }
                            _ => "Načítám EPG... (u velkých instalací TVHeadend to může chvíli trvat)"
                                .to_string(),
                        };
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(label);
                        });
                    }
                });
            });
            return;
        }

        // Toolbar - always visible once there's data to show.
        egui::Panel::top("epg_toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Hledat kanál:");
                ui.text_edit_singleline(&mut self.filter);
                ui.separator();
                if ui.button("Nyní").on_hover_text("Posunout mřížku na aktuální čas").clicked() {
                    self.epg_center_on_now = true;
                }
                ui.separator();
                ui.label(format!("{} kanálů", self.channels.len()));
            });
        });

        // Progress/error banners, only when there's something to say -
        // own panel so they don't push the grid around when they appear/
        // disappear mid-session.
        let show_progress = self.epg_progress.is_some_and(|(loaded, total)| total > 0 && loaded < total);
        let epg_error = self.epg_error.clone();
        if show_progress || epg_error.is_some() {
            egui::Panel::top("epg_status").show(ui, |ui| {
                if show_progress {
                    if let Some((loaded, total)) = self.epg_progress {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Načítám další data EPG... ({loaded} z {total})"));
                        });
                    }
                }
                if let Some(err) = &epg_error {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(200, 140, 40),
                            format!("Poslední obnovení EPG selhalo ({err}) - zobrazuji starší data."),
                        );
                        if ui.button("Obnovit").clicked() {
                            let ctx = ui.ctx().clone();
                            self.start_epg_refresh(ctx);
                        }
                    });
                }
            });
        }

        if let Some(msg) = self.recordings_message.clone() {
            egui::Panel::top("epg_action_message").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(msg);
                    if ui.small_button("✕").clicked() {
                        self.recordings_message = None;
                    }
                });
            });
        }

        let filter_lower = self.filter.to_lowercase();
        let channels: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|c| filter_lower.is_empty() || c.name.to_lowercase().contains(&filter_lower))
            .collect();
        if channels.is_empty() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label("Žádný kanál neodpovídá hledání.");
                });
            });
            return;
        }

        // Row height/header height/zoom - module-level constants (shared
        // with the Up/Down/Left/Right keyboard scroll handler).
        const ROW_HEIGHT: f32 = EPG_ROW_HEIGHT;
        const HEADER_HEIGHT: f32 = EPG_HEADER_HEIGHT;
        const PIXELS_PER_MIN: f32 = EPG_PIXELS_PER_MIN;

        let now = epg::now_unix();

        // Visible time span: from the earliest loaded event to the latest
        // across every (unfiltered - so it doesn't jump around as you
        // search) channel, so the ruler/scroll range covers everything
        // that's actually been fetched. Falls back to a 3h window if
        // there's simply no data yet for anyone.
        let mut timeline_start = now;
        let mut timeline_end = now + 3 * 3600;
        for events in self.epg.values() {
            if let Some(first) = events.first() {
                timeline_start = timeline_start.min(first.start);
            }
            if let Some(last) = events.last() {
                timeline_end = timeline_end.max(last.stop);
            }
        }
        let timeline_width =
            ((timeline_end - timeline_start).max(60) as f32 / 60.0) * PIXELS_PER_MIN;

        // +1 fake trailing row - without it the last real channel sits
        // flush against the bottom edge (and the horizontal scrollbar on
        // the grid side), which reads as visually cut off.
        let row_count = channels.len() + 1;

        egui::Panel::left("epg_channels")
            .resizable(true)
            .default_size(190.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                // Spacer so row 0 lines up with the grid body, which has
                // the hour ruler above it.
                ui.add_space(HEADER_HEIGHT);
                let output = egui::ScrollArea::vertical()
                    .id_salt("epg_channels_vscroll")
                    .auto_shrink([false, false])
                    .vertical_scroll_offset(self.epg_grid_scroll_y)
                    .show_rows(ui, ROW_HEIGHT, row_count, |ui, row_range| {
                        for i in row_range {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), ROW_HEIGHT),
                                egui::Sense::hover(),
                            );
                            // Fake trailing row (`i == channels.len()`) -
                            // just reserves the space, nothing to draw.
                            let Some(ch) = channels.get(i) else {
                                continue;
                            };
                            ui.painter().text(
                                rect.left_center() + egui::vec2(4.0, 0.0),
                                egui::Align2::LEFT_CENTER,
                                format!("{}  {}", ch.number, ch.name),
                                egui::FontId::proportional(13.0),
                                ui.visuals().text_color(),
                            );
                        }
                    });
                if (output.state.offset.y - self.epg_grid_scroll_y).abs() > 0.5 {
                    self.epg_grid_scroll_y = output.state.offset.y;
                }
            });

        // Set from inside the (deeply nested) grid-drawing closures below
        // by the right-click context menu, and acted on afterwards, once
        // those closures - and their borrow of `self.epg` - have gone out
        // of scope (mirrors the "capture, then act" pattern used in
        // `poll_recordings` for the same disjoint-borrow reason).
        let mut epg_record_action: Option<i64> = None;
        let mut epg_autorec_action: Option<(String, String)> = None;

        egui::CentralPanel::default().show(ui, |ui| {
            if self.epg_center_on_now {
                self.epg_grid_scroll_x =
                    ((now - timeline_start) as f32 / 60.0 * PIXELS_PER_MIN - 60.0).max(0.0);
                self.epg_center_on_now = false;
            }
            let h_output = egui::ScrollArea::horizontal()
                .id_salt("epg_timeline_hscroll")
                .auto_shrink([false, false])
                .horizontal_scroll_offset(self.epg_grid_scroll_x)
                .show(ui, |ui| {
                ui.set_width(timeline_width);
                ui.spacing_mut().item_spacing.y = 0.0;

                // Hour ruler - lives inside the horizontal scroll area
                // (so it slides sideways with the grid) but *outside* the
                // vertical one below (so it stays put while you scroll
                // through channels).
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(timeline_width, HEADER_HEIGHT),
                    egui::Sense::hover(),
                );
                let painter = ui.painter();
                let mut t = timeline_start - timeline_start.rem_euclid(3600);
                while t < timeline_end {
                    let x = header_rect.left() + (t - timeline_start) as f32 / 60.0 * PIXELS_PER_MIN;
                    painter.line_segment(
                        [egui::pos2(x, header_rect.top()), egui::pos2(x, header_rect.bottom())],
                        egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
                    );
                    painter.text(
                        egui::pos2(x + 3.0, header_rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        epg::format_time(t),
                        egui::FontId::proportional(12.0),
                        egui::Color32::LIGHT_GRAY,
                    );
                    t += 3600;
                }

                let output = egui::ScrollArea::vertical()
                    .id_salt("epg_timeline_vscroll")
                    .auto_shrink([false, false])
                    .vertical_scroll_offset(self.epg_grid_scroll_y)
                    .show_rows(ui, ROW_HEIGHT, row_count, |ui, row_range| {
                        for i in row_range {
                            let (row_rect, _) = ui.allocate_exact_size(
                                egui::vec2(timeline_width, ROW_HEIGHT),
                                egui::Sense::hover(),
                            );
                            if i % 2 == 1 {
                                ui.painter().rect_filled(
                                    row_rect,
                                    0.0,
                                    egui::Color32::from_gray(28),
                                );
                            }

                            // Fake trailing row (`i == channels.len()`) -
                            // just reserves the space, nothing to draw.
                            let Some(ch) = channels.get(i) else {
                                continue;
                            };
                            let Some(events) = self.epg.get(&ch.channel_id) else {
                                continue;
                            };
                            for ev in events {
                                if ev.stop <= timeline_start || ev.start >= timeline_end {
                                    continue;
                                }
                                let start_x = row_rect.left()
                                    + (ev.start - timeline_start).max(0) as f32 / 60.0
                                        * PIXELS_PER_MIN;
                                let end_x = row_rect.left()
                                    + (ev.stop - timeline_start).min(timeline_end - timeline_start)
                                        as f32
                                        / 60.0
                                        * PIXELS_PER_MIN;
                                let block_rect = egui::Rect::from_min_max(
                                    egui::pos2(start_x + 1.0, row_rect.top() + 1.0),
                                    egui::pos2((end_x - 1.0).max(start_x + 2.0), row_rect.bottom() - 1.0),
                                );
                                let is_current = ev.start <= now && now < ev.stop;
                                let fill = if is_current {
                                    egui::Color32::from_rgb(30, 90, 140)
                                } else {
                                    egui::Color32::from_gray(50)
                                };
                                ui.painter().rect_filled(block_rect, 3.0, fill);
                                ui.painter().with_clip_rect(block_rect).text(
                                    block_rect.left_center() + egui::vec2(4.0, 0.0),
                                    egui::Align2::LEFT_CENTER,
                                    ev.title.clone(),
                                    egui::FontId::proportional(12.0),
                                    egui::Color32::WHITE,
                                );

                                // `click()` (not just `hover()`) so the
                                // block also picks up right-clicks for the
                                // context menu below - hover behaviour is
                                // unaffected, it doesn't depend on Sense.
                                let resp = ui.interact(
                                    block_rect,
                                    ui.id().with((ch.channel_id.as_str(), ev.event_id)),
                                    egui::Sense::click(),
                                );
                                let resp = if let Some(desc) = epg::synopsis(ev) {
                                    resp.on_hover_ui(|ui| {
                                        ui.set_max_width(360.0);
                                        ui.strong(ev.title.clone());
                                        ui.label(format!(
                                            "{}–{}",
                                            epg::format_time(ev.start),
                                            epg::format_time(ev.stop)
                                        ));
                                        ui.add_space(4.0);
                                        ui.label(desc);
                                    })
                                } else {
                                    resp.on_hover_text(format!(
                                        "{}–{}  {}",
                                        epg::format_time(ev.start),
                                        epg::format_time(ev.stop),
                                        ev.title
                                    ))
                                };

                                // Right-click menu: schedule a one-time or
                                // recurring recording of this programme.
                                // Only sets the local `epg_*_action`
                                // captures here - the actual
                                // `spawn_recording_action` call happens
                                // after this whole grid closure returns
                                // (see below `CentralPanel::show`), since
                                // it needs `&mut self` while `events` here
                                // still holds a borrow of `self.epg`.
                                resp.context_menu(|ui| {
                                    ui.set_min_width(200.0);
                                    ui.label(egui::RichText::new(ev.title.clone()).strong());
                                    ui.separator();
                                    if ui.button("⏺ Nahrát").clicked() {
                                        epg_record_action = Some(ev.event_id);
                                        ui.close();
                                    }
                                    if ui.button("🔁 Nahrávat opakovaně").clicked() {
                                        epg_autorec_action =
                                            Some((ev.title.clone(), ch.channel_id.clone()));
                                        ui.close();
                                    }
                                });
                            }
                        }
                    });
                if (output.state.offset.y - self.epg_grid_scroll_y).abs() > 0.5 {
                    self.epg_grid_scroll_y = output.state.offset.y;
                }

                // "now" marker, drawn last so it's on top of everything.
                let now_x =
                    header_rect.left() + (now - timeline_start) as f32 / 60.0 * PIXELS_PER_MIN;
                ui.painter().line_segment(
                    [
                        egui::pos2(now_x, header_rect.top()),
                        egui::pos2(now_x, header_rect.top() + HEADER_HEIGHT + output.inner_rect.height()),
                    ],
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(220, 60, 60)),
                );
            });
            if (h_output.state.offset.x - self.epg_grid_scroll_x).abs() > 0.5 {
                self.epg_grid_scroll_x = h_output.state.offset.x;
            }
        });

        // Now that the grid closures above (and their borrow of
        // `self.epg`) are done, actually fire the action the context menu
        // asked for. Both share `spawn_recording_action`/`action_rx` with
        // the Nahrávky tab's cancel/remove/delete buttons - success just
        // reloads `self.recordings` (harmless/no-op if the Nahrávky tab
        // hasn't been opened yet this session), failure shows up as
        // "Akce selhala: ..." wherever that message is displayed (both
        // tabs show `self.recordings_message`).
        if let Some(event_id) = epg_record_action {
            let ctx = ui.ctx().clone();
            self.spawn_recording_action(ctx, move |client| {
                let config_uuid = client.dvr_default_config_uuid()?;
                client.dvr_record_event(event_id, &config_uuid)
            });
            self.recordings_message = Some("Nahrávání bylo naplánováno.".to_string());
        }
        if let Some((title, channel_uuid)) = epg_autorec_action {
            let ctx = ui.ctx().clone();
            self.spawn_recording_action(ctx, move |client| {
                let config_uuid = client.dvr_default_config_uuid()?;
                client.dvr_autorec_create_for_title(&title, &channel_uuid, &config_uuid)
            });
            self.recordings_message = Some("Opakující se nahrávání bylo vytvořeno.".to_string());
        }
    }

    // ---- Nahrávky (recordings) -----------------------------------------

    /// Nahrané pořady (Play/Stáhnout/Smazat), Plánované (Zrušit) a
    /// Opakující se (Upravit/Smazat) - three sub-tabs sharing one fetch
    /// (`self.recordings`, see `start_recordings_refresh`).
    fn recordings_tab(&mut self, ui: &mut egui::Ui) {
        let fullscreen = is_fullscreen(ui.ctx()) && self.playing_recording.is_some();

        if self.active_server_id.is_none() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label("Nejsi připojený - přejdi do Nastavení > Připojení.");
                });
            });
            return;
        }

        if self.recordings.is_none() && !self.recordings_loading && self.recordings_rx.is_none() {
            let ctx = ui.ctx().clone();
            self.start_recordings_refresh(ctx);
        }

        // Cinema mode: fullscreen while a recording is playing shows just
        // the video (+ its own shrink icon), same as the TV tab - no
        // toolbar, lists, or messages underneath.
        if !fullscreen {
            egui::Panel::top("recordings_toolbar").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.recordings_tab, RecordingsTab::Finished, "Nahrané");
                    ui.selectable_value(&mut self.recordings_tab, RecordingsTab::Upcoming, "Plánované");
                    ui.selectable_value(&mut self.recordings_tab, RecordingsTab::Autorec, "Opakující se");
                    ui.separator();
                    if ui.button("Obnovit").clicked() {
                        let ctx = ui.ctx().clone();
                        self.start_recordings_refresh(ctx);
                    }
                    if self.recordings_loading {
                        ui.spinner();
                    }
                });
            });

            if let Some(err) = self.recordings_error.clone() {
                egui::Panel::top("recordings_error").show(ui, |ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 60, 60),
                        format!("Načtení nahrávek selhalo: {err}"),
                    );
                });
            }
            if let Some(msg) = self.recordings_message.clone() {
                egui::Panel::top("recordings_message").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(msg);
                        if ui.small_button("✕").clicked() {
                            self.recordings_message = None;
                        }
                    });
                });
            }
        }

        // Video window - only takes up space at all while a recording is
        // actually playing (started via a "▶ Přehrát" button below);
        // docked and modestly sized (resizable) in the normal view,
        // fullscreen while in cinema mode. Same building blocks as the TV
        // tab's video panel (auto-hiding hover controls in fullscreen,
        // fullscreen toggle, mpv `PaintCallback`) - see
        // `draw_recording_video`.
        if self.playing_recording.is_some() {
            if fullscreen {
                egui::CentralPanel::default().show(ui, |ui| {
                    let available = ui.available_size();
                    let (rect, _resp) = ui.allocate_exact_size(available, egui::Sense::hover());
                    self.draw_recording_video(ui, rect, fullscreen);
                });
                return;
            }
            egui::Panel::top("recordings_video").resizable(true).default_size(260.0).show(
                ui,
                |ui| {
                    let available = ui.available_size();
                    let (rect, _resp) = ui.allocate_exact_size(available, egui::Sense::hover());
                    self.draw_recording_video(ui, rect, fullscreen);
                },
            );
        }

        let Some(data) = self.recordings.clone() else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Načítám nahrávky...");
                    });
                });
            });
            return;
        };

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                match self.recordings_tab {
                    RecordingsTab::Finished => self.recordings_finished_list(ui, &data),
                    RecordingsTab::Upcoming => self.recordings_upcoming_list(ui, &data),
                    RecordingsTab::Autorec => self.recordings_autorec_list(ui, &data),
                }
            });
        });

        if let Some(mut state) = self.autorec_edit.take() {
            let mut keep_open = true;
            egui::Window::new("Upravit opakující se nahrávání")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    keep_open = self.autorec_edit_form(ui, &mut state);
                });
            if keep_open {
                self.autorec_edit = Some(state);
            }
        }
    }

    fn recordings_finished_list(&mut self, ui: &mut egui::Ui, data: &RecordingsData) {
        let mut entries: Vec<(&DvrEntry, bool)> = data
            .finished
            .iter()
            .map(|e| (e, false))
            .chain(data.failed.iter().map(|e| (e, true)))
            .collect();
        entries.sort_by(|a, b| b.0.start.cmp(&a.0.start));
        if entries.is_empty() {
            ui.label("Žádné nahrané pořady.");
            return;
        }
        for (entry, failed) in entries {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.strong(entry.disp_title.clone());
                        if !entry.disp_subtitle.is_empty() {
                            ui.label(entry.disp_subtitle.clone());
                        }
                        ui.label(format!(
                            "{}  •  {}–{}  •  {}",
                            entry.channelname,
                            epg::format_time(entry.start),
                            epg::format_time(entry.stop),
                            recordings::human_size(entry.filesize),
                        ));
                        if failed {
                            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), entry.status.clone());
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("🗑 Smazat").clicked() {
                            let ctx = ui.ctx().clone();
                            let uuid = entry.uuid.clone();
                            self.spawn_recording_action(ctx, move |client| client.dvr_remove(&uuid));
                        }
                        let url = data.urls.get(&entry.uuid).cloned();
                        let progress = self
                            .downloads
                            .get(&entry.uuid)
                            .map(|s| (s.downloaded, s.total, s.paused));
                        if let Some((downloaded, total, paused)) = progress {
                            let fraction = total
                                .filter(|&t| t > 0)
                                .map(|t| (downloaded as f32 / t as f32).clamp(0.0, 1.0));
                            let text = match total {
                                Some(t) => format!(
                                    "{} / {}",
                                    recordings::human_size(downloaded as i64),
                                    recordings::human_size(t as i64)
                                ),
                                None => recordings::human_size(downloaded as i64),
                            };
                            let mut toggle_pause = false;
                            let mut cancel = false;
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::ProgressBar::new(fraction.unwrap_or(0.0))
                                        .desired_width(160.0)
                                        .text(text),
                                );
                                ui.horizontal(|ui| {
                                    let label = if paused { "▶ Pokračovat" } else { "⏸ Pauza" };
                                    if ui.small_button(label).clicked() {
                                        toggle_pause = true;
                                    }
                                    if ui.small_button("✕ Zrušit").clicked() {
                                        cancel = true;
                                    }
                                });
                            });
                            if toggle_pause {
                                if let Some(state) = self.downloads.get_mut(&entry.uuid) {
                                    state.paused = !state.paused;
                                    state.control.set_paused(state.paused);
                                }
                            }
                            if cancel {
                                if let Some(state) = self.downloads.get(&entry.uuid) {
                                    state.control.cancel();
                                }
                            }
                        } else if let Some(url) = &url {
                            if ui.button("⬇ Stáhnout").clicked() {
                                let folder = self.settings.downloads_dir.trim().to_string();
                                if folder.is_empty() {
                                    // Not configured yet - send the user
                                    // to set it instead of asking every
                                    // time (see `settings_downloads_tab`).
                                    self.top_tab = TopTab::Settings;
                                    self.settings_tab = SettingsTab::Downloads;
                                    self.settings_message =
                                        Some("Nejprve nastav složku pro stahování nahrávek.".to_string());
                                } else {
                                    let dest = PathBuf::from(&folder).join(recordings::safe_filename(
                                        &entry.disp_title,
                                        &entry.filename,
                                    ));
                                    let control = DownloadControl::new();
                                    let (tx, rx) = std::sync::mpsc::channel();
                                    recordings::spawn_download(
                                        ui.ctx().clone(),
                                        url.clone(),
                                        dest,
                                        control.clone(),
                                        tx,
                                    );
                                    self.downloads.insert(
                                        entry.uuid.clone(),
                                        DownloadState {
                                            downloaded: 0,
                                            total: None,
                                            paused: false,
                                            control,
                                            rx,
                                        },
                                    );
                                }
                            }
                            if ui.button("▶ Přehrát").clicked() {
                                if self.player.is_some() {
                                    // Buffer to a local temp file, handing
                                    // it to mpv as soon as enough has
                                    // arrived (not the whole thing - see
                                    // `recording_buffer` doc comment for
                                    // why buffering happens at all here).
                                    if let Some(player) = &self.player {
                                        let _ = player.stop();
                                    }
                                    self.clear_recording_playback();
                                    self.playing = None;
                                    self.paused = false;
                                    self.player_playback_error = None;
                                    self.playing_recording = Some(PlayingRecording {
                                        title: entry.disp_title.clone(),
                                    });
                                    self.video_controls_until =
                                        Some(Instant::now() + VIDEO_CONTROLS_IDLE_TIMEOUT);

                                    let dest = recordings::playback_cache_dir().join(
                                        recordings::safe_filename(&entry.disp_title, &entry.filename),
                                    );
                                    let control = DownloadControl::new();
                                    let (tx, rx) = std::sync::mpsc::channel();
                                    recordings::spawn_download(
                                        ui.ctx().clone(),
                                        url.clone(),
                                        dest.clone(),
                                        control.clone(),
                                        tx,
                                    );
                                    self.recording_buffer = Some(RecordingBuffer {
                                        path: dest,
                                        downloaded: 0,
                                        total: None,
                                        paused: false,
                                        loaded_into_player: false,
                                        control,
                                        rx,
                                    });
                                } else {
                                    // No embedded mpv (init failed at
                                    // startup) - fall back to whatever
                                    // the OS considers the default
                                    // handler for the URL.
                                    recordings::play_url(url);
                                }
                            }
                        } else {
                            ui.label("(soubor nedostupný)");
                        }
                    });
                });
            });
        }
    }

    fn recordings_upcoming_list(&mut self, ui: &mut egui::Ui, data: &RecordingsData) {
        if data.upcoming.is_empty() {
            ui.label("Žádné naplánované nahrávání.");
            return;
        }
        let mut entries: Vec<&DvrEntry> = data.upcoming.iter().collect();
        entries.sort_by_key(|e| e.start);
        for entry in entries {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.strong(entry.disp_title.clone());
                        if !entry.disp_subtitle.is_empty() {
                            ui.label(entry.disp_subtitle.clone());
                        }
                        ui.label(format!(
                            "{}  •  {}–{}",
                            entry.channelname,
                            epg::format_time(entry.start),
                            epg::format_time(entry.stop),
                        ));
                        if entry.sched_status == "recording" {
                            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), "● Právě se nahrává");
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Zrušit").clicked() {
                            let ctx = ui.ctx().clone();
                            let uuid = entry.uuid.clone();
                            self.spawn_recording_action(ctx, move |client| client.dvr_cancel(&uuid));
                        }
                    });
                });
            });
        }
    }

    fn recordings_autorec_list(&mut self, ui: &mut egui::Ui, data: &RecordingsData) {
        if data.autorec.is_empty() {
            ui.label("Žádná opakující se nahrávání.");
            return;
        }
        for entry in &data.autorec {
            let uuid = entry.get("uuid").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let title = entry
                .get("title")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("(bez názvu)")
                .to_string();
            let enabled = entry.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            let channel_uuid = entry.get("channel").and_then(|v| v.as_str()).unwrap_or("");
            let channel_name = if channel_uuid.is_empty() {
                "kterýkoliv kanál".to_string()
            } else {
                self.channels
                    .iter()
                    .find(|c| c.channel_id == channel_uuid)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "neznámý kanál".to_string())
            };
            let days = entry
                .get("weekdays")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| d.as_i64())
                        .filter(|n| (1..=7).contains(n))
                        .map(|n| WEEKDAY_LABELS[(n - 1) as usize])
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "každý den".to_string());

            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(title.clone());
                            if !enabled {
                                ui.colored_label(egui::Color32::GRAY, "(vypnuto)");
                            }
                        });
                        ui.label(format!("{channel_name}  •  {days}"));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("🗑 Smazat").clicked() {
                            let ctx = ui.ctx().clone();
                            let uuid = uuid.clone();
                            self.spawn_recording_action(ctx, move |client| client.autorec_delete(&uuid));
                        }
                        if ui.button("✏ Upravit").clicked() {
                            self.autorec_edit = Some(AutorecEditState::from_value(entry));
                        }
                    });
                });
            });
        }
    }

    /// Renders the autorec edit form's contents; returns `false` once the
    /// user cancels (the dialog should then close - see `recordings_tab`).
    /// Success closes it too, but that happens via `poll_recordings`
    /// clearing `self.autorec_edit` once the background save completes.
    fn autorec_edit_form(&mut self, ui: &mut egui::Ui, state: &mut AutorecEditState) -> bool {
        let mut keep_open = true;

        ui.horizontal(|ui| {
            ui.label("Název:");
            ui.text_edit_singleline(&mut state.title);
        });
        ui.checkbox(&mut state.enabled, "Aktivní");
        ui.horizontal(|ui| {
            ui.label("Kanál:");
            let selected_label = if state.channel.is_empty() {
                "Kterýkoliv".to_string()
            } else {
                self.channels
                    .iter()
                    .find(|c| c.channel_id == state.channel)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "Kterýkoliv".to_string())
            };
            egui::ComboBox::from_id_salt("autorec_channel")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.channel, String::new(), "Kterýkoliv");
                    for ch in &self.channels {
                        ui.selectable_value(&mut state.channel, ch.channel_id.clone(), ch.name.clone());
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label("Dny:");
            for (i, label) in WEEKDAY_LABELS.iter().enumerate() {
                ui.checkbox(&mut state.weekdays[i], *label);
            }
        });

        if let Some(err) = &state.error {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let save_clicked = ui
                .add_enabled(!state.saving, egui::Button::new("Uložit"))
                .clicked();
            if state.saving {
                ui.spinner();
            }
            if ui.add_enabled(!state.saving, egui::Button::new("Zrušit")).clicked() {
                keep_open = false;
            }

            if save_clicked {
                if let Some(obj) = state.node.as_object_mut() {
                    obj.insert("title".to_string(), serde_json::Value::String(state.title.clone()));
                    obj.insert("enabled".to_string(), serde_json::Value::Bool(state.enabled));
                    obj.insert(
                        "channel".to_string(),
                        serde_json::Value::String(state.channel.clone()),
                    );
                    let days: Vec<serde_json::Value> = state
                        .weekdays
                        .iter()
                        .enumerate()
                        .filter(|(_, &on)| on)
                        .map(|(i, _)| serde_json::Value::from((i as i64) + 1))
                        .collect();
                    obj.insert("weekdays".to_string(), serde_json::Value::Array(days));
                }
                let node = state.node.clone();
                if let Some(server) = self
                    .active_server_id
                    .as_ref()
                    .and_then(|id| self.settings.servers.iter().find(|s| &s.id == id))
                    .cloned()
                {
                    let ctx = ui.ctx().clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    state.rx = Some(rx);
                    state.saving = true;
                    state.error = None;
                    std::thread::spawn(move || {
                        let result = TvhClient::new(&server.url, &server.user, &server.password)
                            .map_err(|e| e.to_string())
                            .and_then(|client| client.autorec_save(&node).map_err(|e| e.to_string()));
                        let _ = tx.send(result);
                        ctx.request_repaint();
                    });
                }
            }
        });

        keep_open
    }

    // ---- Nastavení > Připojení (server list + add/edit form) --------

    fn settings_connection_tab(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            if self.server_edit.is_some() {
                self.render_server_edit(ui);
            } else {
                self.render_server_list(ui);
            }
        });
    }

    fn render_server_list(&mut self, ui: &mut egui::Ui) {
        ui.heading("Servery");
        ui.add_space(8.0);

        if self.settings.servers.is_empty() {
            ui.label("Zatím nemáš uložený žádný server.");
            ui.add_space(8.0);
        }

        let mut set_primary: Option<String> = None;
        let mut delete: Option<String> = None;
        let mut edit: Option<ServerProfile> = None;
        let mut connect_to: Option<ServerProfile> = None;

        egui::Grid::new("server_list_grid")
            .num_columns(4)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                for server in &self.settings.servers {
                    let is_primary = self.settings.primary_id.as_deref() == Some(server.id.as_str());
                    let is_active = self.active_server_id.as_deref() == Some(server.id.as_str());

                    let star = if is_primary { "★" } else { "☆" };
                    if ui
                        .button(star)
                        .on_hover_text("Primární server (načte se při spuštění appky)")
                        .clicked()
                    {
                        set_primary = Some(server.id.clone());
                    }

                    let mut name = server.name.clone();
                    if is_active {
                        name = format!("{name}  (připojeno)");
                    }
                    if !server.selected_tags.is_empty() {
                        name = format!("{name}  [{} štítků]", server.selected_tags.len());
                    }
                    ui.label(name);

                    if ui.button("Připojit").clicked() {
                        connect_to = Some(server.clone());
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Upravit").clicked() {
                            edit = Some(server.clone());
                        }
                        if ui.button("Smazat").clicked() {
                            delete = Some(server.id.clone());
                        }
                    });
                    ui.end_row();
                }
            });

        ui.add_space(12.0);
        if ui.button("+ Přidat server").clicked() {
            self.server_edit = Some(ServerEditState::blank());
        }

        if self.connecting {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Připojuji se...");
            });
        }
        if let Some(err) = &self.error {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err.as_str());
        }
        if let Some(msg) = &self.settings_message {
            ui.add_space(8.0);
            ui.label(msg.as_str());
        }
        if let Some(err) = &self.player_error {
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::from_rgb(200, 140, 40),
                format!("Přehrávání videa nebude dostupné: {err}"),
            );
        }

        // Apply queued actions after the loop/grid above (avoids
        // borrowing `self.settings.servers` and `self` mutably at once).
        if let Some(id) = set_primary {
            self.settings.primary_id = Some(id);
            self.settings_message = match self.settings.save() {
                Ok(()) => None,
                Err(e) => Some(format!("Chyba při ukládání: {e}")),
            };
        }
        if let Some(id) = delete {
            self.settings.servers.retain(|s| s.id != id);
            if self.settings.primary_id.as_deref() == Some(id.as_str()) {
                self.settings.primary_id = self.settings.servers.first().map(|s| s.id.clone());
            }
            if self.active_server_id.as_deref() == Some(id.as_str()) {
                self.active_server_id = None;
                self.channels.clear();
                self.server_info = None;
                self.selected = None;
                self.logo_textures.clear();
                self.logo_rx = None;
                self.stop_playback();
            }
            let _ = self.settings.save();
        }
        if let Some(server) = edit {
            self.server_edit = Some(ServerEditState::from_profile(&server));
        }
        if let Some(server) = connect_to {
            let ctx = ui.ctx().clone();
            self.start_connect(ctx, server);
        }
    }

    fn render_server_edit(&mut self, ui: &mut egui::Ui) {
        let is_new;
        let can_save;
        let mut do_test = false;

        {
            let Some(edit) = self.server_edit.as_mut() else {
                return;
            };
            is_new = edit.id.is_none();

            ui.heading(if is_new { "Přidat server" } else { "Upravit server" });
            ui.add_space(12.0);

            egui::Grid::new("server_edit_grid")
                .num_columns(2)
                .spacing([8.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Název (např. TV1):");
                    ui.text_edit_singleline(&mut edit.name);
                    ui.end_row();

                    ui.label("Server (např. 192.168.0.10:9981):");
                    ui.text_edit_singleline(&mut edit.url);
                    ui.end_row();

                    ui.label("Uživatel:");
                    ui.text_edit_singleline(&mut edit.user);
                    ui.end_row();

                    ui.label("Heslo:");
                    ui.add(egui::TextEdit::singleline(&mut edit.password).password(true));
                    ui.end_row();
                });
            ui.label("Heslo se uloží nešifrovaně do souboru nastavení na disku.");

            can_save = !edit.name.trim().is_empty() && !edit.url.trim().is_empty();

            ui.add_space(12.0);
            if ui
                .add_enabled(
                    !edit.url.trim().is_empty() && !edit.testing,
                    egui::Button::new("Test"),
                )
                .on_hover_text("Zkusí se přihlásit, ukáže verzi serveru a dostupné štítky kanálů")
                .clicked()
            {
                do_test = true;
            }

            if edit.testing {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Testuji...");
                });
            }

            if let Some(result) = &edit.test_result {
                ui.add_space(8.0);
                match result {
                    Ok(ok) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(60, 160, 60),
                            format!("OK: {}", ok.server_label),
                        );
                        if ok.tags.is_empty() {
                            ui.label("Server nehlásí žádné štítky kanálů (nebo je API neposkytuje).");
                        } else {
                            ui.add_space(4.0);
                            ui.label("Štítky kanálů (nezaškrtnuto = zobrazit všechny kanály):");
                            for tag in &ok.tags {
                                let mut checked = edit.selected_tags.contains(&tag.uuid);
                                if ui.checkbox(&mut checked, &tag.name).changed() {
                                    if checked {
                                        edit.selected_tags.push(tag.uuid.clone());
                                    } else {
                                        edit.selected_tags.retain(|u| u != &tag.uuid);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), e.as_str());
                    }
                }
            }
        }

        ui.add_space(16.0);
        let mut save_and_connect = false;
        let mut save_only = false;
        let mut cancel = false;

        ui.horizontal(|ui| {
            if ui.add_enabled(can_save, egui::Button::new("Uložit")).clicked() {
                save_only = true;
            }
            if ui
                .add_enabled(can_save && !self.connecting, egui::Button::new("Uložit a připojit"))
                .clicked()
            {
                save_and_connect = true;
            }
            if ui.button("Zrušit").clicked() {
                cancel = true;
            }
        });

        if self.connecting {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Připojuji se...");
            });
        }
        if let Some(err) = &self.error {
            ui.add_space(8.0);
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err.as_str());
        }

        if do_test {
            let ctx = ui.ctx().clone();
            self.start_server_test(ctx);
        }

        if cancel {
            self.server_edit = None;
        } else if save_only || save_and_connect {
            let was_active_server = self
                .server_edit
                .as_ref()
                .and_then(|e| e.id.as_ref())
                .is_some_and(|id| self.active_server_id.as_deref() == Some(id.as_str()));
            let profile = self.commit_server_edit();
            // If we just edited the server we're *currently* connected to
            // (e.g. changed its channel-tag selection), reconnect right
            // away even on a plain "Uložit" - otherwise the channel list
            // on screen stays stale until the user manually reconnects,
            // which looks exactly like "tag filtering doesn't do
            // anything".
            if save_and_connect || was_active_server {
                if let Some(profile) = profile {
                    let ctx = ui.ctx().clone();
                    self.start_connect(ctx, profile);
                }
            }
        }
    }

    /// Save `self.server_edit` into `self.settings` (insert or update),
    /// persist to disk, close the form, and return the saved profile.
    fn commit_server_edit(&mut self) -> Option<ServerProfile> {
        let edit = self.server_edit.take()?;
        let profile = ServerProfile {
            id: edit.id.clone().unwrap_or_else(Settings::new_id),
            name: edit.name.trim().to_string(),
            url: edit.url.trim().to_string(),
            user: edit.user,
            password: edit.password,
            selected_tags: edit.selected_tags,
        };

        if let Some(existing) = self.settings.servers.iter_mut().find(|s| s.id == profile.id) {
            *existing = profile.clone();
        } else {
            self.settings.servers.push(profile.clone());
        }
        if self.settings.primary_id.is_none() {
            self.settings.primary_id = Some(profile.id.clone());
        }

        self.settings_message = match self.settings.save() {
            Ok(()) => Some("Uloženo.".to_string()),
            Err(e) => Some(format!("Chyba při ukládání: {e}")),
        };

        Some(profile)
    }

    // ---- Nastavení > ostatní taby --------------------------------------

    fn settings_update_tab(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Kontrola aktualizací");
            ui.add_space(8.0);
            ui.label(format!("Aktuální verze: {}", update::CURRENT_VERSION));
            ui.add_space(8.0);

            let checking = self.update.checking;
            if ui
                .add_enabled(!checking, egui::Button::new("Zkontrolovat aktualizace"))
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_update_check(ctx);
            }
            if checking {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Kontroluji...");
                });
            }

            if let Some(result) = self.update.result.clone() {
                ui.add_space(12.0);
                match result {
                    Ok(info) if info.is_newer => {
                        ui.colored_label(
                            egui::Color32::from_rgb(60, 160, 60),
                            format!("Dostupná nová verze: {}", info.version),
                        );
                        ui.add_space(4.0);
                        let installing = self.update.installing;
                        if ui
                            .add_enabled(!installing, egui::Button::new("Stáhnout a nainstalovat"))
                            .clicked()
                        {
                            let ctx = ui.ctx().clone();
                            self.start_update_install(ctx, info);
                        }
                        if installing {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Stahuji a instaluji - appka se za chvíli sama restartuje...");
                            });
                        }
                    }
                    Ok(_) => {
                        ui.label("Máš nejnovější verzi.");
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), e.as_str());
                    }
                }
            }

            if let Some(msg) = &self.update.install_message {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::from_rgb(220, 60, 60), msg.as_str());
            }

            ui.add_space(20.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label("Repozitář:");
            ui.hyperlink(format!(
                "https://github.com/{}/{}",
                update::REPO_OWNER,
                update::REPO_NAME
            ));
        });
    }

    fn settings_about_tab(&self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(logo) = &self.about_logo {
                    ui.add(egui::Image::new(logo).max_width(72.0).max_height(72.0));
                    ui.add_space(12.0);
                }
                ui.vertical(|ui| {
                    ui.heading("TVH Client");
                    ui.label(format!("Verze {}", update::CURRENT_VERSION));
                    ui.label("Autor: David Trubka");
                });
            });
            ui.add_space(12.0);
            ui.label("Desktopový klient pro TVHeadend (Rust + egui, video přes vestavěný mpv).");
            ui.add_space(8.0);
            ui.label("Licence: PolyForm Noncommercial 1.0.0");
            ui.hyperlink("https://polyformproject.org/licenses/noncommercial/1.0.0/");
            ui.add_space(8.0);
            ui.label("Zdrojový kód:");
            ui.hyperlink(format!(
                "https://github.com/{}/{}",
                update::REPO_OWNER,
                update::REPO_NAME
            ));

            if let Some(info) = &self.server_info {
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(egui::RichText::new("Připojený server").strong());
                if let Some(name) = &info.name {
                    ui.label(name);
                }
                if let Some(version) = &info.sw_version {
                    ui.label(format!("Verze TVHeadend: {version}"));
                }
                if let Some(api_version) = info.api_version {
                    ui.label(format!("API verze: {api_version}"));
                }
            }

            if let Some(err) = &self.player_error {
                ui.add_space(16.0);
                ui.colored_label(
                    egui::Color32::from_rgb(200, 140, 40),
                    format!("Přehrávání videa není dostupné: {err}"),
                );
            }
        });
    }

    /// Where recordings download to (Nahrávky tab's "⬇ Stáhnout") - a
    /// plain text field rather than a folder-picker dialog (no such
    /// dependency in this project, see `recordings::downloads_dir` docs).
    /// Once `Settings::downloads_dir` is non-empty, the download button
    /// never asks again - see `recordings_finished_list`.
    fn settings_downloads_tab(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Stahování nahrávek");
            ui.add_space(8.0);
            ui.label("Složka, kam se ukládají nahrávky stažené v záložce Nahrávky:");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.settings.downloads_dir);
                if ui.button("Výchozí").clicked() {
                    self.settings.downloads_dir = recordings::downloads_dir().display().to_string();
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Uložit").clicked() {
                    match self.settings.save() {
                        Ok(()) => self.settings_message = Some("Uloženo.".to_string()),
                        Err(e) => self.settings_message = Some(format!("Uložení selhalo: {e}")),
                    }
                }
                if !self.settings.downloads_dir.trim().is_empty()
                    && ui.button("Otevřít složku").clicked()
                {
                    recordings::open_in_file_manager(std::path::Path::new(
                        self.settings.downloads_dir.trim(),
                    ));
                }
            });
            if let Some(msg) = &self.settings_message {
                ui.add_space(8.0);
                ui.label(msg);
            }
        });
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        match self.settings_tab {
            SettingsTab::Connection => self.settings_connection_tab(ui),
            SettingsTab::Downloads => self.settings_downloads_tab(ui),
            SettingsTab::UpdateCheck => self.settings_update_tab(ui),
            SettingsTab::About => self.settings_about_tab(ui),
        }
    }
}

impl eframe::App for TvhApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_connect(&ctx);
        self.poll_logos();
        self.poll_epg();
        self.poll_recordings();
        self.poll_player_events();
        self.poll_server_test();
        self.poll_update_check();
        self.poll_update_install();

        let fullscreen = is_fullscreen(&ctx);
        // Escape always backs out of fullscreen - a standard affordance
        // so the user is never stuck full-screen hunting for the shrink
        // icon (e.g. if the mouse isn't near the video corner).
        if fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        // Global keyboard shortcuts - skipped while a text field (search
        // box, server url/name/password, ...) has focus, so typing "t"
        // into e.g. the channel search box doesn't also jump to the TV
        // tab out from under you.
        if !ctx.egui_wants_keyboard_input() {
            ctx.input(|i| {
                // Volume: +/- always.
                if i.key_pressed(egui::Key::Plus) {
                    self.adjust_volume(5.0);
                }
                if i.key_pressed(egui::Key::Minus) {
                    self.adjust_volume(-5.0);
                }
                if self.top_tab == TopTab::Epg {
                    // On the EPG tab the arrow keys scroll the programme
                    // grid instead of volume - Up/Down through channels,
                    // Left/Right through time (same step as one "Nyní"
                    // hour-ruler tick).
                    if i.key_pressed(egui::Key::ArrowUp) {
                        self.epg_grid_scroll_y = (self.epg_grid_scroll_y - EPG_ROW_HEIGHT).max(0.0);
                    }
                    if i.key_pressed(egui::Key::ArrowDown) {
                        self.epg_grid_scroll_y += EPG_ROW_HEIGHT;
                    }
                    if i.key_pressed(egui::Key::ArrowLeft) {
                        self.epg_grid_scroll_x =
                            (self.epg_grid_scroll_x - EPG_PIXELS_PER_MIN * 60.0).max(0.0);
                    }
                    if i.key_pressed(egui::Key::ArrowRight) {
                        self.epg_grid_scroll_x += EPG_PIXELS_PER_MIN * 60.0;
                    }
                    // ~200 channels makes a single-row Up/Down step slow
                    // going - Page Up/Down jump 20 rows at a time instead
                    // of switching the live channel like everywhere else.
                    const EPG_PAGE_ROWS: f32 = 20.0;
                    if i.key_pressed(egui::Key::PageUp) {
                        self.epg_grid_scroll_y =
                            (self.epg_grid_scroll_y - EPG_ROW_HEIGHT * EPG_PAGE_ROWS).max(0.0);
                    }
                    if i.key_pressed(egui::Key::PageDown) {
                        self.epg_grid_scroll_y += EPG_ROW_HEIGHT * EPG_PAGE_ROWS;
                    }
                } else {
                    if i.key_pressed(egui::Key::ArrowUp) {
                        self.adjust_volume(5.0);
                    }
                    if i.key_pressed(egui::Key::ArrowDown) {
                        self.adjust_volume(-5.0);
                    }
                    // Channel switching: Page Up/Page Down.
                    if i.key_pressed(egui::Key::PageUp) {
                        self.select_relative_channel(-1);
                    }
                    if i.key_pressed(egui::Key::PageDown) {
                        self.select_relative_channel(1);
                    }
                }
            });
            // Tab switching: T/E/R/N.
            if ctx.input(|i| i.key_pressed(egui::Key::T)) {
                self.top_tab = TopTab::Tv;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::E)) {
                self.top_tab = TopTab::Epg;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::R)) {
                self.top_tab = TopTab::Recordings;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::N)) {
                self.top_tab = TopTab::Settings;
            }
        }

        // Cinema mode: while fullscreen on the TV tab, or on the Nahrávky
        // tab with a recording playing, skip the top menu too (each tab
        // hides its own side/bottom panels in that case) so only the
        // video + its own shrink icon show.
        if !(fullscreen
            && (self.top_tab == TopTab::Tv
                || (self.top_tab == TopTab::Recordings && self.playing_recording.is_some())))
        {
            self.menu_bar(ui);
        }

        match self.top_tab {
            TopTab::Tv => self.tv_tab(ui),
            TopTab::Epg => self.epg_tab(ui),
            TopTab::Recordings => self.recordings_tab(ui),
            TopTab::Settings => self.settings_screen(ui),
        }
    }
}
