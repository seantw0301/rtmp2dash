# rtmp2dash

[English](./README.md)

純 Rust 的 **RTMP → live MPEG-DASH** 服務：以**本機推流（publish）**與／或**遠端拉流（pull）**取得直播，remux 成 DASH（`index.mpd` + CMAF/fMP4）。

- **Codec**：固定 H.264 + AAC（passthrough，不重編碼）
- **並行**：推流與拉流可同時運作；多 channel 並行（同一 channel 僅一來源）
- **執行期不依賴 ffmpeg**（測試腳本可選用 ffmpeg 產生樣本）

**授權：[MIT License](./LICENSE)** — 本專案依 MIT 開源。

## Build

需求：已安裝 [Rust](https://rustup.rs/)（`rustc` / `cargo`）。執行期不需 ffmpeg；僅煙霧測試腳本需要。

```bash
# 開發建置（debug）
cargo build

# 發行建置（建議）
cargo build --release
```

產出二元檔：

| 模式 | 路徑 |
|------|------|
| debug | `target/debug/rtmp2dash` |
| release | `target/release/rtmp2dash` |

直接執行建置結果：

```bash
./target/release/rtmp2dash --config config.yaml
```

`./script/start.sh` 會自動執行 `cargo build --release` 後再背景啟動。

## 快速開始

```bash
# 背景啟動 / 停止 / 重啟
./script/start.sh
./script/stop.sh
./script/restart.sh

# 或前景執行
cargo run --release -- --config config.yaml
```

推流與播放範例：

```bash
ffmpeg -re -i input.mp4 -c:v libx264 -g 60 -keyint_min 60 -sc_threshold 0 \
  -c:a aac -f flv rtmp://127.0.0.1:1935/live/demo

# 播放
open http://127.0.0.1:8080/live/demo/index.mpd
```

拉流範例（見 `config.yaml`；請改成你自己的來源 URL）：

```yaml
pull:
  - url: "rtmp://origin.example.com:1935/live/stream1"
    channel: "demo"
```

啟動後播放：`http://127.0.0.1:8080/live/demo/index.mpd`（檔名為標準 `.mpd`）。

煙霧測試（需 ffmpeg）：

```bash
./script/smoke_test.sh
```

## 用途

| 場景 | 說明 |
|------|------|
| 直播轉發（推流） | OBS / 編碼器以 RTMP 推入，輸出 live DASH |
| 直播轉發（拉流） | 由 `config.yaml` 指定遠端 RTMP URL，主動拉取並輸出 DASH |
| 多頻道 | `…/live/<channel_id>`；推流與拉流可並行 |
| 邊緣 / 自架 | 輕量單二元檔 + YAML，輸出寫入本機 cache |

## 設定摘要

見根目錄 [`config.yaml`](./config.yaml)。重點欄位：

| 欄位 | 說明 |
|------|------|
| `rtmp.port` | RTMP 監聽埠 |
| `dash.port` | DASH HTTP 埠 |
| `cache.dir` | 切片與 MPD 輸出目錄 |
| `cache.segment_duration_secs` | 切片長度（秒，**預設 2**） |

完整說明：[doc/config.md](./doc/config.md)

## 目錄

| 路徑 | 內容 |
|------|------|
| `src/` | 程式原始碼 |
| `doc/` | **全部文件** |
| `script/` | 啟動 / 停止 / 測試腳本 |
| `LICENSE` | MIT 授權 |

開發規範（含「單檔 ≤ 1000 行」）：[doc/layout.md](./doc/layout.md)

## 文件

- [文件索引](./doc/README.md)
- [架構](./doc/architecture.md)
- [用法](./doc/usage.md)
- [設定](./doc/config.md)

## 限制（目前版本）

- 僅 H.264 + AAC
- DASH 為 HTTP（HTTPS 後續可加）
- 播放清單檔名為標準 `index.mpd`
