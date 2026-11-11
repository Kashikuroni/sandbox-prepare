use gpui::WindowAppearance;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::System => "System",
        }
    }

    pub fn palette(self, appearance: WindowAppearance) -> Palette {
        match self {
            Self::Light => Palette::AYU_LIGHT,
            Self::Dark => Palette::AYU_MIRAGE,
            Self::System => match appearance {
                WindowAppearance::Light | WindowAppearance::VibrantLight => Palette::AYU_LIGHT,
                WindowAppearance::Dark | WindowAppearance::VibrantDark => Palette::AYU_MIRAGE,
            },
        }
    }
}

/// Base UI tokens from Zed's Ayu Light and Ayu Mirage themes, matching the reference editor.
/// https://github.com/zed-industries/zed/blob/b1962f7cd8569ecade486c40320e3cff2d4b9d22/assets/themes/ayu/ayu.json
/// Log levels use a custom muted palette for each theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub editor_background: u32,
    pub panel_background: u32,
    pub title_bar_background: u32,
    pub title_bar_inactive_background: u32,
    pub status_bar_background: u32,
    pub border: u32,
    pub border_focused: u32,
    pub text: u32,
    pub text_muted: u32,
    pub icon: u32,
    pub icon_muted: u32,
    pub icon_accent: u32,
    pub element_background: u32,
    pub element_hover: u32,
    pub element_selected: u32,
    pub error: u32,
    pub log_debug: u32,
    pub log_info: u32,
    pub log_warning: u32,
    pub log_error: u32,
}

impl Palette {
    pub const AYU_LIGHT: Self = Self {
        editor_background: 0xfcfcfc,
        panel_background: 0xececed,
        title_bar_background: 0xdcddde,
        title_bar_inactive_background: 0xececed,
        status_bar_background: 0xdcddde,
        border: 0xcfd1d2,
        border_focused: 0xc4daf6,
        text: 0x5c6166,
        text_muted: 0x8b8e92,
        icon: 0x5c6166,
        icon_muted: 0x8b8e92,
        icon_accent: 0x3b9ee5,
        element_background: 0xececed,
        element_hover: 0xdfe0e1,
        element_selected: 0xcfd0d2,
        error: 0xef7271,
        log_debug: 0x656a70,
        log_info: 0x6f9283,
        log_warning: 0x92602f,
        log_error: 0xa15353,
    };

    pub const AYU_MIRAGE: Self = Self {
        editor_background: 0x242835,
        panel_background: 0x353944,
        title_bar_background: 0x464a52,
        title_bar_inactive_background: 0x353944,
        status_bar_background: 0x464a52,
        border: 0x53565d,
        border_focused: 0x24556f,
        text: 0xcccac2,
        text_muted: 0x9a9a98,
        icon: 0xcccac2,
        icon_muted: 0x9a9a98,
        icon_accent: 0x72cffe,
        element_background: 0x353944,
        element_hover: 0x43464f,
        element_selected: 0x53565d,
        error: 0xf18779,
        log_debug: 0xa4a7ad,
        log_info: 0xa7c6a0,
        log_warning: 0xd7b58a,
        log_error: 0xe6a0a0,
    };

    pub fn log_level_color(self, level: &str) -> u32 {
        match level {
            "INFO" => self.log_info,
            "WARN" | "WARNING" => self.log_warning,
            "ERROR" => self.log_error,
            _ => self.log_debug,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_follows_all_platform_appearances() {
        for appearance in [WindowAppearance::Light, WindowAppearance::VibrantLight] {
            assert_eq!(ThemeMode::System.palette(appearance), Palette::AYU_LIGHT);
        }
        for appearance in [WindowAppearance::Dark, WindowAppearance::VibrantDark] {
            assert_eq!(ThemeMode::System.palette(appearance), Palette::AYU_MIRAGE);
        }
    }

    #[test]
    fn explicit_modes_ignore_system_changes() {
        for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
            assert_eq!(ThemeMode::Light.palette(appearance), Palette::AYU_LIGHT);
            assert_eq!(ThemeMode::Dark.palette(appearance), Palette::AYU_MIRAGE);
        }
    }
}
