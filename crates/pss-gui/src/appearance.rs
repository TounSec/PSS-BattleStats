//! User-configurable lookk: a light/dark/system mode, + optional overrides (fonts, size, colors).
//! Every override is `None` by default, meaning "whatever the mode gives"

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, Theme, ThemePreference,
};
use serde::{Deserialize, Serialize};

use crate::palette;

/// egui's default `Body` size, the reference for scalling the other text styles
const DEFAULT_SIZE: f32 = 14.0;
pub const SIZE_RANGE: std::ops::RangeInclusive<f32> = 9.0..=28.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    System,
    Light,
    Dark,
}

impl Mode {
    fn preference(self) -> ThemePreference {
        match self {
            Mode::System => ThemePreference::System,
            Mode::Light => ThemePreference::Light,
            Mode::Dark => ThemePreference::Dark,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub mode: Mode,
    pub font: Option<PathBuf>,
    pub mono_font: Option<PathBuf>,
    pub size: Option<f32>,
    pub accent: Option<[u8; 3]>,
    pub panel_fill: Option<[u8; 3]>,
    pub text: Option<[u8; 3]>,
    pub win: Option<[u8; 3]>,
    pub loss: Option<[u8; 3]>,
}

fn color(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

impl Appearance {
    /// A missing or unreadable file just means "defaults"
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    pub fn has_overrides(&self) -> bool {
        Self {
            mode: self.mode,
            ..Self::default()
        } != *self
    }

    /// Pushes the settings into egui. Cheap enough to call on every change while previewing; the
    /// fonts are only rebuilt when the chosen font files actually changed
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.set_theme(self.mode.preference());

        let fonts_key = (self.font.clone(), self.mono_font.clone());
        let fonts_id = egui::Id::new("appearance_fonts_key");
        let last: Option<(Option<PathBuf>, Option<PathBuf>)> = ctx.data(|d| d.get_temp(fonts_id));

        if last.as_ref() != Some(&fonts_key) {
            ctx.set_fonts(self.font_definitions());
            ctx.data_mut(|d| d.insert_temp(fonts_id, fonts_key));
        }

        // Both themes get the overrides, so switching day/night keeps the user's choices
        let scale = self.size.unwrap_or(DEFAULT_SIZE) / DEFAULT_SIZE;
        for theme in [Theme::Light, Theme::Dark] {
            let mut visuals = match theme {
                Theme::Light => egui::Visuals::light(),
                Theme::Dark => egui::Visuals::dark(),
            };

            if let Some(rgb) = self.accent {
                visuals.selection.bg_fill = color(rgb);
                visuals.hyperlink_color = color(rgb);
            }
            if let Some(rgb) = self.panel_fill {
                visuals.panel_fill = color(rgb);
            }
            if let Some(rgb) = self.text {
                visuals.override_text_color = Some(color(rgb));
            }

            ctx.set_visuals_of(theme, visuals);
            ctx.style_mut_of(theme, |style| {
                for (text_style, font) in egui::Style::default().text_styles {
                    style
                        .text_styles
                        .insert(text_style, FontId::new(font.size * scale, font.family));
                }
            });
        }

        palette::set_outcome_colors(self.win, self.loss);
    }

    /// egui's built-in fonts, with the user's files put first. The built-ins stay as fallbacks,
    /// player and alliance names use all sorts of characters a custom font may not cover
    fn font_definitions(&self) -> FontDefinitions {
        let mut defs = FontDefinitions::default();

        for (family, path, name) in [
            (FontFamily::Proportional, &self.font, "custom-ui"),
            (FontFamily::Monospace, &self.mono_font, "custom-mono"),
        ] {
            let Some(bytes) = path.as_ref().and_then(|p| std::fs::read(p).ok()) else {
                continue;
            };

            defs.font_data
                .insert(name.to_string(), Arc::new(FontData::from_owned(bytes)));
            defs.families
                .entry(family)
                .or_default()
                .insert(0, name.to_string());
        }

        defs
    }
}

/// Name of the font egui uses when nothing is overridden, for the "inherited" label
fn inherited_font_name(family: FontFamily) -> String {
    FontDefinitions::default()
        .families
        .get(&family)
        .and_then(|names| names.first().cloned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- window

#[derive(Default)]
pub struct Window {
    open: bool,
    /// What was applied when the window opened, restored on cancel
    original: Appearance,
}

pub enum Action {
    None,
    /// A setting changed: re-apply for a live preview
    Changed,
    /// "Apply": the caller saves the file
    Applied,
    /// "Cancelled" or the close button: the caller restores this value
    Cancelled(Appearance),
}

impl Window {
    pub fn open(&mut self, current: &Appearance) {
        self.original = current.clone();
        self.open = true;
    }

    pub fn show(&mut self, ctx: &egui::Context, a: &mut Appearance, file: &Path) -> Action {
        if !self.open {
            return Action::None;
        }

        let mut open = true;
        let mut action = Action::None;

        egui::Window::new(tr!("appearance.title"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                let before = a.clone();

                ui.horizontal(|ui| {
                    ui.strong(tr!("appearance.mode"));
                    ui.weak(tr!("appearance.mode_hint"));
                });
                ui.columns(3, |cols| {
                    for (col, (mode, key)) in cols.iter_mut().zip([
                        (Mode::System, "appearance.system"),
                        (Mode::Light, "appearance.light"),
                        (Mode::Dark, "appearance.dark"),
                    ]) {
                        let selected = a.mode == mode;
                        let button = egui::Button::new(tr!(key)).selected(selected);

                        if col
                            .add_sized([col.available_width(), 28.0], button)
                            .clicked()
                        {
                            a.mode = mode;
                        }
                    }
                });

                if a.mode == Mode::System {
                    let key = match ctx.system_theme() {
                        Some(Theme::Dark) => "appearance.system_dark",
                        Some(Theme::Light) => "appearance.system_light",
                        None => "appearance.system_unknown",
                    };

                    ui.weak(tr!(key));
                }

                ui.separator();

                ui.horizontal(|ui| {
                    ui.strong(tr!("appearance.custom"));
                    ui.weak(tr!("appearance.custom_hint"));
                });

                egui::Grid::new("appearance_grid")
                    .num_columns(3)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        font_row(
                            ui,
                            &tr!("appearance.font"),
                            &mut a.font,
                            &inherited_font_name(FontFamily::Proportional),
                        );
                        font_row(
                            ui,
                            &tr!("appearance.mono_font"),
                            &mut a.mono_font,
                            &inherited_font_name(FontFamily::Monospace),
                        );

                        ui.label(tr!("appearance.size"));
                        let mut size = a.size.unwrap_or(DEFAULT_SIZE);

                        if ui
                            .add(egui::Slider::new(&mut size, SIZE_RANGE).fixed_decimals(1))
                            .changed()
                        {
                            a.size = Some(size);
                        }

                        reset_button(ui, &mut a.size);
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.label(tr!("appearance.colors"));
                let visuals = ui.visuals().clone();

                color_row(
                    ui,
                    &tr!("appearance.accent"),
                    &mut a.accent,
                    visuals.selection.bg_fill,
                );
                color_row(
                    ui,
                    &tr!("appearance.panel"),
                    &mut a.panel_fill,
                    visuals.panel_fill,
                );
                color_row(
                    ui,
                    &tr!("appearance.text"),
                    &mut a.text,
                    visuals.text_color(),
                );

                outcome_row(ui, &mut a.win, &mut a.loss);

                ui.separator();

                ui.horizontal(|ui| {
                    ui.weak(tr!("appearance.saved_in"));
                    ui.monospace(file.display().to_string());
                });

                let mut preview = toml::to_string_pretty(&*a).unwrap_or_default();

                if !a.has_overrides() {
                    preview.push_str(&tr!("appearance.no_override"));
                }

                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.monospace(preview.trim_end());
                });

                if *a != before {
                    action = Action::Changed;
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button(tr!("appearance.clear")).clicked() {
                        *a = Appearance {
                            mode: a.mode,
                            ..Appearance::default()
                        };

                        action = Action::Changed;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(tr!("appearance.apply")).clicked() {
                            action = Action::Applied;
                        }
                        if ui.button(tr!("appearance.cancel")).clicked() {
                            action = Action::Cancelled(self.original.clone());
                        }
                    });
                });
            });

        // The window's own close button counts as a cancel
        if !open && matches!(action, Action::None | Action::Changed) {
            action = Action::Cancelled(self.original.clone());
        }
        if matches!(action, Action::Applied | Action::Cancelled(_)) {
            self.open = false;
        }

        action
    }
}

