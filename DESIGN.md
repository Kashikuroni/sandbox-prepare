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

Native GPUI desktop UI supports operating projects, sandboxes, backups, and processes through compact editor-style chrome, restrained controls, and distinct editor/panel surfaces. Preserve native window dragging, double-click titlebar behavior, and keyboard operation. Workspace composition lives in `crates/workspace/src/lib.rs`; surface conventions live in its `projects_view.rs`, `sandboxes_view.rs`, and `settings_view.rs`. Shared form selectors, titlebar context pickers, inputs, and palette live in `crates/ui/src/select.rs`, `crates/ui/src/context_picker.rs`, `crates/ui/src/text_input.rs`, and `crates/ui/src/theme.rs`. Titlebar context composition and actions live in `crates/workspace/src/context_bar.rs`; the working-copy menu lives in `crates/workspace/src/source_menu.rs`. One working directory identifies one sandbox, including its code, configuration, databases, and process lifecycle. Labels derive from the branch/path rather than an independently entered sandbox name.

## Colors

`Palette` in `crates/ui/src/theme.rs` is authoritative. Base tokens are pinned to Zed Ayu Light and Ayu Mirage at commit `b1962f7cd8569ecade486c40320e3cff2d4b9d22`; log levels use a custom muted palette.
Light/Dark/System selects Ayu Light/Ayu Mirage/platform appearance. Active titlebars and status bars use chrome; inactive titlebars use panel background. Text and icons share their normal and muted roles.

| Token | Light | Mirage |
| --- | --- | --- |
| Border | `#cfd1d2` | `#53565d` |
| Focused border | `#c4daf6` | `#24556f` |
| Element hover | `#dfe0e1` | `#43464f` |
| Element selected | `#cfd0d2` | `#53565d` |
| UI error | `#ef7271` | `#f18779` |

Custom `Palette::log_*` roles render DEBUG/TRACE in gray, INFO in green, WARN/WARNING in orange, and ERROR in red. Read their exact values from `crates/ui/src/theme.rs`.

## Typography

UI inherits GPUI's default family and `text_base`; secondary descriptions, buttons, and status messages use `text_sm`. Section headings use `text_xl`, confirmation titles `text_lg`. Text inputs use 14 px; JSON uses Menlo with 22 px lines. Single-line inputs use 30 px lines. Log levels are bold and colored; message text retains the normal text color.

## Layout

Main window opens at 1100 × 720 with a 640 × 400 minimum and a 34 px titlebar. Its right-aligned 22 px settings button contains a 14 px Zed gear. On macOS, the titlebar reserves 78 px for native traffic lights positioned at (9, 9), following Zed's SDK 26+ layout; fullscreen and other platforms use 8 px left padding. Settings reuses its existing window, opening at 1000 × 740 with a 760 × 500 minimum.
The titlebar carries a compact project picker and one working-copy trigger, with labels that truncate within width budgets that shrink with the window. The working-copy parent menu is 300 px wide; nested branch/worktree menus are up to 320 px wide. Nested menus align with the invoking row, flip left when needed, and clamp within the viewport with an 8 px margin. The workspace has a fixed 220 px panel for section navigation only. Projects uses a scrollable form with 24 px inset, muted field labels above controls, thin section dividers, and compact Project / Launch tabs. Working-copy selection and preparation belong in the titlebar menu.
Settings has a fixed 240 px left panel: native drag/control area, search, then Appearance, Storage, Logging, Sandboxes. The right side has a 48 px draggable header with the form/JSON switch aligned right; there is no user/project label. Content scrolls below it with 24 px horizontal inset.
Setting rows place a flexible title/description column left and a fixed control right, separated by 24 px; rows have 20 px vertical padding and bottom dividers. Storage fields are 280 px wide; logging fields are 120 px. Save actions align right.
The main log panel opens on the right, defaults to 420 px, and resizes from its left edge with a 260 px minimum. Narrow windows hide navigation while logs are open; the resize edge changes cursor without a hover highlight.

## Elevation & Depth

Normal surfaces use tonal layering and thin borders. Selector popovers use panel background, occlusion, and `shadow_md`; confirmation dialogs add a 30% black backdrop and `shadow_lg` over an editor-colored surface.

## Shapes

Buttons, inputs, and selector triggers use GPUI `rounded_sm`; selector popovers and confirmation dialogs use `rounded_md`. Controls have 1 px borders; button borders are transparent until focused unless explicitly outlined.

## Components

