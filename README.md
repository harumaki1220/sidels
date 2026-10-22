# sidels

シェルの横に今いるディレクトリのファイル一覧を出しておく小さなツールです。

打っている途中のコマンドにも少し反応します（`cd src/` で移動先を先に見せる、`rm *.log` で消えるファイルを光らせる、など）。
Git の管理下では、変更のあるファイルに `M` や `?` の印が付き（消したファイルも取り消し線で出ます）、見出しに今のブランチ（push していないコミットがあれば `main ↑2` のように）が出ます。
5 分以内に書き換えられたファイルには `●` が付くので、Claude Code などが触ったファイルもすぐ分かります。
Git が無視しているもの（`node_modules/` など）は薄く表示します（Git の外では、`.` で始まるものを薄くします）。

中で動くのはいつものシェル（`$SHELL`）なので、設定やキー操作はそのまま使えます。

> Linux（WSL 含む）専用です。x86_64 と ARM（aarch64）に対応しています。

## インストール

ビルド済みのファイルをダウンロードして、`~/.local/bin` などパスの通った場所に置きます（`uname -m` で PC の種類に合ったファイルを選びます）。

```sh
curl -L https://github.com/harumaki1220/sidels/releases/latest/download/sidels-$(uname -m)-linux.tar.gz | tar xz
mv sidels ~/.local/bin/
```

Rust（1.88 以上）があれば、ソースからビルドすることもできます（C のリンカーも必要です。Ubuntu なら `sudo apt install build-essential`）。

```sh
cargo install --locked --git https://github.com/harumaki1220/sidels
```

`sidels` で起動し、`exit` でシェルを終えると一緒に終了します。

## 操作

| 操作 | 動き |
|---|---|
| F12 | パネルの出し入れ（狭いウィンドウでは自動で隠れる） |
| ホイール / Shift+↑↓ | 過去の出力をスクロール |
| ドラッグ → Ctrl+C | シェル側だけを選択してコピー（選択していないときの Ctrl+C は、いつもどおり中断） |

## 気をつけること

- マウス操作を受け取るので、Windows Terminal の右クリック貼り付けは効きません（Ctrl+V は使えます）
- 入力中の内容は画面から読み取っているので、zsh-autosuggestions の灰色の候補にも反応することがあります

## ライセンス

[MIT](LICENSE)
