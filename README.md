# psez

Windows PowerShell および SSH（ConPTY）環境で安全に動作する、モードレスの CUI テキストエディタです。

## 特徴

- **モードレス設計** — 起動直後からそのままテキスト入力が可能です（Vim のようなモード切替は不要）
- **日本語・全角文字対応** — `unicode-width` クレートにより East Asian Width を正確に計算し、全角文字のカーソル位置を正しく管理します
- **ConPTY クラッシュ回避** — Windows の SSH（ConPTY）環境で全角文字をバックスペース削除するとSSH接続がクラッシュする既知の問題を、フル・リドロー方式により完全に回避しています
- **システムクリップボード連携** — コピー・カットした内容を他のアプリと共有できます

## インストール

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
| `Ctrl+W` / `F2` | 名前を付けて保存 |
| `Ctrl+Q` | 終了（未保存時は確認） |
| `Ctrl+Z` | アンドゥ（最大1000回） |
| `Ctrl+C` | コピー開始 → もう一度押して確定 |
| `Ctrl+X` | カット開始 → もう一度押して確定 |
| `Ctrl+V` | ペースト（システムクリップボードから） |
| `Ctrl+A` | 行頭へ移動 |
| `Ctrl+E` | 行末へ移動 |
| `Ctrl+H` | ヘルプ表示 / 非表示 |
| `Tab` | タブ文字を挿入（4カラム幅で表示） |
| 矢印キー | カーソル移動 |
| `Home` / `End` | 行頭 / 行末 |
| `PageUp` / `PageDown` | ページスクロール |
| `Backspace` / `Delete` | 文字削除 |
| `Enter` | 改行 |
| `Esc` | 選択のキャンセル |

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

## ライセンス

[GNU General Public License v3.0](LICENSE)
