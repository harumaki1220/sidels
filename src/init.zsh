# sidels の中の zsh で、入力欄の中身とカーソル位置を sidels に知らせる。
# sidels は普段、画面の文字から打ちかけのコマンドを推測しているので、
# zsh-autosuggestions の灰色の候補や折り返しを読み違えることがある。これで正確になる。
# .zshrc に `eval "$(sidels init zsh)"` と書いて読み込む。sidels の外の zsh では何もしない。
if [[ -n $SIDELS ]]; then
  # 制御文字と `;` は空白にする（制御文字列が途中で切れないように）。
  # 1 文字を 1 文字に置き換えるので、カーソル位置はずれない
  _sidels_report() {
    print -rn -- $'\e]7777;sidels;'"$CURSOR;${${BUFFER//[[:cntrl:]]/ }//;/ }"$'\a'
  }
  # Enter や Ctrl+C で入力が終わったら、空になったことを知らせる
  _sidels_done() {
    print -rn -- $'\e]7777;sidels;0;\a'
  }
  # zsh-syntax-highlighting（0.7 まで）は、読み込んだ時点にある widget を全部包み込み、動くたびに色付けをやり直す。
  # 包まれると、知らせるたびに zsh-autosuggestions の灰色の候補の色が消えてしまうので、
  # .zshrc を全部読み終えた後（最初のプロンプトの前）に登録する
  _sidels_setup() {
    add-zsh-hook -d precmd _sidels_setup
    # 入力欄の部品がまだ読み込まれていないと、add-zle-hook-widget は何も言わずに失敗する
    zmodload zsh/zle
    autoload -Uz add-zle-hook-widget
    add-zle-hook-widget line-pre-redraw _sidels_report
    add-zle-hook-widget line-finish _sidels_done
  }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd _sidels_setup
fi
