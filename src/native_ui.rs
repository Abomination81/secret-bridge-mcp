use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, Stroke, TextEdit,
    Vec2,
};
use zeroize::{Zeroize, Zeroizing};

use crate::SERVICE_NAME;
use crate::{AppConfig, protocol};

const WINDOW_WIDTH: f32 = 520.0;
const WINDOW_HEIGHT_SECRET: f32 = 640.0;
const WINDOW_HEIGHT_CONFIRM: f32 = 540.0;
const WINDOW_INSET: i8 = 4;
const CONFIRM_ACTIONS_HEIGHT: f32 = 112.0;

const PAGE_BG: Color32 = Color32::from_rgb(2, 7, 4);
const CARD_BG: Color32 = Color32::from_rgb(8, 17, 10);
const INPUT_BG: Color32 = Color32::from_rgb(5, 8, 5);
const MUTED_BG: Color32 = Color32::from_rgb(13, 26, 16);
const BORDER: Color32 = Color32::from_rgb(35, 92, 43);
const BORDER_SOFT: Color32 = Color32::from_rgb(22, 59, 28);
const TEXT: Color32 = Color32::from_rgb(244, 247, 244);
const MUTED: Color32 = Color32::from_rgb(184, 195, 186);
const MUTED_2: Color32 = Color32::from_rgb(116, 135, 118);
const PRIMARY: Color32 = Color32::from_rgb(57, 255, 20);
const PRIMARY_HOVER: Color32 = Color32::from_rgb(125, 255, 100);
const ON_PRIMARY: Color32 = Color32::from_rgb(3, 16, 4);
const SUCCESS: Color32 = PRIMARY;

enum Mode {
    Secret {
        secret_id: String,
        client: String,
        label: String,
        description: String,
        env_var: Option<String>,
        replacing: bool,
    },
    Confirm {
        client: String,
        title: String,
        message: String,
    },
}

enum UiResult {
    Secret(Zeroizing<String>),
    Approved,
    Cancelled,
}

enum UiReply {
    Secret(Result<bool, String>),
    Confirm(bool),
}

enum UiRequest {
    Secret {
        secret_id: String,
        client: String,
        label: String,
        description: String,
        env_var: Option<String>,
        replacing: bool,
        reply: SyncSender<UiReply>,
    },
    Confirm {
        client: String,
        title: String,
        message: String,
        reply: SyncSender<UiReply>,
    },
    Shutdown,
}

static UI_REQUESTS: OnceLock<Sender<UiRequest>> = OnceLock::new();

struct SecretBridgeUi {
    mode: Option<Mode>,
    reply: Option<SyncSender<UiReply>>,
    completed: Option<UiResult>,
    secret: Zeroizing<String>,
    clipboard_cleared: bool,
    _secure_input: Option<SecureInputGuard>,
    window_size: Vec2,
    shown: bool,
}

impl SecretBridgeUi {
    fn new(
        mode: Mode,
        reply: SyncSender<UiReply>,
        secure_input: Option<SecureInputGuard>,
        window_size: Vec2,
    ) -> Self {
        Self {
            mode: Some(mode),
            reply: Some(reply),
            completed: None,
            secret: Zeroizing::new(String::new()),
            clipboard_cleared: false,
            _secure_input: secure_input,
            window_size,
            shown: false,
        }
    }

    fn finish(&mut self, ctx: &egui::Context, result: UiResult) {
        self.completed = Some(result);
        ctx.request_repaint();
    }

    fn close(&mut self, ctx: &egui::Context) {
        self.secret.zeroize();
        self.finish(ctx, UiResult::Cancelled);
    }

