use eframe::egui::{self, Color32, Rounding, Stroke, Visuals};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

pub const DEFAULT_GRUVBOX_CSS: &str = r#"/* lan-audio Ricing Theme (Waybar / GTK @define-color syntax)
 * Edit this file while `lan-audio` GUI is running — changes hot-reload live!
 */

/* Colors (gruvbox) */
@define-color black	#282828;
@define-color red	#cc241d;
@define-color green	#98971a;
@define-color yellow	#d79921;
@define-color blue	#458588;
@define-color purple	#b16286;
@define-color aqua	#689d6a;
@define-color gray	#a89984;
@define-color brgray	#928374;
@define-color brred	#fb4934;
@define-color brgreen	#b8bb26;
@define-color bryellow	#fabd2f;
@define-color brblue	#83a598;
@define-color brpurple	#d3869b;
@define-color braqua	#8ec07c;
@define-color white	#ebdbb2;
@define-color bg2	#504945;

/* Semantic roles */
@define-color warning 	@bryellow;
@define-color critical	@red;
@define-color mode	@black;
@define-color unfocused	@bg2;
@define-color focused	@braqua;
@define-color inactive	@purple;
@define-color sound	@brpurple;
@define-color network	@purple;
@define-color memory	@braqua;
@define-color cpu	@green;
@define-color temp	@brgreen;
@define-color layout	@bryellow;
@define-color gputemp	@aqua;
@define-color date	@black;
@define-color time	@white;

/* Optional geometry / ricing tweaks */
border-radius: 0;
border-width: 1.5;
font-scale: 1.0;
"#;

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct RiceTheme {
    pub black: Color32,
    pub red: Color32,
    pub green: Color32,
    pub yellow: Color32,
    pub blue: Color32,
    pub purple: Color32,
    pub aqua: Color32,
    pub gray: Color32,
    pub brgray: Color32,
    pub brred: Color32,
    pub brgreen: Color32,
    pub bryellow: Color32,
    pub brblue: Color32,
    pub brpurple: Color32,
    pub braqua: Color32,
    pub white: Color32,
    pub bg2: Color32,

    pub warning: Color32,
    pub critical: Color32,
    pub mode: Color32,
    pub unfocused: Color32,
    pub focused: Color32,
    pub inactive: Color32,
    pub sound: Color32,
    pub network: Color32,
    pub memory: Color32,
    pub cpu: Color32,
    pub temp: Color32,
    pub layout: Color32,
    pub gputemp: Color32,
    pub date: Color32,
    pub time: Color32,

    pub border_radius: f32,
    pub border_width: f32,
    pub font_scale: f32,
}

impl Default for RiceTheme {
    fn default() -> Self {
        Self::from_css(DEFAULT_GRUVBOX_CSS)
    }
}

fn parse_hex_color(s: &str) -> Option<Color32> {
    let hex = s.trim().trim_start_matches('#');
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color32::from_rgb(r, g, b))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(r, g, b, a))
        }
        _ => None,
    }
}

impl RiceTheme {
    pub fn from_css(css: &str) -> Self {
        let mut raw_defs: HashMap<String, String> = HashMap::new();
        let mut border_radius = 0.0f32;
        let mut border_width = 1.5f32;
        let mut font_scale = 1.0f32;

        // Strip block comments /* ... */
        let mut cleaned = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            cleaned.push_str(&rest[..start]);
            if let Some(end) = rest[start + 2..].find("*/") {
                rest = &rest[start + 2 + end + 2..];
            } else {
                rest = "";
                break;
            }
        }
        cleaned.push_str(rest);

