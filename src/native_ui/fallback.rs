use super::{
    Mode, PROMPT_CANCELLED_EXIT_CODE, PROMPT_FAILED_EXIT_CODE, SERVICE_NAME, SecureInputGuard,
    ensure_entry_is_empty, vault_message,
};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, Stroke, TextEdit,
    Vec2,
};
use std::thread;
use std::time::Duration;
use zeroize::{Zeroize, Zeroizing};
const WINDOW_WIDTH: f32 = 440.0;
const WINDOW_HEIGHT_SECRET: f32 = 400.0;
const WINDOW_HEIGHT_CONFIRM: f32 = 420.0;

const PAGE_BG: Color32 = Color32::WHITE;
const INPUT_BG: Color32 = Color32::from_rgb(245, 245, 247);
const TEXT: Color32 = Color32::from_rgb(29, 29, 31);
const MUTED: Color32 = Color32::from_rgb(110, 110, 115);
const PRIMARY: Color32 = Color32::from_rgb(0, 113, 227);

enum UiResult {
    Secret(Zeroizing<String>),
    Approved,
    Cancelled,
}

struct SecretBridgeUi {
    mode: Option<Mode>,
    completed: Option<UiResult>,
    secret: Zeroizing<String>,
    clipboard_cleared: bool,
    input_focused: bool,
    _secure_input: Option<SecureInputGuard>,
    window_size: Vec2,
    shown: bool,
}

