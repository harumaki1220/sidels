# README の GIF（demo.tape）を撮るための、見せる用のプロジェクトとシェルを用意する。
# 本物のリポジトリやシェルの設定が映らないように、毎回 /tmp に作り直す。
demo=/tmp/sidels-demo
rm -rf "$demo"
mkdir -p "$demo/zdotdir" "$demo/my-app/src" "$demo/my-app/target/debug"
cd "$demo/my-app" || return

printf 'fn main() {\n    println!("hello");\n}\n' > src/main.rs
printf 'pub mod config;\n' > src/lib.rs
printf 'pub struct Config;\n' > src/config.rs
printf '[package]\nname = "my-app"\nversion = "0.1.0"\n' > Cargo.toml
printf '# my-app\n' > README.md
printf 'target/\n' > .gitignore
touch Cargo.lock target/debug/my-app app.log debug.log error.log

git init -q -b main
git add .
git -c user.name=demo -c user.email=demo@example.com commit -q -m init
# 変更のあるファイルと、Git がまだ知らないファイルを作っておく
printf '# my-app\n\nA small demo.\n' > README.md
printf -- '- write tests\n' > TODO.md
# ● は 5 分以内に書き換えたものに付くので、用意したファイルは古くしておく
find . -path ./.git -prune -o -exec touch -d '1 hour ago' {} +

# 中で動かすシェル: 短いプロンプトと、sidels のシェル連携だけの zsh
cat > "$demo/zdotdir/.zshrc" <<'ZSHRC'
# 環境変数の VISUAL などが vim だと vi 風のキー操作になり、台本の Ctrl+U が行を消さなくなる
bindkey -e
PROMPT='%F{blue}%~%f %F{green}❯%f '
eval "$(sidels init zsh)"
ZSHRC
export SHELL=/usr/bin/zsh ZDOTDIR="$demo/zdotdir" HOME="$demo"
