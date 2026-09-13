# gpui-shell — Parley GPUI rewrite spike

A time-boxed spike evaluating a [GPUI](https://www.gpui.rs/) rewrite of
Parley UI (currently Tauri 2 + Next.js/React). Standalone
crate, **not** a member of the root Cargo workspace — it has its own empty
`[workspace]` table, its own `Cargo.lock` (committed), and its own
`target/` (gitignored). Nothing under `frontend/` was touched.

## Running it

```bash
cd spikes/gpui-shell
cargo run                 # debug build, opens the window
cargo run --release        # release build
cargo run -- --roundtrip   # headless-ish zorite round-trip check, exits 0/1
cargo test                  # no #[test]s in this spike; exists for completeness
```

> **Environment note (this machine only):** this Fedora box has
> `libxkbcommon-x11.so.0` but not the `-devel` package (no `.so` symlink), so
> the final link step fails with `unable to find library -lxkbcommon-x11`
> unless a fix is applied. Either install `xkbcommon-x11-devel` (needs root,
> unavailable in this sandbox) or point the linker at a symlink:
> ```bash
> mkdir -p /tmp/spike-libs
> ln -sf /lib64/libxkbcommon-x11.so.0 /tmp/spike-libs/libxkbcommon-x11.so
> RUSTFLAGS="-L /tmp/spike-libs" cargo build
> ```
> This is a local system-packaging gap, not a project issue — a normal dev
> box with the `-devel` package installed needs no workaround.

## The four checks

1. **Shell** (`src/main.rs`, `SpikeShell`) — client-side-decorated window
   (`WindowDecorations::Client` + `TitleBar::window_options()`), a gpui-kit
   `TitleBar` + `Root` + collapsible `Sidebar` switching between the four
   check views, and a light/dark theme toggle button (top-right of the title
   bar, calls `Theme::change`). **Worked well.** The sidebar/title-bar/theme
   system is complete and ergonomic once you find the right import paths.

2. **Live transcript** (`src/recording.rs`, `RecordingView`) — a
   `MessageScrollerState`-backed virtualized list fed by a `smol::Timer` loop
   every ~600ms, appending a fake transcript segment (speaker label,
   timestamp, 1-4 sentences) or mutating the last row in place ~60% of the
   time when it's still "partial" (simulating Whisper's partial->final
   revision). Tail-following, stop-on-scroll-up, and the jump-to-latest
   button are **all built into `MessageScroller`/`MessageScrollerState`** —
   no custom scroll-position bookkeeping needed. A two-channel level meter
   (`Arc<AtomicU32>` mic/system, updated by a background `std::thread` faking
   sine+noise) is read only inside `render()`, driven by a separate ~30fps
   timer that just calls `cx.notify()`. **Worked well** and validates the
   intended pattern: no 60fps Tauri-style event stream, just cheap repaint
   requests plus atomic reads gated by whether the view is actually mounted.

3. **Summary editor** (`src/summary.rs` + `src/db.rs`) — a `zorite-editor`
   `EditorState` with WYSIWYG markdown styling (`SyntaxStyle` built from
   `cx.theme()` tokens). Tries `~/.local/share/io.github.hankanman.Parley/meeting_minutes.sqlite`
   first: opens it `?mode=ro` via `rusqlite` (`bundled` feature — no system
   libsqlite3 needed), queries `summary_processes` for the most recently
   `completed` row, and parses the `result` JSON's `markdown` field (schema
   confirmed against `frontend/src-tauri/migrations/20250916100000_initial_schema.sql`
   and `20251101000000_add_summary_backup.sql`; verified against the real DB
   on this machine — a `meeting-*` row with real markdown). Falls back to the
   bundled `fixtures/summary.md` (H1-H3, bold/italic, ordered/bulleted lists,
   nested task lists, a 3-column table, a blockquote, inline code) if the DB
   is missing or has no completed summary. "Save" writes the editor's
   current markdown to `$XDG_RUNTIME_DIR/parley-gpui-spike-summary.md` (or
   `/tmp` as a fallback) — **never** back to the database.

4. **Tray** (`src/tray.rs`) — a `gpui-tray` `StatusNotifierItem` with Start
   recording / Stop recording (toggles a `SharedRecordingState` global also
   reflected in the "Tray" check view), Show window (logged; gpui-tray on
   Linux has no window-activate hook exposed to call from here — genuine
   gap, not a bug), and Quit. Confirmed live in the smoke run: the desktop's
   SNI host queried `GetAll`/`GetLayout` over D-Bus and the menu dispatched
   correctly.

## Resolved versions

| Crate | Version | Source |
| --- | --- | --- |
| `gpui-pre` (aka `gpui`) | 0.3.4 | crates.io, via gpui-kit |
| `gpui-kit` | 0.6.1 | crates.io |
| `zorite-editor` | 0.10.0 | crates.io |
| `gpui-tray` | 0.1.0 | **git**, `domenkozar/gpui-tray` rev `dfcd45b4bd82963abcee9e6cc35f83f7a4fa6c03` |

## Build numbers (this machine: 32 logical cores, NVIDIA RTX 3080 Ti)

- Cold `cargo build` (debug, from an empty `target/`): **52.1s** wall
  (571s user / 92s system — heavily parallel).
- `cargo build --release`: **1m 32s** wall.
- Debug binary: 723 MB (unstripped, full debuginfo).
- Release binary: **54 MB** (40 MB stripped).
- `cargo test`: 0 tests defined (none needed for this spike's scope; the
  round-trip check is a CLI flag instead, per the task brief).

## Round-trip check

