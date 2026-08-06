//! Application UI: top menu (TV / EPG / Nahrávky / Nastavení), channel
//! list + embedded video playback, and settings sub-tabs (connection,
//! update check, about).
//!
//! Note on egui/eframe 0.33+: `eframe::App` no longer has an `update(&self,
//! ctx: &Context, ...)` method - it now has `ui(&mut self, ui: &mut Ui,
//! ...)`, and the old `TopBottomPanel`/`SidePanel` types were unified into
//! a single `egui::Panel` with `Panel::top()`/`::bottom()`/`::left()`/
//! `::right()` constructors, whose `.show()` takes the parent `Ui` instead
//! of the `Context`. Panel order matters: `CentralPanel` must always be
//! added last.
//!
//! Startup behavior: if there are saved connection settings, the app
//! tries to connect immediately (before the first frame is even drawn -
//! see `TvhApp::new`); on success it lands on the TV tab, on failure (or
//! if there's nothing saved) it lands on Nastavení > Připojení so the
//! user can fix things.
//!
//! Video playback embeds mpv directly into the TV tab's `CentralPanel`
//! via an `egui_glow::CallbackFn` / `egui::PaintCallback` - see
//! `player/mpv.rs` for the render-context/self-referential-struct
//! details. The bottom panel next to the video is where per-channel EPG
//! info ("now playing" / "next up") is meant to go next.

use crate::player::MpvPlayer;
use crate::settings::Settings;
use crate::tvh::{Channel, ServerInfo, TvhClient};
use crate::update;
use eframe::egui;
use eframe::egui_glow::CallbackFn;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

enum ConnectMsg {
    Success(ServerInfo, Vec<Channel>),
    Error(String),
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
    UpdateCheck,
    About,
}

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

pub struct TvhApp {
    top_tab: TopTab,
    settings_tab: SettingsTab,

    url: String,
    user: String,
    password: String,
    remember: bool,

    connecting: bool,
    error: Option<String>,
    settings_message: Option<String>,

    server_info: Option<ServerInfo>,
    channels: Vec<Channel>,
    filter: String,
    selected: Option<usize>,

    rx: Option<Receiver<ConnectMsg>>,

    // `None` if mpv/glow init failed - `player_error` then explains why.
    // Wrapped in `Arc` so the paint-callback closure (which egui requires
    // to be `'static`) can hold its own cheap clone.
    player: Option<Arc<MpvPlayer>>,
    player_error: Option<String>,
    // Which channel index mpv currently has loaded, so we only call
    // `player.load()` when the selection actually changes.
    playing: Option<usize>,
    paused: bool,

    update: UpdateState,
}