    fn header(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, title: &str, subtitle: &str) {
        let drag_rect = ui.available_rect_before_wrap();
        let drag_rect =
            egui::Rect::from_min_size(drag_rect.min, Vec2::new(drag_rect.width(), 58.0));
        let drag = ui.interact(drag_rect, ui.id().with("title_drag"), egui::Sense::drag());
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(title).size(20.0).strong().color(TEXT));
                ui.add_space(3.0);
                ui.label(RichText::new(subtitle).size(13.0).color(MUTED));
            });
            ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                let close = ui.add(
                    egui::Button::new(RichText::new("×").size(25.0).color(MUTED))
                        .frame(false)
                        .min_size(Vec2::new(32.0, 32.0)),
                );
                if close.on_hover_text("Cancel and close").clicked() {
                    self.close(ctx);
                }
            });
        });
        ui.add_space(18.0);
    }

    fn request_origin(ui: &mut egui::Ui, client: &str) {
        ui.label(
            RichText::new("CLAIMED REQUESTER")
                .size(11.0)
                .strong()
                .color(MUTED_2),
        );
        ui.add_space(7.0);
        Frame::new()
            .fill(MUTED_BG)
            .corner_radius(CornerRadius::same(10))
            .stroke(Stroke::new(1.0, BORDER_SOFT))
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(SUCCESS, "●");
                    ui.label(RichText::new(client).size(13.0).color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new("Unverified local process")
                                .size(12.0)
                                .color(MUTED),
                        );
                    });
                });
            });
    }

    fn brand_footer(ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new("Built by").size(11.0).color(MUTED_2));
            ui.hyperlink_to(
                RichText::new("Abomination81")
                    .size(11.0)
                    .strong()
                    .color(PRIMARY),
                "https://github.com/Abomination81",
            );
            ui.label(RichText::new("·").size(11.0).color(MUTED_2));
            ui.hyperlink_to(
                RichText::new("X @Abomination81")
                    .size(11.0)
                    .strong()
                    .color(PRIMARY),
                "https://x.com/Abomination81",
            );
        });
    }

    fn secret_view(
        &mut self,
        ui: &mut egui::Ui,
        client: &str,
        label: &str,
        description: &str,
        env_var: Option<&str>,
        replacing: bool,
    ) {
        let ctx = ui.ctx().clone();
        self.header(
            ui,
            &ctx,
            if replacing {
                "Replace stored secret"
            } else {
                "Secure secret entry"
            },
            "The value stays on this device and out of AI messages",
        );
        Self::request_origin(ui, client);
        ui.add_space(16.0);

        ui.label(RichText::new("SECRET").size(11.0).strong().color(MUTED_2));
        ui.add_space(7.0);
        ui.label(RichText::new(label).size(16.0).strong().color(TEXT));
        ui.add_space(5.0);
        ui.label(RichText::new(description).size(13.0).color(MUTED));

        if let Some(env_var) = env_var {
            ui.add_space(10.0);
            Frame::new()
                .fill(MUTED_BG)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("ENV  {env_var}"))
                            .monospace()
                            .size(12.0)
                            .color(MUTED),
                    );
                });
        }

        ui.add_space(22.0);
        ui.label(
            RichText::new("Paste or enter secret")
                .size(14.0)
                .color(TEXT),
        );
        ui.add_space(8.0);
        Frame::new()
            .fill(INPUT_BG)
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(Margin::symmetric(14, 10))
            .show(ui, |ui| {
                let input = ui.add_sized(
                    [ui.available_width(), 34.0],
                    TextEdit::singleline(&mut *self.secret)
                        .password(true)
                        .hint_text("Secret value")
                        .text_color(TEXT)
                        .font(FontId::monospace(15.0))
                        .frame(Frame::NONE),
                );
                if self.secret.is_empty() {
                    input.request_focus();
                }
            });
        let pasted = ctx.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Paste(_)))
        });
        if pasted {
            ctx.copy_text(String::new());
            self.clipboard_cleared = true;
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.colored_label(SUCCESS, "●");
            ui.label(RichText::new(vault_message()).size(12.0).color(MUTED));
        });
        ui.label(
            RichText::new("Stored locally; approved .env exports are readable local files.")
                .size(11.0)
                .color(MUTED_2),
        );
        if self.clipboard_cleared {
            ui.label(
                RichText::new("Clipboard cleared after paste")
                    .size(11.0)
                    .color(MUTED_2),
            );
        }
        ui.add_space(10.0);
        Frame::new()
            .fill(Color32::from_rgb(15, 29, 31))
            .corner_radius(CornerRadius::same(10))
            .stroke(Stroke::new(1.0, Color32::from_rgb(31, 65, 61)))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("✓").size(14.0).strong().color(SUCCESS));
                    ui.label(
                        RichText::new(
                            "The AI receives only a confirmation and an opaque secret ID.",
                        )
                        .size(12.0)
                        .color(Color32::from_rgb(172, 197, 193)),
                    );
                });
            });
        ui.add_space(10.0);
        let enabled = !self.secret.is_empty() && self.secret.len() <= 65_536;
        let button = egui::Button::new(
            RichText::new(if replacing {
                "Replace secret securely"
            } else {
                "Store secret securely"
            })
            .size(15.0)
            .strong()
            .color(if enabled { ON_PRIMARY } else { MUTED_2 }),
        )
        .fill(if enabled { PRIMARY } else { MUTED_BG })
        .corner_radius(CornerRadius::same(11))
        .stroke(Stroke::NONE)
        .min_size(Vec2::new(ui.available_width(), 50.0));
        let response = ui.add_enabled(enabled, button);
        if enabled && response.hovered() {
            ui.painter().rect_stroke(
                response.rect,
                CornerRadius::same(11),
                Stroke::new(1.0, PRIMARY_HOVER),
                egui::StrokeKind::Inside,
            );
        }
        if response.clicked() {
            let secret = Zeroizing::new(std::mem::take(&mut *self.secret));
            self.finish(&ctx, UiResult::Secret(secret));
        }
        ui.add_space(3.0);
        if ui
            .add(egui::Button::new(RichText::new("Cancel").size(13.0).color(MUTED)).frame(false))
            .clicked()
        {
            self.close(&ctx);
        }
        ui.add_space(8.0);
        Self::brand_footer(ui);
    }

    fn confirm_view(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        client: &str,
        title: &str,
        message: &str,
    ) {
        self.header(ui, ctx, title, "Review every detail before approving");
        Self::request_origin(ui, client);
        ui.add_space(16.0);

        let message_height = (ui.available_height() - CONFIRM_ACTIONS_HEIGHT - 28.0).max(100.0);
        Frame::new()
            .fill(INPUT_BG)
            .corner_radius(CornerRadius::same(12))
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(message_height)
                    .min_scrolled_height(message_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(RichText::new(message).size(13.0).color(TEXT));
                    });
            });
        ui.add_space(10.0);
        let approve = ui.add(
            egui::Button::new(
                RichText::new("Approve")
                    .size(15.0)
                    .strong()
                    .color(ON_PRIMARY),
            )
            .fill(PRIMARY)
            .corner_radius(CornerRadius::same(11))
            .min_size(Vec2::new(ui.available_width(), 50.0)),
        );
        if approve.clicked() {
            self.finish(ctx, UiResult::Approved);
        }
        ui.add_space(3.0);
        if ui
            .add(egui::Button::new(RichText::new("Cancel").size(13.0).color(MUTED)).frame(false))
            .clicked()
        {
            self.close(ctx);
        }
        ui.add_space(8.0);
        Self::brand_footer(ui);
    }

    fn show(&self, ctx: &egui::Context) {
        if let Some(monitor_size) = ctx.input(|input| input.viewport().monitor_size) {
            let position = egui::Pos2::new(
                ((monitor_size.x - self.window_size.x) / 2.0).max(0.0),
                ((monitor_size.y - self.window_size.y) / 2.0).max(0.0),
            );
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::AlwaysOnTop,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn dismiss(ctx: &egui::Context) {
        // Disable hit-testing before teardown. On macOS this maps to
        // NSWindow.setIgnoresMouseEvents(true), so the surface cannot intercept clicks while
        // visibility and close commands propagate through the window server.
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn complete_request(&mut self, ctx: &egui::Context) {
        let Some(outcome) = self.completed.take() else {
            return;
        };
        let Some(mode) = self.mode.take() else {
            return;
        };
        let Some(reply) = self.reply.take() else {
            return;
        };
        Self::dismiss(ctx);
        match (mode, outcome) {
            (Mode::Secret { secret_id, .. }, UiResult::Secret(secret)) => {
                let error_reply = reply.clone();
                let spawn_result = thread::Builder::new()
                    .name("secret-bridge-credential-store".into())
                    .spawn(move || {
                        let result = keyring::Entry::new(SERVICE_NAME, &secret_id)
                            .map_err(|_| {
                                "could not access the operating-system credential store".to_string()
                            })
                            .and_then(|entry| {
                                ensure_entry_is_empty(&entry)?;
                                entry.set_password(&secret).map_err(|_| {
                                    "could not store the secret in the operating-system credential store"
                                        .to_string()
                                })?;
                                Ok(true)
                            });
                        let _ = reply.send(UiReply::Secret(result));
                    });
                if let Err(error) = spawn_result {
                    let _ = error_reply.send(UiReply::Secret(Err(format!(
                        "could not start the credential storage worker: {error}"
                    ))));
                }
            }
            (Mode::Secret { .. }, UiResult::Cancelled) => {
                let _ = reply.send(UiReply::Secret(Ok(false)));
            }
            (Mode::Confirm { .. }, UiResult::Approved) => {
                let _ = reply.send(UiReply::Confirm(true));
            }
            (Mode::Confirm { .. }, UiResult::Cancelled) => {
                let _ = reply.send(UiReply::Confirm(false));
            }
            (Mode::Secret { .. }, UiResult::Approved) => {
                let _ = reply.send(UiReply::Secret(Err("invalid UI result state".into())));
            }
            (Mode::Confirm { .. }, UiResult::Secret(_)) => {
                let _ = reply.send(UiReply::Confirm(false));
            }
        }
        self.secret.zeroize();
        self.clipboard_cleared = false;
        self._secure_input = None;
    }
}

impl eframe::App for SecretBridgeUi {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(50));
        if self.mode.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let mut visuals = egui::Visuals::dark();
        visuals.window_fill = CARD_BG;
        visuals.panel_fill = PAGE_BG;
        visuals.override_text_color = Some(TEXT);
        visuals.hyperlink_color = PRIMARY;
        visuals.selection.bg_fill = PRIMARY;
        visuals.widgets.inactive.bg_fill = MUTED_BG;
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(17, 44, 21);
        visuals.widgets.active.bg_fill = PRIMARY;
        ctx.set_visuals(visuals);

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(CARD_BG)
                    .corner_radius(CornerRadius::same(24))
                    .stroke(Stroke::new(1.0, BORDER))
                    .outer_margin(Margin::same(WINDOW_INSET))
                    .inner_margin(Margin::same(24)),
            )
            .show(ui, |ui| {
                let mode = self.mode.take().expect("mode checked above");
                match &mode {
                    Mode::Secret {
                        secret_id: _,
                        client,
                        label,
                        description,
                        env_var,
                        replacing,
                    } => self.secret_view(
                        ui,
                        client,
                        label,
                        description,
                        env_var.as_deref(),
                        *replacing,
                    ),
                    Mode::Confirm {
                        client,
                        title,
                        message,
                    } => self.confirm_view(ui, &ctx, client, title, message),
                }
                self.mode = Some(mode);
            });
        if !self.shown {
            self.show(&ctx);
            self.shown = true;
        }
        self.complete_request(&ctx);
    }
}

