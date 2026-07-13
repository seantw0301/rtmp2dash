# 專案結構與硬性規範

本文件定義 `rtmp2dash` 的目錄約定與開發限制。

## 目錄約定

| 路徑 | 用途 |
|------|------|
| `src/` | Rust 原始碼 |
| `doc/` | **所有文件**（設計、用法、設定、規範） |
| `script/` | **啟動 / 停止 / 測試** 等腳本 |
| `config.yaml` | 執行設定（範例） |
| `cache/` | 執行期 DASH 輸出（可忽略進版控） |
| `LICENSE` | MIT 授權全文 |
| `README.md` | 專案入口說明（簡要；細節見 `doc/`） |

## 硬性規則

1. **文件**：說明文件一律放在 [`doc/`](./) 之下，勿在其他目錄散落 `.md` 文件（根目錄僅保留 `README.md`）。
2. **腳本與測試**：啟動、停止、煙霧測試等腳本一律放在 [`../script/`](../script/) 之下。
3. **單檔行數上限**：每一支程式原始碼檔案（`.rs`）**不可超過 1000 行**；超過時必須拆成 sub-modules（例如 `mod foo;` + `foo/*.rs`）。
4. **授權**：本專案以 **MIT License** 開源，見根目錄 [`../LICENSE`](../LICENSE)。

## 原始碼模組對照

| 模組 | 職責 |
|------|------|
| `config` | 讀取 / 驗證 `config.yaml` |
| `channel` | 多路 channel 的 publish 鎖 |
| `rtmp` | RTMP listen、handshake、publish session |
| `demux` | FLV/AVC+AAC → access units |
| `dash` | CMAF segment + live `index.mpd` |
| `http` | HTTP 提供 MPD / segments |

目前各 `.rs` 檔皆遠低於 1000 行；日後擴充請維持此上限。
