# Dashboard rendering

The settings dashboard and Theme Studio share a Rust egui UI. Their default host
uses the existing vendored `egui-directx11` painter and a native D3D11 swap chain.
It tries `D3D_DRIVER_TYPE_HARDWARE` first, then Windows' WARP software rasterizer
if hardware device/swap-chain initialization fails. This does not replace the
original taskbar bar renderer or its authored theme.

WARP is **CPU software rendering**, not a GPU optimization. The diagnostic log
records the selected driver and adapter; do not infer hardware acceleration merely
from “D3D11”. Feature level 11.0 is required. Both D3D11 paths embed the shader-model-5
binaries in `vendor/egui-directx11/shaders`; no downloaded runtime, DirectX SDK,
shader compiler DLL or companion executable is required. Painter adaptations and
upstream attribution remain in `vendor/egui-directx11/PATCH.md`.

If D3D11 initialization fails on both paths, the dashboard releases its instance
guard and starts the same executable with `--dashboard-opengl`, retaining the
owner, initial page, original arguments and diagnostic logging. Winit permits
only one event loop per process, so this fallback uses a new process. The original
D3D11 error is carried in `UH_DASHBOARD_D3D11_ERROR`. The child claims the same
guard and never retries D3D11. Errors after the first successful D3D11 frame are
reported as runtime failures rather than silently restarting the application.

The fallback uses the existing eframe/OpenGL host. All paths share settings,
fonts, theme previews, native titlebar styling, account safeguards and
save-on-close behavior. D3D11 uses the patched egui-winit integration for keyboard,
pointer, IME, clipboard and file drops; secondary egui viewports are embedded in
the main window. Custom GPU paint callbacks are not used or supported by the
vendored painter.

## Repaint and preview work

Redraws are event driven. A minimized/zero-sized window does not paint or keep a
polling redraw loop alive; a restore/resize requests a fresh frame. An open
settings window still has timed updates for live usage/countdown state. Closing
settings removes its process; the resident taskbar process remains separate.

Theme and context-menu previews resolve host dimensions against the existing
monitor/taskbar geometry snapshot, lazily refreshed at most once per second in
the separate Studio process. Surface selection, placement, providers and language
are resolved from current state, not a cached whole `ThemeRuntime`. Context-menu
library discovery runs when its picker opens or a menu is saved rather than on
every paint. Asset lists use a shared snapshot invalidated by imports, deletion,
picker changes and entering the Assets page.

These changes reduce repeated work; a hardware backend alone does not eliminate
CPU layout, filesystem or native-window-query latency. See `HANDOFF.md` for the
latest **observed** machine-specific timings, not performance guarantees.

## Diagnostics and verification

For the local pinned GNU/LLVM target:

```powershell
.\target\x86_64-pc-windows-gnullvm\release\usagehub.exe --studio --diagnose
```

Normally omit renderer switches to exercise hardware-first selection. To force
software rendering use `--dashboard-warp`; to diagnose the eframe host directly
use `--dashboard-opengl`. The software path can consume more CPU than hardware.

For bounded frame samples, set `UH_UI_FRAME_TIMINGS=1` **only for the diagnostic
launch**, with `--diagnose`. The host records at most 512 successful frames:

```text
dashboard frame timing page=... ui_us=... paint_us=... present_us=... total_us=...
```

This option neither changes pages nor schedules extra frames. Navigate actual
tabs and distinguish CPU UI time from painting/Present/vsync and cold font or
preview initialization. Remove the environment variable after diagnosis. Normal
runs retain only bounded slow-frame diagnostics; the old automatic
`UH_PROFILE_PAGES` page-cycling harness is removed.

`cargo test --locked --quiet -- --test-threads=1` includes existing renderer
fallback/runtime-error selection, off-screen WARP texture readback, close
cancellation and geometry/preview state regressions. Passing tests does not prove
native focus, taskbar coverage, GPU selection or narrow-window interaction:
verify those on the real app. Existing disposable VM flows are described in
[Windows VM testing](windows-vm-testing.md); no Hyper-V run is implied by a local
hardware smoke.
