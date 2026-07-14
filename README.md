# rtmp2dash

[中文](./README_TW.md)

Pure-Rust **RTMP → live MPEG-DASH** service: ingest live video via **local publish** and/or **remote pull**, then remux to DASH (`index.mpd` + CMAF/fMP4).

- **Codec**: H.264 + AAC only (passthrough, no re-encode)
- **Concurrency**: publish and pull can run together; multiple channels in parallel (one source per channel)
- **No ffmpeg at runtime** (optional for test scripts that generate sample streams)

**License: [MIT License](./LICENSE)**

## Build

Requires [Rust](https://rustup.rs/) (`rustc` / `cargo`). ffmpeg is not needed at runtime; only the smoke-test script uses it.

```bash
# Debug build
cargo build

# Release build (recommended)
cargo build --release
```

Binary output:

| Mode | Path |
|------|------|
| debug | `target/debug/rtmp2dash` |
| release | `target/release/rtmp2dash` |

Run the built binary:

```bash
./target/release/rtmp2dash --config config.yaml
```

`./script/start.sh` runs `cargo build --release` and then starts the process in the background.

## Quick start

```bash
# Background start / stop / restart
./script/start.sh
./script/stop.sh
./script/restart.sh

# Or run in the foreground
cargo run --release -- --config config.yaml
```

Publish and play example:

```bash
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -keyint_min 60 -sc_threshold 0 \
  -c:a aac -f flv rtmp://127.0.0.1:6136/live/demo

# Play
open http://127.0.0.1:8080/live/demo/index.mpd
```

Pull example (see `config.yaml`; use your own origin URL):

```yaml
pull:
  - url: "rtmp://origin.example.com:1935/live/stream1"
    channel: "demo"
```

After start, play: `http://127.0.0.1:8080/live/demo/index.mpd` (standard `.mpd` filename).

Smoke test (requires ffmpeg):

```bash
./script/smoke_test.sh
```

## Use cases

| Scenario | Description |
|----------|-------------|
| Live relay (publish) | OBS / encoder pushes RTMP in; output live DASH |
| Live relay (pull) | Remote RTMP URL in `config.yaml`; pull and output DASH |
| Multi-channel | `…/live/<channel_id>`; publish and pull can run in parallel |
| Edge / self-host | Lightweight single binary + YAML; writes to local cache |

## Config summary

See [`config.yaml`](./config.yaml) in the repo root. Key fields:

| Field | Description |
|-------|-------------|
| `rtmp.port` | RTMP listen port |
| `dash.port` | DASH HTTP port |
| `cache.dir` | Segment and MPD output directory |
| `cache.segment_duration_secs` | Segment length in seconds (**default 2**) |

Full reference: [doc/config.md](./doc/config.md)

## Layout

| Path | Contents |
|------|----------|
| `src/` | Source code |
| `doc/` | **All documentation** |
| `script/` | Start / stop / test scripts |
| `LICENSE` | MIT license |

Dev conventions (including “≤ 1000 lines per file”): [doc/layout.md](./doc/layout.md)

## Docs

- [Doc index](./doc/README.md)
- [Architecture](./doc/architecture.md)
- [Usage](./doc/usage.md)
- [Config](./doc/config.md)

## Limitations (current version)

- H.264 + AAC only
- DASH over HTTP (HTTPS can be added later)
- Playlist filename is standard `index.mpd`
