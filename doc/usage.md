# 用法

## 建置

```bash
cargo build --release
```

## 啟動 / 停止

建議使用 `script/` 下一鍵腳本（會建置 release 並背景執行）：

```bash
./script/start.sh          # 背景啟動，日誌 script/rtmp2dash.log
./script/stop.sh           # 停止
./script/restart.sh        # 停止後再啟動
```

前景執行：

```bash
cargo run --release -- --config config.yaml
```

健康檢查：

```bash
curl -s http://127.0.0.1:8080/healthz
```

查詢目前有效（有 ingest lease）的 channel：

```bash
curl -s http://127.0.0.1:8080/channels
# {"channels":[{"id":"demo","mpd":"/live/demo/index.mpd"}]}
```

## 拉流

於 `config.yaml` 設定 `pull` 後啟動服務即可，無需另開指令：

```yaml
pull:
  - url: "rtmp://origin.example.com:1935/live/stream1"
    channel: "demo"
```

播放：`http://127.0.0.1:8080/live/demo/index.mpd`

## 推流（ffmpeg 範例）

推流端請使用 H.264 + AAC，並設定穩定的 GOP（建議約等於切片長度的倍數）：

```bash
ffmpeg -re -i input.mp4 \
  -c:v libx264 -pix_fmt yuv420p -g 60 -keyint_min 60 -sc_threshold 0 \
  -c:a aac -f flv \
  rtmp://127.0.0.1:6136/live/demo
```

OBS：伺服器 `rtmp://127.0.0.1:6136/live`，串流金鑰填 `demo`（即 channel id）。

可同時推多路，例如另一路用 `…/live/demo2`。

## 播放

用支援 MPEG-DASH 的播放器開啟：

```text
http://127.0.0.1:8080/live/demo/index.mpd
```

**VLC**：選「媒體 → 開啟網路串流」。若仍無畫面，確認服務仍在推/拉流（`curl` 該 MPD 應回 200，且 cache 目錄持續出現新的 `seg_*.m4s`）。Live MPD 使用固定 `availabilityStartTime` + `SegmentTimeline`（對齊各段 `tfdt`），播放器需定期刷新 MPD（`minimumUpdatePeriod`）才能持續追 live edge。

亦可改用瀏覽器 + dash.js / Shaka Player 測試。

本機 Homebrew 的 `ffmpeg`/`ffprobe` 若未編入 `dash` demuxer，無法直接 `-i index.mpd`；可改測 `init.mp4` 與 `seg_N.m4s`（先 concat 再 ffprobe），或輪詢 MPD 的 `SegmentTimeline` / `startNumber` 是否持續前進。

## 自動化煙霧測試

需本機有 `ffmpeg`：

```bash
./script/smoke_test.sh
```

腳本會：啟動服務 → 產生短片並推流 → 檢查 `index.mpd` / segments → 停止服務。

## 更多

- 架構：[architecture.md](./architecture.md)
- 設定：[config.md](./config.md)
- 目錄規範：[layout.md](./layout.md)