        for line in cleaned.lines() {
            let l = line.trim().trim_end_matches(';').trim();
            if l.is_empty() || l.starts_with("//") || l.starts_with('#') {
                continue;
            }
            if let Some(after) = l.strip_prefix("@define-color") {
                let mut parts = after.split_whitespace();
                if let (Some(name), Some(val)) = (parts.next(), parts.next()) {
                    raw_defs.insert(name.trim().to_string(), val.trim().to_string());
                }
            } else if let Some((k, v)) = l.split_once(':') {
                let key = k.trim();
                let val = v.trim().trim_end_matches("px").trim();
                match key {
                    "border-radius" => {
                        if let Ok(n) = val.parse::<f32>() {
                            border_radius = n.clamp(0.0, 24.0);
                        }
                    }
                    "border-width" => {
                        if let Ok(n) = val.parse::<f32>() {
                            border_width = n.clamp(0.0, 6.0);
                        }
                    }
                    "font-scale" => {
                        if let Ok(n) = val.parse::<f32>() {
                            font_scale = n.clamp(0.7, 2.0);
                        }
                    }
                    _ => {}
                }
            }
        }

        let resolve = |key: &str, fallback: Color32| -> Color32 {
            let mut cur = key;
            for _ in 0..8 {
                if let Some(val) = raw_defs.get(cur) {
                    if let Some(ref_name) = val.strip_prefix('@') {
                        cur = ref_name.trim();
                    } else if let Some(c) = parse_hex_color(val) {
                        return c;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            fallback
        };

        let black = resolve("black", Color32::from_rgb(0x28, 0x28, 0x28));
        let red = resolve("red", Color32::from_rgb(0xcc, 0x24, 0x1d));
        let green = resolve("green", Color32::from_rgb(0x98, 0x97, 0x1a));
        let yellow = resolve("yellow", Color32::from_rgb(0xd7, 0x99, 0x21));
        let blue = resolve("blue", Color32::from_rgb(0x45, 0x85, 0x88));
        let purple = resolve("purple", Color32::from_rgb(0xb1, 0x62, 0x86));
        let aqua = resolve("aqua", Color32::from_rgb(0x68, 0x9d, 0x6a));
        let gray = resolve("gray", Color32::from_rgb(0xa8, 0x99, 0x84));
        let brgray = resolve("brgray", Color32::from_rgb(0x92, 0x83, 0x74));
        let brred = resolve("brred", Color32::from_rgb(0xfb, 0x49, 0x34));
        let brgreen = resolve("brgreen", Color32::from_rgb(0xb8, 0xbb, 0x26));
        let bryellow = resolve("bryellow", Color32::from_rgb(0xfa, 0xbd, 0x2f));
        let brblue = resolve("brblue", Color32::from_rgb(0x83, 0xa5, 0x98));
        let brpurple = resolve("brpurple", Color32::from_rgb(0xd3, 0x86, 0x9b));
        let braqua = resolve("braqua", Color32::from_rgb(0x8e, 0xc0, 0x7c));
        let white = resolve("white", Color32::from_rgb(0xeb, 0xdb, 0xb2));
        let bg2 = resolve("bg2", Color32::from_rgb(0x50, 0x49, 0x45));

        Self {
            black,
            red,
            green,
            yellow,
            blue,
            purple,
            aqua,
            gray,
            brgray,
            brred,
            brgreen,
            bryellow,
            brblue,
            brpurple,
            braqua,
            white,
            bg2,
            warning: resolve("warning", bryellow),
            critical: resolve("critical", red),
            mode: resolve("mode", black),
            unfocused: resolve("unfocused", bg2),
            focused: resolve("focused", braqua),
            inactive: resolve("inactive", purple),
            sound: resolve("sound", brpurple),
            network: resolve("network", purple),
            memory: resolve("memory", braqua),
            cpu: resolve("cpu", green),
            temp: resolve("temp", brgreen),
            layout: resolve("layout", bryellow),
            gputemp: resolve("gputemp", aqua),
            date: resolve("date", black),
            time: resolve("time", white),
            border_radius,
            border_width,
            font_scale,
        }
    }

    pub fn apply_to_egui(&self, ctx: &egui::Context) {
        let mut style = (*ctx.style()).clone();
        let rounding = Rounding::same(self.border_radius);

        let mut visuals = Visuals::dark();
        visuals.override_text_color = Some(self.time);
        visuals.panel_fill = self.black;
        visuals.window_fill = self.black;
        visuals.faint_bg_color = Color32::from_rgb(0x32, 0x30, 0x2f);
        visuals.extreme_bg_color = Color32::from_rgb(0x1d, 0x20, 0x21);
        visuals.window_rounding = rounding;
        visuals.menu_rounding = rounding;
        visuals.window_stroke = Stroke::new(self.border_width, self.unfocused);

        visuals.selection.bg_fill = self.focused.linear_multiply(0.28);
        visuals.selection.stroke = Stroke::new(self.border_width, self.focused);

        // Non-interactive widgets
        visuals.widgets.noninteractive.bg_fill = self.black;
        visuals.widgets.noninteractive.weak_bg_fill = self.black;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(self.border_width, self.unfocused);
        visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, self.time);
        visuals.widgets.noninteractive.rounding = rounding;

        // Inactive interactive widgets
        visuals.widgets.inactive.bg_fill = self.bg2;
        visuals.widgets.inactive.weak_bg_fill = self.bg2;
        visuals.widgets.inactive.bg_stroke = Stroke::new(self.border_width, self.unfocused);
        visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, self.white);
        visuals.widgets.inactive.rounding = rounding;

        // Hovered widgets
        visuals.widgets.hovered.bg_fill = self.bg2;
        visuals.widgets.hovered.weak_bg_fill = self.bg2;
        visuals.widgets.hovered.bg_stroke = Stroke::new(self.border_width, self.focused);
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.5_f32, self.white);
        visuals.widgets.hovered.rounding = rounding;

