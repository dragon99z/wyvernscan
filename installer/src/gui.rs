//! The installer window: an options screen, then a result screen.
use crate::{admin_enabled, app_folder_in, install, install_dir, theme, valid_dir, Options, MENU_LABEL};
use eframe::egui::{self, RichText};
use std::path::{Path, PathBuf};

// Raw RGBA baked in by assets/make_icon.py, so no image decoder is needed (same as the app).
const ICON_256: &[u8] = include_bytes!("../../assets/icon-256.rgba");
const ICON_96: &[u8] = include_bytes!("../../assets/icon-96.rgba");

enum Screen {
    Options,
    /// Installed; holds the warnings for any extra that couldn't be set up.
    Done(Vec<String>),
    Failed(String),
}

struct App {
    /// The install folder as typed; checked when Install is pressed.
    path: String,
    path_error: Option<String>,
    /// Where it was installed, for the result screen.
    dir: PathBuf,
    opts: Options,
    screen: Screen,
    logo: egui::TextureHandle,
}

pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("WyvernScan Setup")
            .with_icon(egui::IconData { rgba: ICON_256.to_vec(), width: 256, height: 256 })
            .with_inner_size([460.0, 400.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "WyvernScan Setup",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Box::new(App::new(&cc.egui_ctx))
        }),
    )
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let image = egui::ColorImage::from_rgba_unmultiplied([96, 96], ICON_96);
        let logo = ctx.load_texture("wyvernscan-logo", image, egui::TextureOptions::LINEAR);
        let (dir, screen) = match install_dir() {
            Ok(dir) => (dir, Screen::Options),
            Err(e) => (PathBuf::new(), Screen::Failed(e.to_string())),
        };
        // Pre-ticked from an earlier install, so a reinstall doesn't quietly undo it.
        let admin = admin_enabled(&dir);
        let path = dir.display().to_string();
        Self { path, path_error: None, dir, opts: Options { menu: true, desktop: false, admin }, screen, logo }
    }

    fn install(&mut self) {
        let dir = match valid_dir(&self.path) {
            Ok(dir) => dir,
            Err(e) => return self.path_error = Some(e.to_string()),
        };
        self.screen = match install(&dir, &self.opts) {
            Ok(warnings) => Screen::Done(warnings),
            Err(e) => Screen::Failed(e.to_string()),
        };
        self.dir = dir;
    }

    fn ui(&mut self, ui: &mut egui::Ui) {
        use theme::colors::*;
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(self.logo.id(), egui::vec2(56.0, 56.0))));
            ui.vertical(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("WyvernScan Setup").heading().color(ACCENT));
                ui.label(RichText::new("See what's eating your disk - fast.").color(MUTED));
            });
        });
        ui.separator();
        ui.add_space(4.0);

        // Buttons set these; acting on them after the match keeps `self.screen` unborrowed.
        let (mut install_now, mut close) = (false, false);
        match &self.screen {
            Screen::Options => {
                ui.label(RichText::new("Install to").color(MUTED).small());
                ui.horizontal(|ui| {
                    let browse = ui.button("Browse...").clicked();
                    let field = egui::TextEdit::singleline(&mut self.path).font(egui::TextStyle::Monospace);
                    if ui.add_sized([ui.available_width(), 24.0], field).changed() {
                        self.path_error = None;
                    }
                    if browse {
                        // Blocks while the system dialog is open, like the app's own pickers.
                        let start = Path::new(&self.path).parent().filter(|p| p.is_dir());
                        let mut dialog = rfd::FileDialog::new().set_title("Choose where to install WyvernScan");
                        if let Some(start) = start {
                            dialog = dialog.set_directory(start);
                        }
                        if let Some(picked) = dialog.pick_folder() {
                            self.path = app_folder_in(picked).display().to_string();
                            self.path_error = None;
                        }
                    }
                });
                if let Some(e) = &self.path_error {
                    ui.colored_label(DANGER, e);
                }
                ui.add_space(12.0);
                if let Some(label) = MENU_LABEL {
                    ui.checkbox(&mut self.opts.menu, label);
                }
                ui.checkbox(&mut self.opts.desktop, "Create a desktop shortcut");
                #[cfg(windows)]
                {
                    ui.checkbox(&mut self.opts.admin, "Always run WyvernScan as administrator");
                    ui.label(
                        RichText::new(
                            "Needed for the fast NTFS scan. Windows asks for permission on every start, \
                             and starting it from a non-admin terminal will not work.",
                        )
                        .small()
                        .color(MUTED),
                    );
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        close = ui.button("Cancel").clicked();
                        // The theme's "strong" colour is dark (it is for pressed buttons), so set both.
                        let primary = egui::Button::new(RichText::new("Install").strong().color(BG)).fill(ACCENT);
                        install_now = ui.add(primary).clicked();
                    });
                });
            }
            Screen::Done(warnings) => {
                ui.label(RichText::new("WyvernScan is installed.").strong().color(OTHER_ACCENT));
                ui.label(RichText::new(self.dir.display().to_string()).monospace().color(MUTED));
                ui.add_space(8.0);
                for w in warnings {
                    ui.colored_label(ACCENT, w);
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
                    ui.add_space(4.0);
                    close = ui.button("Close").clicked();
                });
            }
            Screen::Failed(msg) => {
                ui.label(RichText::new("Installation failed.").strong().color(DANGER));
                ui.label(msg);
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
                    ui.add_space(4.0);
                    close = ui.button("Close").clicked();
                });
            }
        }
        if install_now {
            self.install();
        }
        if close {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| self.ui(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No window needed: run real frames through egui's context. Catches layout panics,
    /// and drives the same `install` the button calls.
    #[test]
    fn every_screen_draws_and_install_reaches_done() {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx);
        let dir = std::env::temp_dir().join(format!("wyvernscan-gui-test-{}", std::process::id()));
        app.opts = Options { menu: false, desktop: false, admin: false };
        let frame = |app: &mut App| {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.ui(ui));
            });
        };

        frame(&mut app); // options screen
        app.path = "relative/folder".into(); // refused, and the window stays on the options screen
        app.install();
        assert!(app.path_error.is_some() && matches!(app.screen, Screen::Options));
        frame(&mut app);
        app.path = dir.display().to_string(); // a typed, non-default path
        app.install();
        assert!(matches!(&app.screen, Screen::Done(w) if w.is_empty()) && app.dir == dir);
        // On macOS the app is inside the bundle, not directly in the folder.
        let exe = if cfg!(target_os = "macos") {
            dir.join("Contents/MacOS/wyvernscan")
        } else {
            dir.join(if cfg!(windows) { "wyvernscan.exe" } else { "wyvernscan" })
        };
        assert!(exe.exists(), "{} was not installed", exe.display());
        frame(&mut app);
        app.screen = Screen::Done(vec!["Desktop shortcut could not be set up: boom".into()]);
        frame(&mut app);
        app.screen = Screen::Failed("boom".into());
        frame(&mut app);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