impl Drop for SecretBridgeUi {
    fn drop(&mut self) {
        self.secret.zeroize();
        if let Some(reply) = self.reply.take() {
            match self.mode.take() {
                Some(Mode::Secret { .. }) => {
                    let _ = reply.send(UiReply::Secret(Err("SecretBridge UI closed".into())));
                }
                Some(Mode::Confirm { .. }) => {
                    let _ = reply.send(UiReply::Confirm(false));
                }
                None => {}
            }
        }
    }
}

fn validate_ui_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<(), String> {
    crate::validation::validate_display_text(name, value, min, max, allow_newlines)
}

fn valid_env_name(name: &str) -> bool {
    if name.len() > 128 {
        return false;
    }
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_uppercase())
        && chars.all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        })
}

fn valid_secret_id(id: &str) -> bool {
    crate::validation::valid_secret_id(id)
}

fn vault_message() -> &'static str {
    #[cfg(target_os = "macos")]
    return "Will be stored in macOS Keychain";

    #[cfg(target_os = "windows")]
    return "Will be stored in Windows Credential Manager";

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return "Will be stored in the operating-system credential store";
}

pub(crate) fn prompt_and_store_secret(
    secret_id: &str,
    client: &str,
    label: &str,
    description: &str,
    env_var: Option<&str>,
    replacing: bool,
) -> Result<bool, String> {
    if !valid_secret_id(secret_id) {
        return Err("invalid provisional secret ID".into());
    }
    validate_ui_text("client", client, 1, 80, false)?;
    validate_ui_text("label", label, 3, 120, false)?;
    validate_ui_text("description", description, 3, 500, true)?;
    if env_var.is_some_and(|name| !valid_env_name(name)) {
        return Err("invalid environment variable name".into());
    }
    let sender = UI_REQUESTS
        .get()
        .ok_or_else(|| "native UI broker is not running".to_string())?;
    let (reply, response) = mpsc::sync_channel(0);
    sender
        .send(UiRequest::Secret {
            secret_id: secret_id.to_string(),
            client: client.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            env_var: env_var.map(str::to_string),
            replacing,
            reply,
        })
        .map_err(|_| "native UI broker stopped".to_string())?;
    match response.recv().map_err(|_| "native UI broker stopped")? {
        UiReply::Secret(result) => result,
        UiReply::Confirm(_) => Err("native UI returned the wrong response type".into()),
    }
}

