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
Settings has a fixed 240 px left panel: native drag/control area, search, then Appearance, Storage, Logging. The right side has a 48 px draggable header with the form/JSON switch aligned right; there is no user/project label. Content scrolls below it with 24 px horizontal inset.
Setting rows place a flexible title/description column left and a fixed control right, separated by 24 px; rows have 20 px vertical padding and bottom dividers. Storage fields are 280 px wide; logging fields are 120 px. Save actions align right.
The main log panel opens on the right, defaults to 420 px, and resizes from its left edge with a 260 px minimum. Narrow windows hide navigation while logs are open; the resize edge changes cursor without a hover highlight.

## Elevation & Depth

Normal surfaces use tonal layering and thin borders. Confirmation dialogs add a 30% black backdrop, occlusion, and `shadow_lg` over an editor-colored surface.

## Shapes

Buttons and inputs use GPUI `rounded_sm`; confirmation dialogs use `rounded_md`. Controls have 1 px borders; button borders are transparent until focused unless explicitly outlined.

## Components

Backups uses a compact two-row list with 16 px page inset and a shared Download/Update action beside a 28 px Zed refresh icon. Each database row shows its status, then Local/S3 dates and sizes; those groups wrap on narrow panes. Clicking the summary or pressing Enter/Space reveals source filenames, exact local timestamps with UTC offset, and the managed local path. Long detail lines scroll horizontally inside their row. S3 errors remain visible when collapsed. Transfer progress uses a 2 px bar and fixed-width tabular values updated once per second; archive verification and waiting for the pair appear in the row status. The introductory explanation is omitted. A separate Verify integrity action checks both local files with cancellation and throttled progress, also when S3 is unavailable. Startup/Refresh only stat local files (presence, size, mtime); changed files require explicit verification or download. Failed integrity results persist across restarts. Full checks hash compressed input while decoding it, in one read pass. Verification shows compressed bytes read and caps its percentage at 99% until all checks succeed.

Sandboxes uses the same 16 px inset and compact 28 px controls as Backups. The toolbar provides Refresh, Create/Recreate, Start, Stop and Delete; unavailable actions stay disabled. Two flat database rows show version, running/stopped state, host endpoints, restored table counts and source backups. Connection details copy both host endpoints and Docker-network aliases. A visible stage and expanded-byte count describe restoration without invented percentages. Errors remain inline. Recreate and Delete use confirmations with Cancel initially focused, keyboard containment and Escape cancellation. Closing initiates cleanup while the window remains visible; a failure permits retrying exit. Processes is a separate virtualized bottom-aligned log view with All/MySQL/ClickHouse/Restore filters and Pause/Resume, sharing theme and Logging preferences. Database metrics and network inspection are later work.

The 28 px search input uses the original Zed magnifying-glass icon and 20 px line height. Input carets use `text_muted` gray in both themes. Search matches section names, setting titles, and descriptions across all sections. Empty results say “No matching settings.” Selected navigation and theme choices use `element_selected`; buttons use `element_hover` on hover and `border_focused` for focus.
Appearance applies immediately. Backups are managed as one active local pair in the application data directory. A single Download/Update action fetches both databases, verifies both archives, and publishes the pair before deleting older files. Each row shows remote metadata, local freshness and separate download/verification states. S3 failures retain local copies and show freshness as unknown. Save dialogs and the folder setting are absent. Progress values update once per second in fixed-width fields with tabular numerals; cancellation and completion remain responsive. Storage masks access and secret keys and saves through “Save connection”; credentials remain in SQLite. Logging saves each valid edited field automatically after a 300 ms debounce, without resetting text, selection, or focus. Invalid drafts show inline errors and do not alter saved values. Pending edits are flushed before navigation or close. Logging also has retention status, “Check now”, and “Clear all logs…”. Feedback appears below the header, with errors using the UI error color.
“Edit settings.json” replaces the form with a multiline editor inside the same window; “Back to settings” returns. “Save JSON” validates and applies settings, then synchronizes forms. Credentials are edited in Storage.
Tab/Shift-Tab traverse settings controls; Enter/Space activate buttons. JSON Tab inserts indentation; Shift-Tab can leave the editor. Cmd/Ctrl+S saves the current form or JSON. Cmd/Ctrl+, opens settings from the workspace.
Closing settings or switching form/JSON destinations with unsaved edits opens a discard confirmation. Clearing logs requires a destructive confirmation. Cancel receives initial focus; Tab/Shift-Tab remain between the two modal buttons; Escape cancels. Destructive action text uses the UI error color.
The shared settings model propagates theme changes to the workspace, logs, backups, settings, and its inputs. System mode responds to native appearance changes; explicit Light/Dark remains fixed.
Log rows show local timestamp, bold colored level, first message line, and a right-side Details action. Details exposes the full record and Copy JSON. Current and History remain distinct views with category/level filters.

## Do's and Don'ts

- Do use semantic `Palette` roles and the existing Zed icon assets for consistent new controls.
- Do preserve label/description/control rows, visible focus borders, automatic Logging saves, explicit Storage/JSON saves, and confirmation focus containment.
- Don't substitute UI error colors for the custom log severity colors or move credentials into general settings JSON.