Projects separates adding a project and its backup prefixes from configuring the selected working copy’s launch. Project setup accepts a local Git directory or remote clone. There is no New sandbox tab, source-creation form, or independent sandbox-name field. Existing database-only environments remain accessible through the titlebar menu. Launch discovers Compose files, records their merge order, and inspects the combined configuration before exposing profile/service toggles, database replacement names, environment-file import, database bindings, and bind mounts. Environment files import into SQLite for all services or one service without changing the source file. Bind mounts offer Source code, Read-only configuration, and Private sandbox data roles with editable paths. Save launch configuration is explicit. Drafts retain input entities, selections, inspected configuration, bindings, and mounts per `SandboxId` when switching context. Invalid bindings and save errors stay inline near their actions; general operation status and cancellation remain visible above the form. Unavailable actions are disabled.

Searchable GPUI selectors show a truncated current label and muted chevron in a 28 px outlined trigger. Their anchored panel stays within the window with an 8 px margin, at up to 320 px wide, with search above a scrollable option list capped at 240 px high. Options use 28 px rows, selected fill and an accent check; empty results say “No matches.” Enter/Space/Down opens, Up/Down moves the highlight, Enter selects, Escape closes and restores trigger focus, Tab closes while allowing traversal, and clicking outside dismisses. Filtering is case-insensitive. Search edits reset the highlighted row and scroll position. Use this outlined control for form and process choices; titlebar context uses the specialized picker below.

Titlebar project and working-copy styling follows Zed source at commit `6ceb6fee8738274589b59139df27efc191ec2231`, with shared metrics in `crates/ui/src/titlebar_style.rs`: default-density 22 px triggers (`ButtonSize::Default`), 12 px labels (`LabelSize::Small`), 12 px icons (`IconSize::XSmall`), and 4 px horizontal padding and icon gaps (`DynamicSpacing::Base04`). Bundled IBM Plex Sans Regular, Zed's `.ZedSans` family, applies to context pickers and their search inputs. Triggers have no border; hover and keyboard focus use `element_hover` fill, while open triggers use `border_focused` fill and `icon_accent` text/icons. The project label uses normal text; the working-copy label and icon are muted. Worktree and branch icons retain the original Zed assets pinned with the palette. Anchored menus retain 13 px text and 30 px option rows. The integrated 36 px search row shares the panel background, with a muted contextual placeholder and accent-blue caret; it has no magnifier, frame, or inner editor-colored fill. Menus show current/other groups where applicable, muted details and statuses, and an accent check for the selection. Search filters labels and details case-insensitively. Searchable project and nested source menus share one viewport height budget: search and footer actions remain fixed while the option area scrolls, capped at 300 px. Their panels are up to 320 px wide with an 8 px window margin. The working-copy parent scrolls as a whole within its viewport height budget. Enter/Space/Down opens; Up/Down moves through options and footer actions, Enter activates, Escape closes and restores trigger focus, Tab closes for traversal, and clicking outside dismisses.

The project menu offers Open Local Folder… through the native directory chooser, reusing an existing project for the same repository, and Clone Repository… through the project form. It groups the current project under “This Window” and other projects under “Recent Projects”. Recent projects record successful opens only, with one durable SQLite history entry per project ordered latest first and excluding the current project from the menu; migration does not backfill history from the project catalog. Startup restores the latest recorded project.

The working-copy menu starts with “Work In” and three 30 px rows: Isolated Worktree, Existing Local Checkout, and Existing Git Worktree. Isolated Worktree opens a searchable list of free branches; entering a new branch name exposes a footer action to create it. Existing Local Checkout selects the main checkout with its current branch. Existing Git Worktree opens a searchable list of branch labels and directory paths. Git metadata loads asynchronously; preparation uses the current Git state rather than treating the displayed snapshot as authoritative. Choosing a directory reuses its sandbox or prepares it, then opens Launch configuration when ready. It does not automatically restore backups or create containers. The menu shows the selected label, directory, runtime status, inline preparation errors, Cancel preparation while busy, and Configure launch… otherwise. Existing environments preserves access to legacy records without a source, unavailable sources, and conflicting records.

The working-copy parent menu supports Up/Down, Enter/Right activation, Escape dismissal, and Tab traversal. Left from a nested menu returns focus to its invoking row. Successful selection hands focus back to the stable titlebar trigger. These keyboard and focus behaviors are verified in source; live keyboard input has not been verified. Render nested popovers as sibling deferred overlays above the parent (priority 4), rather than nesting deferred draws.