`cargo run -- --roundtrip` loads `fixtures/summary.md` into a fresh
`EditorState::with_text(...)`, reads `EditorState::value()` back out, and
diffs byte-for-byte against the original:

```
[roundtrip] OK: 1830 bytes, identical after EditorState round-trip
```

**Finding:** zorite-editor treats markdown source as its own document model
— it does not parse to an AST and re-serialize. `set_text`/`with_text` store
the raw string; WYSIWYG rendering (headings, nested task lists, tables,
etc.) is a live *view* over that string, and edits are byte-range splices
back into it (`replace_range`). So the round trip is exact by construction
for *any* input, including the nested task lists and the table in the
fixture — there's no lossy normalization step to test against, unlike a
typical markdown-it -> AST -> markdown-it round trip. That's a genuinely nice
property for a notes/summary editor (no reformatting surprises), but it also
means this particular check couldn't surface parser bugs the way it might
for an AST-based editor — the meaningful correctness signal for zorite would
instead be "does the WYSIWYG *view* mis-render a construct," which needs an
on-screen check, not a text diff.

## Findings

**What worked well**
- gpui-kit's `Sidebar` + `TitleBar` + `Root` + `Theme` stack is complete,
  well-documented via examples, and produces a native-feeling shell with
  little code — closer to "assemble components" than "hand-roll layout,"
  which is the opposite of what I expected walking in.
- `MessageScrollerState`/`MessageScroller` solved tail-following,
  stop-on-scroll, and jump-to-latest completely out of the box — this was
  the single biggest risk area for the live-transcript UI and it needed zero
  custom code.
- The `Arc<AtomicU32>` + poll-on-render pattern for the level meter is simple
  and cheap; no channel/event plumbing, no risk of the UI thread being
  flooded by a fast audio thread.
- `zorite-editor`'s provider model (`set_markdown_style`, block providers)
  is well-documented (`API.md` is genuinely excellent) and the "source is
  the model" design is a good fit for a markdown notes editor.
- `rusqlite` with `bundled` reads the real, live Parley DB read-only
  with no coordination needed with the running Tauri app (WAL-mode SQLite
  tolerates concurrent readers fine).

**What didn't / friction hit**
- **gpui-tray's `gpui-kit` feature is not on crates.io yet** (0.1.0 there
  only has `default`/`menu-state`; `gpui-kit` is on the repo's `main` branch
  only). Worked around with a pinned git dependency — acceptable for a
  spike, would need the maintainer to cut a release (or this project vendor
  a fork) before shipping on it.
- **Icon catalog is two-tiered and easy to trip on**: `gpui_kit::component::IconName`
  (used by `Button`/`SidebarMenuItem`) is a *curated subset* of the full
  Lucide catalog in `gpui_kit_assets::IconName` (used by `Icon::path`
  directly). `IconName::Mic` compiles against the full catalog but not the
  curated one, so `SidebarMenuItem::icon(IconName::Mic)` fails to resolve
  with a confusing "not found in this scope" until you check the generated
  `icon_name.rs` build output. Not a blocker, just a paper cut — the curated
  list should probably be documented inline or the two enums unified.
- **Import surface is non-obvious for glob users**: `gpui_kit::*` re-exports
  gpui-pre itself, but `h_flex`/`v_flex`/`StyledExt`/`ActiveTheme` live in
  `gpui_kit::component` and must be imported explicitly — mixing the two
  glob imports (as the examples do) works, but if you only import
  `gpui_kit::*` (as I first tried, since it "has everything"), `div()`
  compiles but `h_flex()`/`.font_semibold()` silently don't, with an error
  message that *does* point at the fix, just not obviously.
- **`AsyncApp::update` doesn't return `Result`** in this gpui-pre version
  (unlike the real zed `gpui`, where `cx.update()` on an async context
  returns `Result<T, AppClosed>` and the examples I found online chain
  `.ok()` on it) — it just runs and returns `T` directly. Minor API surprise
  when porting patterns from zed-flavoured examples.
- **Tray "Show window" has no obvious Linux hook** from gpui-tray's public
  API in this spike — there's no `Window::activate()`/focus-request call
  reachable from the tray's action handler without holding a `WindowHandle`
  captured at open time (which this spike does via a closure, logging the
  intent instead of actually focusing, since gpui-pre's window-activation
  API wasn't explored further within the time box).
- No crashes, panics, or hangs were hit anywhere in this spike — everything
  above is either a version-pinning workaround or minor ergonomics friction,
  not a correctness problem.

## What to look at manually

Since this ran headless-adjacent in an agent sandbox (a window *did* briefly
appear on the real Wayland desktop during the smoke test — GNOME/Wayland,
NVIDIA RTX 3080 Ti, Vulkan backend selected automatically), a human should:

- **Feel/perf**: `cargo run --release` and compare window-open latency,
  resize smoothness, and general responsiveness against the Tauri app.
- **Recording view**: watch the transcript actually scroll for 30s+, scroll
  up mid-stream to confirm it stops following and the jump-to-latest button
  fades in/out, then click it. Watch the two level-meter bars animate.
- **Summary view**: confirm it loaded your *real* latest meeting summary
  (the header says "Loaded from database: <title>") rather than the
  fixture, edit some text, click Save, and check
  `$XDG_RUNTIME_DIR/parley-gpui-spike-summary.md` (or `/tmp/...`) landed.
- **Theme toggle**: click the sun/moon button in the title bar and confirm
  every view re-themes live, including the editor's WYSIWYG colors.
- **Tray**: find the tray icon (a small red circle) in your desktop's
  status area and drive recording state from there; confirm the "Tray"
  check view mirrors it.
