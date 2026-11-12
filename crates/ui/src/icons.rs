use std::borrow::Cow;

use gpui::{AssetSource, SharedString, Styled, Svg, px, svg};

#[derive(Clone, Copy)]
pub enum Icon {
    Archive,
    Search,
    Debug,
    Refresh,
    ChevronDown,
    Check,
    Terminal,
    Box,
    Settings,
    SidebarOpen,
    SidebarClosed,
}

impl Icon {
    fn path(self) -> &'static str {
        match self {
            Self::Search => "icons/zed/magnifying_glass.svg",
            Self::Refresh => "icons/zed/rotate_cw.svg",
            Self::ChevronDown => "icons/zed/chevron_down.svg",
            Self::Check => "icons/zed/check.svg",
            Self::Debug => "icons/zed/debug.svg",
            Self::Archive => "icons/zed/archive.svg",
            Self::Terminal => "icons/zed/terminal.svg",
            Self::Box => "icons/zed/box.svg",
            Self::Settings => "icons/zed/settings.svg",
            Self::SidebarOpen => "icons/zed/threads_sidebar_left_open.svg",
            Self::SidebarClosed => "icons/zed/threads_sidebar_left_closed.svg",
        }
    }

    pub fn render(self, color: u32) -> Svg {
        svg()
            .path(self.path())
            .size(px(16.0))
            .flex_shrink_0()
            .text_color(gpui::rgb(color))
    }
}

// Embed assets so the executable also works outside the project directory.
const ASSETS: &[(&str, &[u8])] = &[
    (
        "icons/zed/magnifying_glass.svg",
        include_bytes!("../../../assets/icons/zed/magnifying_glass.svg"),
    ),
    (
        "icons/zed/rotate_cw.svg",
        include_bytes!("../../../assets/icons/zed/rotate_cw.svg"),
    ),
    (
        "icons/zed/chevron_down.svg",
        include_bytes!("../../../assets/icons/zed/chevron_down.svg"),
    ),
    (
        "icons/zed/check.svg",
        include_bytes!("../../../assets/icons/zed/check.svg"),
    ),
    (
        "icons/zed/debug.svg",
        include_bytes!("../../../assets/icons/zed/debug.svg"),
    ),
    (
        "icons/zed/archive.svg",
        include_bytes!("../../../assets/icons/zed/archive.svg"),
    ),
    (
        "icons/zed/terminal.svg",
        include_bytes!("../../../assets/icons/zed/terminal.svg"),
    ),
    (
        "icons/zed/box.svg",
        include_bytes!("../../../assets/icons/zed/box.svg"),
    ),
    (
        "icons/zed/settings.svg",
        include_bytes!("../../../assets/icons/zed/settings.svg"),
    ),
    (
        "icons/zed/threads_sidebar_left_open.svg",
        include_bytes!("../../../assets/icons/zed/threads_sidebar_left_open.svg"),
    ),
    (
        "icons/zed/threads_sidebar_left_closed.svg",
        include_bytes!("../../../assets/icons/zed/threads_sidebar_left_closed.svg"),
    ),
    (
        "icons/zed/LICENSES",
        include_bytes!("../../../assets/icons/zed/LICENSES"),
    ),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        Ok(ASSETS
            .iter()
            .find(|(asset_path, _)| *asset_path == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(ASSETS
            .iter()
            .filter(|(asset_path, _)| asset_path.starts_with(path))
            .map(|(asset_path, _)| (*asset_path).into())
            .collect())
    }
}