fn reset_button<T>(ui: &mut egui::Ui, value: &mut Option<T>) {
    if ui
        .add_enabled(value.is_some(), egui::Button::new("-").small())
        .clicked()
    {
        *value = None;
    }
}

fn font_row(ui: &mut egui::Ui, label: &str, value: &mut Option<PathBuf>, inherited: &str) {
    ui.label(label);

    let shown = match value {
        Some(path) => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        None => tr!("appearance.inherited_font", name = inherited),
    };

    egui::ComboBox::from_id_salt(label)
        .selected_text(shown)
        .width(260.0)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    value.is_none(),
                    tr!("appearance.inherited_font", name = inherited),
                )
                .clicked()
            {
                *value = None;
            }

            if ui.button(tr!("appearance.pick_font")).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Font", &["ttf", "otf"])
                    .pick_file()
            {
                *value = Some(path);
                ui.close();
            }
        });

    reset_button(ui, value);
    ui.end_row();
}

/// A checkbox that switches the override on, plus the swatch when it is
fn color_row(ui: &mut egui::Ui, label: &str, value: &mut Option<[u8; 3]>, inherited: Color32) {
    ui.horizontal(|ui| {
        let mut enabled = value.is_some();

        if ui.checkbox(&mut enabled, label).changed() {
            *value = enabled.then(|| [inherited.r(), inherited.g(), inherited.b()]);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match value {
                Some(rgb) => {
                    ui.color_edit_button_srgb(rgb);
                }
                None => {
                    ui.weak(tr!("appearance.inherited"));
                    // Same size as the real button, just not editable
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(40.0, 18.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 3.0, inherited);
                }
            }
        });
    });
}

fn outcome_row(ui: &mut egui::Ui, win: &mut Option<[u8; 3]>, loss: &mut Option<[u8; 3]>) {
    ui.horizontal(|ui| {
        let mut enabled = win.is_some() || loss.is_some();

        if ui
            .checkbox(&mut enabled, tr!("appearance.outcome"))
            .changed()
        {
            if enabled {
                let w = palette::win();
                let l = palette::loss();
                *win = Some([w.r(), w.g(), w.b()]);
                *loss = Some([l.r(), l.g(), l.b()]);
            } else {
                *win = None;
                *loss = None;
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(rgb) = loss {
                ui.color_edit_button_srgb(rgb);
            }
            if let Some(rgb) = win {
                ui.color_edit_button_srgb(rgb);
            } else {
                ui.weak(tr!("appearance.inherited"));
            }
        });
    });
}