impl SecretBridgeUi {
    fn new(mode: Mode, secure_input: Option<SecureInputGuard>, window_size: Vec2) -> Self {
        Self {
            mode: Some(mode),
            completed: None,
            secret: Zeroizing::new(String::new()),
            clipboard_cleared: false,
            input_focused: false,
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

    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), egui::Sense::hover());
        // Keep a dedicated drag surface clear of labels and buttons. On macOS the native
        // traffic lights occupy the left edge of the transparent system title bar.
        let drag_rect = egui::Rect::from_min_max(
            rect.min + Vec2::new(80.0, 0.0),
            rect.max - Vec2::new(44.0, 0.0),
        );
        let drag = ui.interact(drag_rect, ui.id().with("title_drag"), egui::Sense::drag());
        if drag.is_pointer_button_down_on()
            && ctx.input(|input| input.pointer.button_pressed(egui::PointerButton::Primary))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "SecretBridge",
            FontId::proportional(13.0),
            MUTED,
        );
        #[cfg(not(target_os = "macos"))]
        {
            let close_rect = egui::Rect::from_min_size(
                rect.right_top() - Vec2::new(40.0, 0.0),
                Vec2::splat(40.0),
            );
            if ui
                .put(close_rect, egui::Button::new("×").frame(false))
                .clicked()
            {
                self.close(&ctx);
            }
        }
    }

    fn secret_details(ui: &mut egui::Ui, mode: &Mode) {
        let Mode::Secret {
            client,
            label,
            description,
            env_var,
            workspace,
            replacing,
            ..
        } = mode
        else {
            return;
        };
        if *replacing {
            ui.label(RichText::new("Replace credential").size(12.0).color(MUTED));
            ui.add_space(4.0);
        }
        ui.label(semibold(label, 22.0).color(TEXT));
        ui.add_space(6.0);
        ui.label(RichText::new(description).size(14.0).color(MUTED));
        ui.add_space(18.0);
        ui.label(semibold("Use in this workspace", 12.0).color(MUTED));
        ui.add_space(4.0);
        ui.label(RichText::new(workspace).size(12.0).color(TEXT));
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Saved for {client}. No extra approval needed here."))
            .size(12.0).color(MUTED))
            .on_hover_text("Save & Use authorizes .env writes in this workspace. Client names are supplied by the requesting process and are not verified. Local processes with filesystem access can read exported .env files.");
        if let Some(name) = env_var {
            ui.response()
                .on_hover_text(format!("Environment variable: {name}"));
        }
    }

    fn secret_input(&mut self, ui: &mut egui::Ui) {
        ui.label(semibold("API key or password", 12.0));
        ui.add_space(6.0);
        let response = Frame::new()
            .fill(INPUT_BG)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.add_sized(
                    [ui.available_width(), 24.0],
                    TextEdit::singleline(&mut *self.secret)
                        .password(true)
                        .hint_text("Paste your credential")
                        .text_color(TEXT)
                        .font(FontId::proportional(14.0))
                        .frame(Frame::NONE),
                )
            })
            .inner;
        if !self.input_focused {
            response.request_focus();
            self.input_focused = true;
        }
        if ui.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Paste(_)))
        }) {
            ui.ctx().copy_text(String::new());
            self.clipboard_cleared = true;
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(if self.clipboard_cleared {
                "Clipboard cleared. Your credential stays out of chat."
            } else {
                vault_message()
            })
            .size(11.0)
            .color(MUTED),
        );
        if self.secret.len() > 65_536 {
            ui.label(
                RichText::new("Use a credential smaller than 64 KiB.")
                    .size(11.0)
                    .color(Color32::from_rgb(180, 45, 35)),
            );
        }
        ui.add_space(16.0);
    }

    fn actions(&mut self, ui: &mut egui::Ui, is_secret: bool) {
        let ctx = ui.ctx().clone();
        let enabled = !is_secret || (!self.secret.is_empty() && self.secret.len() <= 65_536);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let response = ui.add_enabled(
                enabled,
                egui::Button::new(
                    semibold(if is_secret { "Save & Use" } else { "Allow" }, 13.0)
                        .color(Color32::WHITE),
                )
                .fill(PRIMARY)
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::same(8))
                .min_size(Vec2::new(112.0, 36.0)),
            );
            // Return submits secret entry, but confirmations still require an explicit click.
            let submit = enabled
                && (response.clicked()
                    || (is_secret && ctx.input(|input| input.key_pressed(egui::Key::Enter))));
            if submit {
                let result = if is_secret {
                    UiResult::Secret(Zeroizing::new(std::mem::take(&mut *self.secret)))
                } else {
                    UiResult::Approved
                };
                self.finish(&ctx, result);
            }
            ui.add_space(8.0);
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").size(13.0).color(TEXT))
                        .fill(INPUT_BG)
                        .stroke(Stroke::NONE)
                        .corner_radius(CornerRadius::same(8))
                        .min_size(Vec2::new(80.0, 36.0)),
                )
                .clicked()
            {
                self.close(&ctx);
            }
        });
    }

    fn render(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut visuals = egui::Visuals::light();
        visuals.window_fill = PAGE_BG;
        visuals.panel_fill = PAGE_BG;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = Color32::from_rgb(210, 231, 255);
        visuals.window_shadow = egui::epaint::Shadow::NONE;
        visuals.popup_shadow = egui::epaint::Shadow::NONE;
        visuals.window_stroke = Stroke::NONE;
        ctx.set_visuals(visuals);

        let Some(mode) = self.mode.take() else {
            return;
        };
        let is_secret = matches!(mode, Mode::Secret { .. });
        egui::Panel::top("title_bar")
            .show_separator_line(false)
            .frame(Frame::new().fill(PAGE_BG))
            .show(ui, |ui| self.title_bar(ui));
        // Allocate controls first, outside the scrolling details, so text length and window
        // size can never push the input, submit button, or cancel button below the viewport.
        egui::Panel::bottom("actions")
            .show_separator_line(false)
            .frame(Frame::new().fill(PAGE_BG).inner_margin(Margin {
                left: 28,
                right: 28,
                top: 16,
                bottom: 24,
            }))
            .show(ui, |ui| {
                if is_secret {
                    self.secret_input(ui);
                }
                self.actions(ui, is_secret);
            });
        egui::CentralPanel::default()
            .frame(Frame::new().fill(PAGE_BG).inner_margin(Margin {
                left: 28,
                right: 28,
                top: 12,
                bottom: 0,
            }))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match &mode {
                            Mode::Secret { .. } => Self::secret_details(ui, &mode),
                            Mode::Confirm {
                                client,
                                title,
                                message,
                            } => {
                                ui.label(RichText::new(title).size(22.0).strong());
                                ui.add_space(16.0);
                                ui.label(RichText::new(message).size(13.0));
                                ui.add_space(16.0);
                                ui.label(
                                    RichText::new(format!(
                                        "Requested by {client} · unverified client name"
                                    ))
                                    .size(11.0)
                                    .color(MUTED),
                                );
                            }
                        }
                    });
            });
        self.mode = Some(mode);
        if ctx.input(|input| {
            input.key_pressed(egui::Key::Escape) || input.viewport().close_requested()
        }) {
            self.close(&ctx);
        }
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
        // the one-shot prompt process finishes.
        ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn complete_request(&mut self, ctx: &egui::Context) {
        let Some(outcome) = self.completed.take() else {
            return;
        };
        let Some(mode) = self.mode.take() else {
            return;
        };
        let secure_input = self._secure_input.take();
        Self::dismiss(ctx);
        match (mode, outcome) {
            (Mode::Secret { secret_id, .. }, UiResult::Secret(secret)) => {
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
                                Ok(())
                            });
                        drop(secret);
                        drop(secure_input);
                        std::process::exit(if result.is_ok() {
                            0
                        } else {
                            PROMPT_FAILED_EXIT_CODE
                        });
                    });
                if spawn_result.is_err() {
                    std::process::exit(PROMPT_FAILED_EXIT_CODE);
                }
            }
            (Mode::Secret { .. }, UiResult::Cancelled) => {
                self.secret.zeroize();
                drop(secure_input);
                std::process::exit(PROMPT_CANCELLED_EXIT_CODE.into());
            }
            (Mode::Confirm { .. }, UiResult::Approved) => {
                drop(secure_input);
                std::process::exit(0);
            }
            (Mode::Confirm { .. }, UiResult::Cancelled) => {
                drop(secure_input);
                std::process::exit(PROMPT_CANCELLED_EXIT_CODE.into());
            }
            (Mode::Secret { .. }, UiResult::Approved) => {
                drop(secure_input);
                std::process::exit(PROMPT_FAILED_EXIT_CODE);
            }
            (Mode::Confirm { .. }, UiResult::Secret(_)) => {
                drop(secure_input);
                std::process::exit(PROMPT_FAILED_EXIT_CODE);
            }
        }
        self.secret.zeroize();
        self.clipboard_cleared = false;
        self._secure_input = None;
    }
}

