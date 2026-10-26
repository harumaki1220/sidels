# README の GIF を作る VHS の台本。リポジトリの一番上で `vhs docs/demo.tape` と打つ。
# sidels がパスの通った場所に入っていて、JetBrainsMono Nerd Font が使えること。

Output docs/demo.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font"
Set FontSize 15
Set Width 1240
Set Height 380
Set Padding 12
Set Theme "OneHalfDark"
Set TypingSpeed 70ms

Hide
Type "source docs/demo-setup.sh && clear"
Enter
Sleep 500ms
Show

# 起動すると、右に一覧。Git の印と、見出しにブランチ。Git が無視しているもの（target/）は薄く
Type "sidels --icons"
Sleep 500ms
Enter
Sleep 3s

# 新しいファイルには ? と ●（最近書き換えた）が付く
Type "echo note > notes.txt"
Sleep 500ms
Enter
Sleep 2.5s

# glob で消えるファイルを光らせる（実行はしない）
Type "rm *.log"
Sleep 2.5s
Ctrl+C
Sleep 500ms

# 打ちかけの名前で絞り込み、パスまで打つと中身を先に見せ、移動するとパネルも付いてくる
Type "cd sr"
Sleep 1.5s
Type "c/"
Sleep 2s
Enter
Sleep 3s