impl TvhApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let saved = Settings::load();
        let have_saved = !saved.url.is_empty() || !saved.user.is_empty() || !saved.password.is_empty();

        let (player, player_error) = match MpvPlayer::new(cc) {
            Ok(p) => (Some(Arc::new(p)), None),
            Err(e) => (None, Some(e)),
        };

        let mut app = Self {
            top_tab: TopTab::Tv,
            settings_tab: SettingsTab::Connection,
            url: saved.url,
            user: saved.user,
            password: saved.password,
            remember: have_saved,
            connecting: false,
            error: None,
            settings_message: None,
            server_info: None,
            channels: Vec::new(),
            filter: String::new(),
            selected: None,
            rx: None,
            player,
            player_error,
            playing: None,
            paused: false,
            update: UpdateState::default(),
        };

        if have_saved {
            // Try to connect right away with what's saved. `poll_connect`
            // sends us to Settings/Connection automatically if this fails.
            app.start_connect(cc.egui_ctx.clone());
        } else {
            app.top_tab = TopTab::Settings;
        }

        app
    }

    // ---- connect ----------------------------------------------------

    fn start_connect(&mut self, ctx: egui::Context) {
        self.connecting = true;
        self.error = None;

        let (tx, rx): (Sender<ConnectMsg>, Receiver<ConnectMsg>) = std::sync::mpsc::channel();
        self.rx = Some(rx);

        let url = self.url.clone();
        let user = self.user.clone();
        let password = self.password.clone();

        std::thread::spawn(move || {
            let result = (|| -> Result<(ServerInfo, Vec<Channel>), String> {
                let client = TvhClient::new(&url, &user, &password).map_err(|e| e.to_string())?;
                let info = client.server_info().map_err(|e| e.to_string())?;
                let channels = client.channels().map_err(|e| e.to_string())?;
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

    fn poll_connect(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        match rx.try_recv() {
            Ok(ConnectMsg::Success(info, channels)) => {
                self.server_info = Some(info);
                self.channels = channels;
                self.connecting = false;
                self.rx = None;
                self.top_tab = TopTab::Tv;

                if self.remember {
                    let settings = Settings {
                        url: self.url.clone(),
                        user: self.user.clone(),
                        password: self.password.clone(),
                    };
                    // Best-effort: a failed save shouldn't block using the app.
                    let _ = settings.save();
                }
            }
            Ok(ConnectMsg::Error(e)) => {
                self.error = Some(e);
                self.connecting = false;
                self.rx = None;
                // Send the user somewhere they can fix the problem.
                self.top_tab = TopTab::Settings;
                self.settings_tab = SettingsTab::Connection;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.connecting = false;
                self.rx = None;
            }
        }
    }

    /// Select a channel and (if mpv is available) start streaming it.
    fn select_channel(&mut self, i: usize) {
        self.selected = Some(i);
        if let (Some(player), Some(ch)) = (&self.player, self.channels.get(i)) {
            match player.load(&ch.stream_url) {
                Ok(()) => {
                    self.playing = Some(i);
                    self.paused = false;
                }
                Err(e) => self.error = Some(e),
            }
        }
    }

    fn stop_playback(&mut self) {
        if let Some(player) = &self.player {
            let _ = player.stop();
        }
        self.playing = None;
        self.paused = false;
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
                } else if let Some(info) = &self.server_info {
                    let name = info.name.clone().unwrap_or_else(|| "TVHeadend".to_string());
                    let version = info.sw_version.clone().unwrap_or_default();
                    ui.label(format!("Připojeno: {name} {version}"));
                } else {
                    ui.colored_label(egui::Color32::GRAY, "Nepřipojeno");
                }
            });

            if self.top_tab == TopTab::Settings {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings_tab, SettingsTab::Connection, "Připojení");
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

        egui::Panel::left("channel_list")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Hledat:");
                    ui.text_edit_singleline(&mut self.filter);
                });
                ui.label(format!("{} kanálů", self.channels.len()));
                ui.separator();

                let filter = self.filter.to_lowercase();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
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
                            let label = format!("{:>5}  {}", ch.number, ch.name);
                            if ui.selectable_label(selected, label).clicked() {
                                self.select_channel(i);
                            }
                        }
                    });
            });

        // Reserved for per-channel info; will grow into an EPG "now/next"
        // panel once EPG data is wired up.
        egui::Panel::bottom("channel_detail")
            .default_size(64.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                if let Some(i) = self.selected {
                    if let Some(ch) = self.channels.get(i).cloned() {
                        ui.horizontal(|ui| {
                            ui.label(format!("{} — {}", ch.number, ch.name));
                            ui.separator();
                            if self.player.is_some() {
                                if self.playing == Some(i) {
                                    let label = if self.paused { "▶ Pokračovat" } else { "⏸ Pauza" };
                                    if ui.button(label).clicked() {
                                        self.paused = !self.paused;
                                        if let Some(player) = &self.player {
                                            player.set_paused(self.paused);
                                        }
                                    }
                                    if ui.button("⏹ Zastavit").clicked() {
                                        self.stop_playback();
                                    }
                                } else {
                                    ui.label("Načítání streamu...");
                                }
                            } else {
                                ui.label("Přehrávání videa není dostupné (viz Nastavení > O programu).");
                            }
                        });
                    }
                } else {
                    ui.label("Vyber kanál vlevo.");
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            let available = ui.available_size();
            let (rect, _response) = ui.allocate_exact_size(available, egui::Sense::hover());
            ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);

            let have_video = self.player.is_some() && self.playing.is_some();
            if have_video {
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
        });
    }

    // ---- EPG / Nahrávky placeholders --------------------------------

    fn placeholder_tab(&self, ui: &mut egui::Ui, text: &str) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                ui.label(text);
            });
        });
    }

    // ---- Nastavení tabs -----------------------------------------------

    fn settings_connection_tab(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Připojení k TVHeadend serveru");
            ui.add_space(12.0);

            egui::Grid::new("connection_grid")
                .num_columns(2)
                .spacing([8.0, 10.0])
                .show(ui, |ui| {
                    ui.label("Server (např. 192.168.0.10:9981):");
                    ui.text_edit_singleline(&mut self.url);
                    ui.end_row();

                    ui.label("Uživatel:");
                    ui.text_edit_singleline(&mut self.user);
                    ui.end_row();

                    ui.label("Heslo:");
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.checkbox(&mut self.remember, "Zapamatovat přihlašovací údaje");
            ui.label("Heslo se v tom případě uloží nešifrovaně do souboru nastavení na disku.");

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let can_connect = !self.connecting && !self.url.trim().is_empty();
                if ui
                    .add_enabled(can_connect, egui::Button::new("Připojit"))
                    .clicked()
                {
                    let ctx = ui.ctx().clone();
                    self.start_connect(ctx);
                }

                if ui.button("Uložit").clicked() {
                    if self.remember {
                        let settings = Settings {
                            url: self.url.clone(),
                            user: self.user.clone(),
                            password: self.password.clone(),
                        };
                        self.settings_message = Some(match settings.save() {
                            Ok(()) => "Uloženo.".to_string(),
                            Err(e) => format!("Chyba při ukládání: {e}"),
                        });
                    } else {
                        self.settings_message = Some(match Settings::clear() {
                            Ok(()) => "Uložené údaje smazány.".to_string(),
                            Err(e) => format!("Chyba při mazání: {e}"),
                        });
                    }
                }

                if self.server_info.is_some() && ui.button("⟵ Odpojit").clicked() {
                    self.stop_playback();
                    self.channels.clear();
                    self.server_info = None;
                    self.selected = None;
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
        });
    }

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
            ui.heading("TVH Client");
            ui.label(format!("Verze {}", update::CURRENT_VERSION));
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

            if let Some(err) = &self.player_error {
                ui.add_space(16.0);
                ui.colored_label(
                    egui::Color32::from_rgb(200, 140, 40),
                    format!("Přehrávání videa není dostupné: {err}"),
                );
            }
        });
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        match self.settings_tab {
            SettingsTab::Connection => self.settings_connection_tab(ui),
            SettingsTab::UpdateCheck => self.settings_update_tab(ui),
            SettingsTab::About => self.settings_about_tab(ui),
        }
    }
}

impl eframe::App for TvhApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_connect();
        self.poll_update_check();
        self.poll_update_install();

        self.menu_bar(ui);

        match self.top_tab {
            TopTab::Tv => self.tv_tab(ui),
            TopTab::Epg => self.placeholder_tab(ui, "EPG - připravujeme."),
            TopTab::Recordings => self.placeholder_tab(ui, "Nahrávky - připravujeme."),
            TopTab::Settings => self.settings_screen(ui),
        }
    }
}