pub(crate) fn confirm(client: &str, title: &str, message: &str) -> Result<bool, String> {
    validate_ui_text("client", client, 1, 80, false)?;
    validate_ui_text("title", title, 1, 80, false)?;
    validate_ui_text("message", message, 1, 12_000, true)?;
    let sender = UI_REQUESTS
        .get()
        .ok_or_else(|| "native UI broker is not running".to_string())?;
    let (reply, response) = mpsc::sync_channel(0);
    sender
        .send(UiRequest::Confirm {
            client: client.to_string(),
            title: title.to_string(),
            message: message.to_string(),
            reply,
        })
        .map_err(|_| "native UI broker stopped".to_string())?;
    match response.recv().map_err(|_| "native UI broker stopped")? {
        UiReply::Confirm(approved) => Ok(approved),
        UiReply::Secret(_) => Err("native UI returned the wrong response type".into()),
    }
}

fn ensure_entry_is_empty(entry: &keyring::Entry) -> Result<(), String> {
    match entry.get_password() {
        Ok(mut existing) => {
            existing.zeroize();
            Err("refusing to overwrite an existing credential ID".into())
        }
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err("operating-system credential lookup failed".into()),
    }
}

pub fn run_desktop(config: AppConfig) -> Result<(), String> {
    let (sender, receiver) = mpsc::channel();
    UI_REQUESTS
        .set(sender.clone())
        .map_err(|_| "native UI broker was initialized twice".to_string())?;
    let protocol_result = Arc::new(Mutex::new(None));
    let worker_result = Arc::clone(&protocol_result);
    thread::Builder::new()
        .name("secret-bridge-protocol".into())
        .spawn(move || {
            let result = protocol::run_stdio(config);
            if let Ok(mut slot) = worker_result.lock() {
                *slot = Some(result);
            }
            let _ = sender.send(UiRequest::Shutdown);
        })
        .map_err(|error| format!("cannot start protocol worker: {error}"))?;

    loop {
        let request = receiver
            .recv()
            .map_err(|_| "native UI request channel stopped".to_string())?;
        match request {
            UiRequest::Secret {
                secret_id,
                client,
                label,
                description,
                env_var,
                replacing,
                reply,
            } => {
                let guard = match SecureInputGuard::try_new() {
                    Ok(guard) => guard,
                    Err(error) => {
                        let _ = reply.send(UiReply::Secret(Err(error)));
                        continue;
                    }
                };
                run_request_window(
                    Mode::Secret {
                        secret_id,
                        client,
                        label,
                        description,
                        env_var,
                        replacing,
                    },
                    reply,
                    Some(guard),
                    WINDOW_HEIGHT_SECRET,
                )?;
            }
            UiRequest::Confirm {
                client,
                title,
                message,
                reply,
            } => run_request_window(
                Mode::Confirm {
                    client,
                    title,
                    message,
                },
                reply,
                None,
                WINDOW_HEIGHT_CONFIRM,
            )?,
            UiRequest::Shutdown => break,
        }
    }

    protocol_result
        .lock()
        .map_err(|_| "protocol result channel failed".to_string())?
        .take()
        .ok_or_else(|| "SecretBridge UI stopped before the protocol worker".to_string())?
}

