# sidels

シェルの横に今いるディレクトリのファイル一覧と Git の状態を出し続けるツールです。

![sidels のデモ](docs/demo.gif)

- 打っている途中のコマンドに反応します。`cd src/` なら移動先の中身を先に見せ、`rm *.log` なら消えるファイルを光らせます
- 変更のあるファイルに Git の印（`M` や `?`）が付きます。見出しには今のブランチと、push していないコミットの数（`main ↑2`）が出ます
- 書き換えたばかりのファイルに `●` が付きます。Claude Code などを動かしている間は、起動してから変わったファイルに付き続けるので、何を触ったかが分かります
- Git が無視しているもの（`node_modules/` など）は薄く表示します
- Claude Code が worktree に移ると、パネルも付いていきます

中で動くのはいつものシェル（`$SHELL`）なので、設定やキー操作はほぼそのまま使えます。例外は「普通のターミナルとの違い」にまとめています。

> Linux（WSL 含む）と macOS で動きます。x86_64 と ARM（Apple Silicon を含む）に対応しています。
> macOS はまだあまり試せていません。

## インストール

ビルド済みのファイルをダウンロードして `~/.local/bin` に置きます。

```sh
mkdir -p ~/.local/bin
curl -L https://github.com/harumaki1220/sidels/releases/latest/download/sidels-$(uname -m)-$(uname -s | tr A-Z a-z).tar.gz | tar xz -C ~/.local/bin
```

`sidels` と打って `command not found` になるときは `~/.local/bin` がパスに入っていません。
次の 1 行を足して、ターミナルを開き直してください（bash なら `~/.zshrc` を `~/.bashrc` に）。

```sh
printf '\nexport PATH="$HOME/.local/bin:$PATH"\n' >> ~/.zshrc
```

Rust（1.88 以上）があれば、ソースからビルドすることもできます。C のリンカーも要ります（Ubuntu なら `sudo apt install build-essential`）。

```sh
cargo install --locked --git https://github.com/harumaki1220/sidels
```

`sidels` で起動し、`exit` でシェルを終えると一緒に終了します。
Nerd Font を使っているなら、`sidels --icons` でファイルの種類ごとのアイコンが付きます。

### zsh との連携（任意）

zsh なら `.zshrc` に次の 1 行を足すと打ちかけのコマンドを zsh から直接受け取ります。
画面から読み取るより正確になり、zsh-autosuggestions の灰色の候補にパネルが反応しなくなります。
sidels の外の zsh では何もしないので、書いたままで大丈夫です（zsh 5.3 以上）。

```zsh
eval "$(sidels init zsh)"
```

## 操作

| 操作 | 動き |
|---|---|
| F12（キーボードによっては fn+F12） | パネルの出し入れ（狭いウィンドウでは自動で隠れる） |
| ホイール / Shift+↑↓ / Shift+PgUp・PgDn | 過去の出力をスクロール（パネルの上ではパネルの一覧をスクロール） |
| ドラッグ → Ctrl+C | シェル側の文字を選んでコピー（端までドラッグすると画面の外まで選べる。選んでいないときの Ctrl+C はいつもどおり中断） |
| ダブルクリック / トリプルクリック | 単語 / 行を選ぶ（パスは丸ごと選べる） |
| 区切り線をドラッグ | パネルの幅を変える（閉じると戻る。起動したときの幅は `sidels --width 36` で決められる） |

## 普通のターミナルとの違い

sidels は、ターミナルの中にもう 1 つ小さなターミナルを作ってシェルを動かしています。
シェルの画面は sidels が描き直しているので、普段ターミナルがやっていることの一部を sidels が代わりにやっています。
そのため、普通のターミナルと少し違うところがあります。

- スクロールと文字の選択は sidels が行います。ターミナルの設定（スクロールの速さ、選択の区切り方など）は効きません
- マウス操作を受け取るので、Windows Terminal の右クリック貼り付けは効きません（Ctrl+V は使えます）
- F12、Shift+↑↓、Shift+PgUp・PgDn は sidels が使います（vim などが画面全体を使っている間は、スクロールのキーはそのアプリに渡します）
- vim などがマウスを使っている間は、ドラッグやクリックはそのアプリに渡します
- コピーは OSC 52 に対応したターミナル（Windows Terminal、Ghostty、iTerm2 など）で使えます。iTerm2 では、設定の「Applications in terminal may access clipboard」がオフだとコピーできません。macOS 標準の Terminal.app では、代わりに `pbcopy` を使います
- zsh との連携を設定していないときは、zsh-autosuggestions の灰色の候補にも反応することがあります
- 中のシェルには `SIDELS=1` が渡ります。`.zshrc` から sidels を自動で起動するときに、入れ子を防ぐのに使えます（`[[ -z $SIDELS ]] && exec sidels`）

## ライセンス

[MIT](LICENSE)