        // Active / clicked widgets
        visuals.widgets.active.bg_fill = self.focused;
        visuals.widgets.active.weak_bg_fill = self.focused;
        visuals.widgets.active.bg_stroke = Stroke::new(self.border_width, self.white);
        visuals.widgets.active.fg_stroke = Stroke::new(1.5_f32, self.black);
        visuals.widgets.active.rounding = rounding;

        // Open combo-box / menu
        visuals.widgets.open.bg_fill = self.bg2;
        visuals.widgets.open.weak_bg_fill = self.bg2;
        visuals.widgets.open.bg_stroke = Stroke::new(self.border_width, self.layout);
        visuals.widgets.open.fg_stroke = Stroke::new(1.5_f32, self.white);
        visuals.widgets.open.rounding = rounding;

        style.visuals = visuals;
        ctx.set_style(style);
    }
}

pub struct ThemeWatcher {
    pub path: PathBuf,
    pub theme: RiceTheme,
    last_mtime: Option<SystemTime>,
    last_check: Instant,
}

impl ThemeWatcher {
    pub fn new(custom_path: Option<PathBuf>) -> Self {
        let path = custom_path.unwrap_or_else(default_style_path);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&path, DEFAULT_GRUVBOX_CSS);
        }

        let (theme, mtime) = load_from_path(&path);
        Self {
            path,
            theme,
            last_mtime: mtime,
            last_check: Instant::now(),
        }
    }

    /// Hot-reload `~/.config/lan-audio/style.css` if modified on disk
    pub fn poll_reload(&mut self, ctx: &egui::Context) -> bool {
        if self.last_check.elapsed().as_millis() < 500 {
            return false;
        }
        self.last_check = Instant::now();

        if let Ok(meta) = fs::metadata(&self.path) {
            if let Ok(mtime) = meta.modified() {
                if self.last_mtime != Some(mtime) {
                    let (new_theme, _) = load_from_path(&self.path);
                    self.theme = new_theme;
                    self.last_mtime = Some(mtime);
                    self.theme.apply_to_egui(ctx);
                    return true;
                }
            }
        }
        false
    }
}

fn load_from_path(path: &PathBuf) -> (RiceTheme, Option<SystemTime>) {
    let mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
    if let Ok(content) = fs::read_to_string(path) {
        (RiceTheme::from_css(&content), mtime)
    } else {
        (RiceTheme::default(), mtime)
    }
}

pub fn default_style_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("lan-audio").join("style.css");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("lan-audio")
            .join("style.css");
    }
    PathBuf::from("style.css")
}
