//! Custom themes: three colors and a name, one JSON file each in the data dir's `themes/` folder.
//! Made in Settings → Appearance or dropped into the folder by hand.

use std::path::{Path, PathBuf};

use iced::Color;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomTheme {
    pub name: String,
    /// `#rrggbb`
    pub background: String,
    pub text: String,
    pub accent: String,
}

/// `#rrggbb` or `rrggbb` as a color.
pub fn hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(Color::from_rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

pub fn to_hex(c: Color) -> String {
    let [r, g, b, _] = c.into_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
}

impl CustomTheme {
    /// (background, text, accent), if all three are valid colors.
    pub fn colors(&self) -> Option<(Color, Color, Color)> {
        Some((hex(&self.background)?, hex(&self.text)?, hex(&self.accent)?))
    }

    pub fn is_valid(&self) -> bool {
        !self.name.trim().is_empty() && self.colors().is_some()
    }

    /// The file this theme is saved in: its name, lowercased, with anything unusual as '-'.
    fn file(&self, dir: &Path) -> PathBuf {
        let slug: String = self.name.trim().to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        dir.join(format!("{slug}.json"))
    }
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("themes")
}

/// Every valid theme in `dir`, by name; files that don't parse are skipped.
pub fn load(dir: &Path) -> Vec<CustomTheme> {
    let mut themes: Vec<CustomTheme> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|f| f.path().extension().is_some_and(|e| e == "json"))
        .filter_map(|f| serde_json::from_slice::<CustomTheme>(&std::fs::read(f.path()).ok()?).ok())
        .filter(CustomTheme::is_valid)
        .collect();
    themes.sort_by_key(|t| t.name.to_lowercase());
    themes.dedup_by(|a, b| a.name == b.name);
    themes
}

pub fn save(dir: &Path, t: &CustomTheme) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(t.file(dir), serde_json::to_vec_pretty(t).expect("plain struct"))
}

pub fn delete(dir: &Path, t: &CustomTheme) -> std::io::Result<()> {
    std::fs::remove_file(t.file(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors() {
        assert_eq!(hex("#ff8000"), Some(Color::from_rgb8(255, 128, 0)));
        assert_eq!(hex(" 2d353b "), Some(Color::from_rgb8(0x2d, 0x35, 0x3b)));
        assert_eq!(hex("#fff"), None);
        assert_eq!(hex("#gg0000"), None);
        assert_eq!(to_hex(Color::from_rgb8(0x2d, 0x35, 0x3b)), "#2d353b");
    }

    #[test]
    fn save_load_delete() {
        let dir = tempfile::tempdir().unwrap();
        let t = CustomTheme { name: "My Theme!".into(), background: "#101010".into(), text: "#eeeeee".into(), accent: "#ff8800".into() };
        save(dir.path(), &t).unwrap();
        assert!(dir.path().join("my-theme-.json").exists());
        std::fs::write(dir.path().join("broken.json"), "{").unwrap();
        let bad = CustomTheme { accent: "nope".into(), name: "Bad".into(), ..t.clone() };
        std::fs::write(dir.path().join("bad.json"), serde_json::to_vec(&bad).unwrap()).unwrap();
        assert_eq!(load(dir.path()), std::slice::from_ref(&t), "broken and invalid files are skipped");
        delete(dir.path(), &t).unwrap();
        assert!(load(dir.path()).is_empty());
    }
}
