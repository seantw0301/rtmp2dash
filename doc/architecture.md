# 架構說明

`rtmp2dash` 是一個純 Rust 的直播轉碼封裝服務：以 **RTMP 推流（publish）** 與／或 **RTMP 拉流（pull）** 取得 H.264+AAC 直播，remux 成 live MPEG-DASH（不重編碼）。兩者可並行。

## 資料流

```text
OBS / ffmpeg (push)              遠端 RTMP (pull, config.yaml)
    │  rtmp://local/live/<ch>         │  rtmp://remote/... 
    ▼                                 ▼
┌─────────────────┐            ┌──────────────┐
│ RTMP Publish    │            │ RTMP Pull    │
│ Server          │            │ Client(s)    │
└────────┬────────┘            └──────┬───────┘
         │                            │
         └────────────┬───────────────┘
                      ▼
              ┌──────────────┐
              │ FLV Demux    │
              │ H264 + AAC   │
              └──────┬───────┘
                     ▼
              ┌──────────────┐
              │ CMAF Packager│
              └──────┬───────┘
                     ▼
            cache/live/<channel>/
              init.mp4 / seg_N.m4s / index.mpd
                     ▼
              HTTP DASH egress
```

## 多路串流（multi-streaming）

- 不同 `channel_id` 可**同時**推流或拉流。
- 同一 `channel_id` 同一時間只允許一個來源（推流 publisher 或拉流 worker）。
- 每個 channel 獨立寫入自己的 cache 子目錄。

## 拉流（pull）

在 `config.yaml` 的 `pull` 列表設定來源 URL 與輸出 `channel`；斷線後依 `reconnect_secs` 自動重連。詳見 [config.md](./config.md)。

## Codec 政策

固定接受 **H.264（AVC）+ AAC**。其他 codec 在 demux 階段拒絕並斷開連線。

媒體為 **passthrough remux**（不重編碼），依賴推流端以正確 codec 與合理 keyframe 間隔推送。

## 切片策略

- 目標切片長度由 `cache.segment_duration_secs` 設定（預設 **2 秒**）。
- 在 video keyframe 處切段，且緩衝時長達到目標後才切。
- 超出 `window_segments` 的舊 `.m4s` 會被刪除，並更新 live MPD 的 `startNumber`。

## TLS

本期 DASH 為 **HTTP**。HTTPS 可於後續版本以設定檔提供憑證路徑擴充。
