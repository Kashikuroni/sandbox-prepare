//! Zed titlebar metrics at its default 16 px UI scale and default density.
//!
//! Source: https://github.com/zed-industries/zed/tree/6ceb6fee8738274589b59139df27efc191ec2231
//! title_bar + platform_title_bar, ui/{components/{button,label,icon},utils/constants}.
//! `.ZedSans` resolves to IBM Plex Sans (assets/settings/default.json).
use gpui::App;
use std::borrow::Cow;

pub const FONT_FAMILY: &str = "IBM Plex Sans";
pub const LABEL_SIZE: f32 = 12.; // LabelSize::Small -> TextSize::Small
pub const BUTTON_HEIGHT: f32 = 22.; // ButtonSize::Default
pub const ICON_SIZE: f32 = 12.; // IconSize::XSmall
pub const BUTTON_PADDING: f32 = 4.; // DynamicSpacing::Base04
pub const ICON_GAP: f32 = 4.; // DynamicSpacing::Base04
pub const TITLEBAR_HEIGHT: f32 = 34.; // max(1.75 * 16, 34)
pub const TRAFFIC_LIGHT_PADDING: f32 = 78.; // Zed's macOS SDK 26+ layout

pub fn register_font(cx: &App) -> anyhow::Result<()> {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(include_bytes!(
            "../../../assets/fonts/zed/IBMPlexSans-Regular.ttf"
        ))])
}
