# Startup benchmark

Target (design "Performance & app shell"): **cold start < 300 ms** to the first painted frame.

## How to measure

```sh
cargo build --release
powershell -File tools/startup-bench.ps1 -Runs 11            # default theme
powershell -File tools/startup-bench.ps1 -Runs 11 -Theme onyx
```

The app records phase marks from the top of `main` (`crates/app/src/startup.rs`). With
`GITDRAGON_STARTUP_EXIT=1` it prints `STARTUP tracing=… platform=… first_frame=…` after the first
frame and quits. The script reports run 1 separately ("first", file cache colder) and
min / median / max over the rest. `wall` is launch to process exit, so it also counts exe loading
and shutdown. Phase logs are also written on every normal start (`GITDRAGON_LOG=startup=info`).

| Phase | Ends when |
|---|---|
| `tracing` | logger is set up |
| `platform` | GPUI platform is up and the `run` closure starts (window system, GPU device, text system) |
| `kit_init` | `gpui_kit::init` (component globals, theme registry) is done |
| `theme` | both skins are registered and the start theme is applied |
| `window_open` | main window, dock area and panels are built |
| `first_frame` | first frame is painted (`Window::on_next_frame`) |

## Results

### 2026-10-08 · shell only (title bar, rail, 3 placeholder panels), no repo loaded

Machine: Windows 11, AMD Ryzen 7 5700X, 32 GB, NVIDIA RTX 4080 (D3D 11.1). Release profile
(thin LTO, codegen-units 1). gpui-kit 0.7.1 / gpui-pre 0.3.8, Rust 1.99.0. Binary 17.5 MB.

| Phase (ms since `main`) | first | min | median | max |
|---|---:|---:|---:|---:|
| platform | 174.7 | 174.5 | 182.6 | 197.7 |
| kit_init | 179.4 | 177.1 | 185.0 | 200.6 |
| theme | 179.5 | 177.2 | 185.1 | 200.7 |
| window_open | 237.8 | 227.9 | 233.9 | 254.8 |
| **first_frame** | 251.3 | 230.2 | **239.9** | 255.9 |
| wall (launch → exit) | 586.1 | 282.3 | 288.2 | 306.5 |

11 runs, Aurora dark. Onyx dark: first_frame median 236.6 ms (6 runs); theme choice costs nothing
measurable.

Debug build for comparison: first_frame ≈ 560 ms.

**Reading:** first frame is under target with ~60 ms to spare. About 75% of it is GPUI's platform
start (`platform`, ~180 ms: Direct3D device, DirectWrite, window class); our code (kit init, both
skins, window + docks) adds ~55 ms. The "first" run's wall time (586 ms) shows the cost of a colder
file cache. A true cold boot is not measured yet.

**Next things to watch:** repo open (< 500 ms target) once git-core lands; whether opening the
window in parallel with platform init is possible; the cold-boot number on a fresh login.