impl eframe::App for SecretBridgeUi {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        PAGE_BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(50));
        if self.mode.is_none() {
            // A secret-storage worker now owns the one-shot process lifetime. Keep the event
            // loop alive but the window hidden and click-through until that worker exits with
            // the operation result.
            return;
        }
        self.render(ui);
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
    }
}

fn semibold(text: &str, size: f32) -> RichText {
    RichText::new(text).font(FontId::new(
        size,
        egui::FontFamily::Name("system-semibold".into()),
    ))
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // Use the installed platform typeface, never redistribute Apple or Microsoft fonts.
    #[cfg(target_os = "macos")]
    let font_files = [
        ("system-regular", "/System/Library/Fonts/SFNS.ttf", 400.0),
        ("system-semibold", "/System/Library/Fonts/SFNS.ttf", 600.0),
    ];
    #[cfg(target_os = "windows")]
    let font_files = [
        ("system-regular", "C:\\Windows\\Fonts\\segoeui.ttf", 400.0),
        ("system-semibold", "C:\\Windows\\Fonts\\seguisb.ttf", 600.0),
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let font_files: [(&str, &str, f32); 0] = [];
    for (name, path, weight) in font_files {
        if let Ok(bytes) = std::fs::read(path) {
            let mut data = egui::FontData::from_owned(bytes);
            data.tweak.coords =
                egui::epaint::text::VariationCoords::new([("wght", weight), ("opsz", 14.0)]);
            fonts.font_data.insert(name.into(), data.into());
        }
    }
    let mut regular = fonts.families[&egui::FontFamily::Proportional].clone();
    if fonts.font_data.contains_key("system-regular") {
        regular.insert(0, "system-regular".into());
    }
    let mut bold = regular.clone();
    if fonts.font_data.contains_key("system-semibold") {
        bold.insert(0, "system-semibold".into());
    }
    fonts
        .families
        .insert(egui::FontFamily::Proportional, regular);
    fonts
        .families
        .insert(egui::FontFamily::Name("system-semibold".into()), bold);
    ctx.set_fonts(fonts);
}

pub(super) fn run_request_window(
    mode: Mode,
    secure_input: Option<SecureInputGuard>,
) -> Result<(), String> {
    let height = if matches!(mode, Mode::Secret { .. }) {
        WINDOW_HEIGHT_SECRET
    } else {
        WINDOW_HEIGHT_CONFIRM
    };
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
            .with_min_inner_size([380.0, 360.0])
            .with_resizable(true)
            .with_decorations(cfg!(target_os = "macos"))
            .with_fullsize_content_view(true)
            .with_title_shown(false)
            .with_titlebar_shown(false)
            .with_transparent(false)
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
        Box::new(move |creation_context| {
            configure_fonts(&creation_context.egui_ctx);
            Ok(Box::new(SecretBridgeUi::new(
                mode,
                secure_input,
                window_size,
            )))
        }),
    )
    .map_err(|error| format!("SecretBridge UI failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dismissal_disables_hit_testing_before_hiding_the_child_window() {
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
        let normal_level = command_index(&egui::ViewportCommand::WindowLevel(
            egui::WindowLevel::Normal,
        ));
        let hidden = command_index(&egui::ViewportCommand::Visible(false));

        assert!(passthrough < normal_level);
        assert!(normal_level < hidden);
        assert!(!commands.contains(&egui::ViewportCommand::Close));
    }
}
