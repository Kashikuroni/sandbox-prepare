---
name: Sandbox Prepare
description: Native GPUI desktop utility with Zed Ayu surfaces and compact controls.
colors:
  light-editor: "#fcfcfc"
  light-panel: "#ececed"
  light-chrome: "#dcddde"
  light-text: "#5c6166"
  light-muted: "#8b8e92"
  light-accent: "#3b9ee5"
  dark-editor: "#242835"
  dark-panel: "#353944"
  dark-chrome: "#464a52"
  dark-text: "#cccac2"
  dark-muted: "#9a9a98"
  dark-accent: "#72cffe"
---

# Design System: Sandbox Prepare

## Overview

Native GPUI desktop UI uses compact editor-style chrome, restrained controls, and distinct editor/panel surfaces. Preserve native window dragging, double-click titlebar behavior, and keyboard operation. Source conventions live in `src/main.rs`, `src/settings_view.rs`, `src/text_input.rs`, and `src/theme.rs`.

## Colors

`Palette` in `src/theme.rs` is authoritative. Base tokens are pinned to Zed Ayu Light and Ayu Mirage at commit `b1962f7cd8569ecade486c40320e3cff2d4b9d22`; log levels use a custom muted palette.
Light/Dark/System selects Ayu Light/Ayu Mirage/platform appearance. Active titlebars and status bars use chrome; inactive titlebars use panel background. Text and icons share their normal and muted roles.

| Token | Light | Mirage |
| --- | --- | --- |
| Border | `#cfd1d2` | `#53565d` |
| Focused border | `#c4daf6` | `#24556f` |
| Element hover | `#dfe0e1` | `#43464f` |
| Element selected | `#cfd0d2` | `#53565d` |
| UI error | `#ef7271` | `#f18779` |

Custom `Palette::log_*` roles render DEBUG/TRACE in gray, INFO in green, WARN/WARNING in orange, and ERROR in red. Read their exact values from `src/theme.rs`.

## Typography

UI inherits GPUI's default family and `text_base`; secondary descriptions, buttons, and status messages use `text_sm`. Section headings use `text_xl`, confirmation titles `text_lg`. Text inputs use 14 px; JSON uses Menlo with 22 px lines. Single-line inputs use 30 px lines. Log levels are bold and colored; message text retains the normal text color.

## Layout

Main window opens at 1100 × 720 with a 640 × 400 minimum and a 40 px titlebar. Its right-aligned 28 px Zed gear opens settings. Settings reuses its existing window, opening at 1000 × 740 with a 760 × 500 minimum.
Settings has a fixed 240 px left panel: native drag/control area, search, then Appearance, Storage, Logging. The right side has an 88 px header with a User badge, `sandbox-prepare`, and the form/JSON switch. Content scrolls below it with 24 px horizontal inset.
Setting rows place a flexible title/description column left and a fixed control right, separated by 24 px; rows have 20 px vertical padding and bottom dividers. Storage fields are 280 px wide; logging fields are 120 px. Save actions align right.
The main log panel opens on the right, defaults to 420 px, and resizes from its left edge with a 260 px minimum. Narrow windows hide navigation while logs are open; the resize edge changes cursor without a hover highlight.

## Elevation & Depth

Normal surfaces use tonal layering and thin borders. Confirmation dialogs add a 30% black backdrop, occlusion, and `shadow_lg` over an editor-colored surface.

## Shapes

Buttons, inputs, and the User badge use GPUI `rounded_sm`; confirmation dialogs use `rounded_md`. Controls have 1 px borders; button borders are transparent until focused unless explicitly outlined.

## Components

Settings search matches section names, setting titles, and descriptions across all sections. Empty results say “No matching settings.” Selected navigation and theme choices use `element_selected`; buttons use `element_hover` on hover and `border_focused` for focus.
Appearance applies immediately. Storage masks access and secret keys and saves through “Save connection”; credentials remain in SQLite. Logging has an explicit save, retention status, “Check now”, and “Clear all logs…”. Feedback appears below the header, with errors using the UI error color.
“Edit settings.json” replaces the form with a multiline editor inside the same window; “Back to settings” returns. “Save JSON” validates and applies settings, then synchronizes forms. Credentials are edited in Storage.
Tab/Shift-Tab traverse settings controls; Enter/Space activate buttons. JSON Tab inserts indentation; Shift-Tab can leave the editor. Cmd/Ctrl+S saves the current form or JSON. Cmd/Ctrl+, opens settings from the workspace.
Closing settings or switching form/JSON destinations with unsaved edits opens a discard confirmation. Clearing logs requires a destructive confirmation. Cancel receives initial focus; Tab/Shift-Tab remain between the two modal buttons; Escape cancels. Destructive action text uses the UI error color.
The shared settings model propagates theme changes to the workspace, logs, backups, settings, and its inputs. System mode responds to native appearance changes; explicit Light/Dark remains fixed.
Log rows show local timestamp, bold colored level, first message line, and a right-side Details action. Details exposes the full record and Copy JSON. Current and History remain distinct views with category/level filters.

## Do's and Don'ts

- Do use semantic `Palette` roles and the existing Zed icon assets for consistent new controls.
- Do preserve label/description/control rows, visible focus borders, explicit form saves, and confirmation focus containment.
- Don't substitute UI error colors for the custom log severity colors or move credentials into general settings JSON.
