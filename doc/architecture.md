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

斷線重連時會 **重置 CMAF 世代**（清除舊 Segmenter / init / MPD，切片編號繼續），再等新 session 的 SPS/PPS+ASC。原因：origin 主機重啟後即使 codec 字串相同，`tfdt` 也會從 0 重開；沿用舊 Segmenter 會留下幽靈 `index.mpd`（seg 已被 janitor 刪光）。

## Codec 政策

固定接受 **H.264（AVC）+ AAC**。其他 codec 在 demux 階段拒絕並斷開連線。

媒體為 **passthrough remux**（不重編碼），依賴推流端以正確 codec 與合理 keyframe 間隔推送。

## 切片策略（Origin CMAF 契約）

與 `hls2dash`（`output_mode: dash`）共用同一契約；跨專案變更必須同步。完整說明見 p2p repo [`doc/segment_chunk_p2p_design.md`](../../p2p/doc/segment_chunk_p2p_design.md)。

- 目標切片長度由 `cache.segment_duration_secs` 設定（預設 **2 秒**，僅作切段目標／解析失敗時的 fallback）。
- 在 video keyframe 處切段，且緩衝時長達到目標後才切；**實際媒體時長可能因 GOP 與上述目標不一致**。
- **合格容差**：實際 duration ∈ **[1.5, 2.5]s**；超出記 metrics，連續超標可標 channel `degraded`（仍出流）。
- MPD 使用 `SegmentTimeline`，每個 `S@d` 取自該 `m4s` 第一個 `traf` 的實際 sample duration；`timeShiftBufferDepth` 為窗口時長總和。
- **禁止 phantom gap**：timeline 每個 `S` 必須有可取的 `seg_N.m4s`。
- 超出 `window_segments` 的舊 `.m4s` 會被刪除，timeline **同步**滑動（先縮 timeline 再刪檔，或即刪且同步縮 timeline）。
- Metrics 後綴：`segment_duration_seconds`、`segment_duration_out_of_tolerance_total`、`gop_estimated_seconds`。

## 世代不變量（MPD / init / segments）

任何時刻必須成立：

> `index.mpd` 只列出與磁碟上 `init.mp4` **同一世代**、且 `tfdt` 單調遞增的切片。

保證方式：

| 事件 | 行為 |
|------|------|
| **Publish 新推流** | `DashPackager::new` 清空 channel 目錄，從 seg 1 開始 |
| **Pull 斷線重連** | `prepare_for_reconnect()`：丢棄舊世代（Segmenter / codec config / segs / `index.mpd` / `init.mp4`），**切片編號繼續**；下一 session 等到新的 SPS/PPS+ASC 後再開新世代。另有 **media idle 45s**（TCP 仍活但無 A/V）強制斷線重連 |
| **程序重啟後 Pull resume** | 只續用切片編號；**刪除**舊 `seg_*.m4s` / `index.mpd` / `init.mp4`（新 Segmenter 的 tfdt 從 0 起，舊媒體不兼容） |
| **中途 SPS/PPS 或 AAC config 變更** | `rotate()`：丟棄舊世代緩衝、刪舊切片與 stale MPD、寫新 `init.mp4`、切片編號**繼續遞增**；第一個新切片落地後才重寫 MPD |
| **RTMP DTS 大跳躍（>5s）** | Demux 發 `TimelineDiscontinuity`（**不再**把 gap clamp 成一幀 duration）；packager `rotate()` 重建 A/V `tfdt` |
| **A/V `tfdt` 偏差 > `av_tfdt_max_skew_ms`** | 每個 fragment drain 檢查；publish/pull 定時掃最新 `seg_*.m4s`；超標立即 `rotate()` |
| **Packager 結束** | `finish()` 等待 writer 佇列排空，避免舊 MPD 在清空後落地 |
| **Janitor** | TTL 刪過期 `seg_*.m4s`；若目錄已無 segment 且 `index.mpd` 逾 TTL，刪除幽靈 MPD（與 orphan `init.mp4`） |

Metrics：`rtmp2dash_av_tfdt_skew_corrections_total`、`rtmp2dash_av_tfdt_last_abs_skew_milliseconds`。

## TLS

本期 DASH 為 **HTTP**。HTTPS 可於後續版本以設定檔提供憑證路徑擴充。
