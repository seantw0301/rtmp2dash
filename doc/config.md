# 設定說明

執行檔預設讀取工作目錄下的 `config.yaml`，也可用 `--config` 或環境變數指定：

```bash
./target/release/rtmp2dash --config /path/to/config.yaml
CONFIG=/path/to/config.yaml ./script/start.sh
```

## 完整範例

```yaml
rtmp:
  listen: "0.0.0.0"
  port: 6136
  app: "live"          # 本機推流 app 名稱

dash:
  listen: "0.0.0.0"
  port: 8080           # HTTP DASH 埠

cache:
  dir: "./cache"
  segment_duration_secs: 2
  window_segments: 90
  # 逾時刪除 seg_*.m4s（依 mtime）。init.mp4 與 index.mpd 永不由 janitor 清理。
  # 未設定時預設為 max(30, window_segments * segment_duration_secs * 2)
  # ttl_secs: 60
  cleanup_interval_secs: 10

# 拉流（可與本機推流並行；可多筆）
pull:
  - url: "rtmp://origin.example.com:1935/live/stream1"
    channel: "demo"        # 輸出路徑 /live/demo/index.mpd
    reconnect_secs: 3       # 斷線重連間隔（秒）
```

## 欄位一覽

| 欄位 | 必填 | 預設 | 說明 |
|------|------|------|------|
| `rtmp.listen` | 是 | — | 本機 RTMP 推流 bind 位址 |
| `rtmp.port` | 是 | — | 本機 RTMP 推流埠 |
| `rtmp.app` | 否 | `live` | 推流 app 名稱 |
| `dash.listen` | 是 | — | HTTP bind 位址 |
| `dash.port` | 是 | — | DASH HTTP 埠 |
| `cache.dir` | 是 | — | 輸出根目錄 |
| `cache.segment_duration_secs` | 否 | `2` | 切片目標秒數（必須 > 0） |
| `cache.window_segments` | 否 | `90` | 視窗內保留的 segment 數（約 3 分鐘＠2s）；磁碟多留 2 片 grace |
| `cache.ttl_secs` | 否 | 自動 | 背景清理過期檔案的秒數（不得低於 live 視窗）；**不含 `init.mp4`** |
| `cache.cleanup_interval_secs` | 否 | `10` | janitor 掃描間隔 |
| `pull` | 否 | `[]` | 遠端 RTMP 拉流列表 |
| `pull[].url` | 是* | — | 遠端 RTMP URL（`rtmp://host:port/app/stream`） |
| `pull[].channel` | 是* | — | 輸出 channel 名稱（DASH 路徑） |
| `pull[].reconnect_secs` | 否 | `3` | 拉流斷線後重連秒數 |

\* 僅在啟用該筆 `pull` 項目時必填。

## URL 對應

### 本機推流（publish）

| 方向 | URL |
|------|-----|
| 推流 | `rtmp://<host>:6136/live/<channel>` |
| 播放 | `http://<host>:8080/live/<channel>/index.mpd` |

### 遠端拉流（pull）範例

| 方向 | URL |
|------|-----|
| 來源 | `rtmp://origin.example.com:1935/live/stream1` |
| 播放 | `http://127.0.0.1:8080/live/demo/index.mpd` |

播放清單檔名為標準 **`index.mpd`**（非 `.mdp`）。

輸出檔案：`{cache.dir}/live/<channel>/init.mp4`、`seg_N.m4s`、`index.mpd`。

Cache 清理：

- 推流中依 `window_segments` 滑動刪除舊 `seg_*.m4s`
- 背景 janitor 依 `ttl_secs` 定時刪除過期的 `seg_*.m4s` / `*.tmp`
- **`init.mp4` 與 `index.mpd` 永不清理**（live 播放必需）

推流與拉流可同時運作；同一 `channel` 名稱不可重複占用。