Backups uses a compact two-row list with 16 px page inset and a shared Download/Update action beside a 28 px Zed refresh icon. Each database row shows its status, then Local/S3 dates and sizes; those groups wrap on narrow panes. Clicking the summary or pressing Enter/Space reveals source filenames, exact local timestamps with UTC offset, and the managed local path. Long detail lines scroll horizontally inside their row. S3 errors remain visible when collapsed. Transfer progress uses a 2 px bar and fixed-width tabular values updated once per second; archive verification and waiting for the pair appear in the row status. The introductory explanation is omitted. A separate Verify integrity action checks both local files with cancellation and throttled progress, also when S3 is unavailable. Startup/Refresh only stat local files (presence, size, mtime); changed files require explicit verification or download. Failed integrity results persist across restarts. Full checks hash compressed input while decoding it, in one read pass. Verification shows compressed bytes read and caps its percentage at 99% until all checks succeed.

Sandboxes uses the same 16 px inset and compact 28 px controls as Backups. The toolbar provides Refresh, Create/Recreate, Start, Stop and Delete; unavailable actions stay disabled. Two flat database rows show version, running/stopped state, host endpoints, restored table counts and source backups. Connection details copy both host endpoints and Docker-network aliases; “Copied” appears only after a successful copy. A visible stage and expanded-byte count describe restoration without invented percentages. Errors remain inline. Recreate and Delete use confirmations with Cancel initially focused, keyboard containment and Escape cancellation. Delete explicitly identifies database volumes and private application data as removed, while downloaded backups and Git sources are kept. Closing initiates cleanup while the window remains visible; a failure permits retrying exit. Processes is a separate virtualized bottom-aligned log view with a searchable All processes/per-process selector and Pause/Resume. Options include database processes and names observed in the selected sandbox’s output, including project services. One bounded memory buffer is shared across sandbox contexts; the displayed records stay scoped to the selected project and sandbox. Processes shares theme and Logging preferences. Database metrics and network inspection are later work.

Settings and form search inputs remain outlined, 28 px high, with the original Zed magnifying-glass icon and 20 px line height. Their carets, like other form input carets, use `text_muted` gray in both themes; only titlebar context search uses the accent-blue caret. Settings search matches section names, setting titles, and descriptions across all sections. Empty results say “No matching settings.” Selected navigation and theme choices use `element_selected`; buttons use `element_hover` on hover and `border_focused` for focus.
Appearance applies immediately. Backups are managed as one active local pair in the application data directory. A single Download/Update action fetches both databases in parallel, verifies archives one at a time as downloads finish, and publishes the pair before deleting older files. A completed download waiting for the verifier has its own row status. Failure or cancellation stops both workers before removing the incomplete pair. Each row shows remote metadata, local freshness and separate download/verification states. S3 failures retain local copies and show freshness as unknown. Save dialogs and the folder setting are absent. Progress values update once per second in fixed-width fields with tabular numerals; cancellation and completion remain responsive. Storage masks access and secret keys and saves through “Save connection”; credentials remain in SQLite. Logging saves each valid edited field automatically after a 300 ms debounce, without resetting text, selection, or focus. Invalid drafts show inline errors and do not alter saved values. Pending edits are flushed before navigation or close. Logging also has retention status, “Check now”, and “Clear all logs…”. Sandboxes settings uses the same debounced, inline-validated numeric control for Concurrent restores (default 2; positive values only). Feedback appears below the header, with errors using the UI error color.
“Edit settings.json” replaces the form with a multiline editor inside the same window; “Back to settings” returns. “Save JSON” validates and applies settings, then synchronizes forms. Credentials are edited in Storage.
Tab/Shift-Tab traverse settings controls; Enter/Space activate buttons. JSON Tab inserts indentation; Shift-Tab can leave the editor. Cmd/Ctrl+S saves the current form or JSON. Cmd/Ctrl+, opens settings from the workspace.
Closing settings or switching form/JSON destinations with unsaved edits opens a discard confirmation. Clearing logs requires a destructive confirmation. Cancel receives initial focus; Tab/Shift-Tab remain between the two modal buttons; Escape cancels. Destructive action text uses the UI error color.
The shared settings model propagates theme changes to the workspace, logs, backups, settings, and its inputs. System mode responds to native appearance changes; explicit Light/Dark remains fixed.
Log rows show local timestamp, bold colored level, first message line, and a right-side Details action. Details exposes the full record and Copy JSON. Current and History remain distinct views with category/level filters.

## Do's and Don'ts

- Do use semantic `Palette` roles and the existing Zed icon assets for consistent new controls.
- Do preserve settings label/description/control rows, visible focus borders, debounced numeric saves, explicit Storage/JSON/Launch saves, and confirmation focus containment.
- Do keep project and nested working-copy choices searchable and launch drafts isolated by sandbox; show operation and validation feedback inline.
- Do identify a sandbox by its working directory and derive its visible label from branch/path; keep working-copy selection in the titlebar and launch editing in Projects.
- Don't substitute UI error colors for the custom log severity colors or move credentials into general settings JSON.
