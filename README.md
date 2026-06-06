# psez

Windows PowerShell、コマンドプロンプト、および SSH（ConPTY）環境で安全に動作する、モードレスの CUI テキストエディタです。

## 特徴

- **モードレス設計** — 起動直後からそのままテキスト入力が可能です（Vim のようなモード切替は不要）
- **日本語・全角文字対応** — `unicode-width` クレートにより East Asian Width を正確に計算し、全角文字のカーソル位置を正しく管理します
- **ConPTY クラッシュ回避** — Windows の SSH（ConPTY）環境で全角文字をバックスペース削除するとSSH接続がクラッシュする既知の問題を、フル・リドロー方式により完全に回避しています
- **システムクリップボード連携** — コピー・カットした内容を他のアプリと共有できます
- **文字コード・改行コード対応** — UTF-8 / UTF-8 BOM / Shift-JIS / EUC-JP、LF / CRLF を自動判別して読み書きします
- **長い行の表示** — 画面幅を超える行は横スクロール、または折り返し表示（Ctrl+F で切り替え）に対応します
- **ファイルサイズ上限** — 500 MB を超えるファイルは開けません

## インストール

### 方法1：バイナリを直接ダウンロード（推奨）

[GitHub Releases](https://github.com/gtms-code/psez/releases/latest) から `psez.exe` をダウンロードして、パスの通ったフォルダに置くだけで使えます。

```sh
# ダウンロードした psez.exe をパスの通ったフォルダにコピー
copy %USERPROFILE%\Downloads\psez.exe C:\Windows\system32\
```

### 方法2：ソースからビルド

Rust（cargo）が必要です。

```sh
git clone https://github.com/gtms-code/psez.git
cd psez
cargo build --release
```

ビルドされたバイナリは `target/release/psez.exe` に生成されます。

生成された `psez.exe` をパスの通ったフォルダ（`C:\Windows\system32` 等）にコピーしてください。

```sh
copy target\release\psez.exe C:\Windows\system32\
```

## 使い方

PowerShell、コマンドプロンプト、どちらからでも起動できます。

```sh
# 新規ファイルを作成する
psez newfile.txt

# 既存ファイルを開く
psez existingfile.txt

# ファイル名を指定しない（後から Save As で保存）
psez
```

## キーバインド

| キー | 動作 |
|------|------|
| `Ctrl+S` | 上書き保存 |
| `Ctrl+W` | 名前を付けて保存 |
| `Ctrl+Q` | 終了（未保存時は確認） |
| `Ctrl+Z` | アンドゥ（最大1000回） |
| `Ctrl+C` | コピー開始 → もう一度押して確定 |
| `Ctrl+X` | カット開始 → もう一度押して確定 |
| `Ctrl+V` | ペースト（システムクリップボードから） |
| `Ctrl+E` | 文字コード・改行コードの変更 |
| `Ctrl+F` | 折り返し表示のON / OFF |
| `Ctrl+H` | ヘルプ表示 / 非表示 |
| `Tab` | タブ文字を挿入（4カラム幅で表示） |
| 矢印キー | カーソル移動 |
| `Home` / `End` | 行頭 / 行末 |
| `PageUp` / `PageDown` | ページスクロール |
| `Backspace` / `Delete` | 文字削除 |
| `Enter` | 改行 |
| `Esc` | 選択のキャンセル |

### 文字コード・改行コードの変更（Ctrl+E）

`Ctrl+E` を押すと2ステップで変更できます。

1. 文字コードを選択：`1` UTF-8 / `2` UTF-8 BOM / `3` Shift-JIS / `4` EUC-JP
2. 改行コードを選択：`L` LF / `C` CRLF

確定後、`Ctrl+S` で保存すると選択した文字コード・改行コードで書き出されます。`Esc` でいつでもキャンセルできます。

### 折り返し表示（Ctrl+F）

`Ctrl+F` を押すたびに折り返し表示のON / OFFを切り替えます。

- **OFF（既定）** — 画面幅を超える行は横スクロールで表示します
- **ON** — 画面幅を超える行を折り返して表示します。全角文字は必ず行頭から表示され、途中で分断されません

## 技術的な背景

Windows PowerShell においては、一般的なエディタの nano や micro では、全角文字をバックスペースで削除した際に、エディタ側とターミナル側でカーソル位置の認識がずれ、不正なエスケープシーケンスが発生して、クラッシュするなどの深刻な問題がありました。

psez はこの問題を **フル・リドロー方式** で解決しています。

- 文字の入力・削除のたびに `\b`（バックスペース文字）や部分的な消去シーケンスを**一切使用しない**
- 変更のたびに行全体を `ClearType::CurrentLine` で消去し、先頭から再描画する
- カーソルは常に `cursor::MoveTo` による絶対座標で配置する

## 依存クレート

| クレート | 用途 |
|----------|------|
| [`crossterm`](https://crates.io/crates/crossterm) | ターミナル制御・キーイベント取得 |
| [`unicode-width`](https://crates.io/crates/unicode-width) | East Asian Width の正確な計算 |
| [`arboard`](https://crates.io/crates/arboard) | システムクリップボード連携 |
| [`encoding_rs`](https://crates.io/crates/encoding_rs) | Shift-JIS / EUC-JP のエンコード・デコード |
| [`chardetng`](https://crates.io/crates/chardetng) | 文字コードの自動判別 |
| [`tempfile`](https://crates.io/crates/tempfile) | アトミックなファイル保存 |

## 免責事項

使用は自己責任でお願いします。ファイルの破損・データの損失・その他いかなる損害が生じた場合も、作者は一切の責任を負いません。大切なファイルを編集する前には必ずバックアップを取ることを推奨します。

## ライセンス

[GNU General Public License v3.0](LICENSE)