fn run_request_window(
    mode: Mode,
    reply: SyncSender<UiReply>,
    secure_input: Option<SecureInputGuard>,
    height: f32,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let event_loop_builder: Option<eframe::EventLoopBuilderHook> = {
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};

        Some(Box::new(|builder| {
            builder.with_activation_policy(ActivationPolicy::Accessory);
        }))
    };

    #[cfg(not(target_os = "macos"))]
    let event_loop_builder = None;

    let window_size = Vec2::new(WINDOW_WIDTH, height);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SecretBridge")
            .with_inner_size(window_size)
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_has_shadow(false)
            .with_window_level(egui::WindowLevel::Normal)
            .with_mouse_passthrough(true)
            .with_visible(false),
        renderer: eframe::Renderer::Glow,
        event_loop_builder,
        ..Default::default()
    };

    eframe::run_native(
        "SecretBridge",
        options,
        Box::new(move |_creation_context| {
            Ok(Box::new(SecretBridgeUi::new(
                mode,
                reply,
                secure_input,
                window_size,
            )))
        }),
    )
    .map_err(|error| format!("SecretBridge UI failed: {error}"))
}

#[cfg(target_os = "macos")]
struct SecureInputGuard(bool);

#[cfg(target_os = "macos")]
impl SecureInputGuard {
    fn try_new() -> Result<Self, String> {
        #[link(name = "Carbon", kind = "framework")]
        unsafe extern "C" {
            fn EnableSecureEventInput() -> i32;
        }
        // SAFETY: This process owns the secret-entry window and pairs a successful call with
        // DisableSecureEventInput in Drop.
        if unsafe { EnableSecureEventInput() } == 0 {
            Ok(Self(true))
        } else {
            Err("macOS Secure Event Input could not be enabled; secret entry was blocked".into())
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SecureInputGuard {
    fn drop(&mut self) {
        if self.0 {
            #[link(name = "Carbon", kind = "framework")]
            unsafe extern "C" {
                fn DisableSecureEventInput() -> i32;
            }
            // SAFETY: Balances the successful EnableSecureEventInput call made by new().
            let _ = unsafe { DisableSecureEventInput() };
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct SecureInputGuard;

#[cfg(not(target_os = "macos"))]
impl SecureInputGuard {
    fn try_new() -> Result<Self, String> {
        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dismissal_disables_hit_testing_before_closing_the_window() {
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            SecretBridgeUi::dismiss(ui.ctx());
        });
        let commands = &output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .commands;

        let command_index = |expected: &egui::ViewportCommand| {
            commands
                .iter()
                .position(|command| command == expected)
                .unwrap_or_else(|| panic!("missing viewport command: {expected:?}"))
        };
        let passthrough = command_index(&egui::ViewportCommand::MousePassthrough(true));
        let hidden = command_index(&egui::ViewportCommand::Visible(false));
        let normal_level = command_index(&egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        let closed = command_index(&egui::ViewportCommand::Close);

        assert!(passthrough < hidden);
        assert!(hidden < normal_level);
        assert!(normal_level < closed);
    }
}
