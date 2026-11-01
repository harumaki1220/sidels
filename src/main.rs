use std::io::{self, Write};

use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;

mod app;
mod git;
mod input;
mod layout;
mod mouse;
mod panel;
mod selection;
mod shell;

use layout::DEFAULT_PANEL_WIDTH;
use selection::AutoScroll;

/// 外側のターミナルのタイトルを保存する（XTWINOPS 22）。
///
/// 中のアプリが変えたタイトルを、終わるときに元に戻すため。対応していないターミナルでは無視される。
const SAVE_TITLE: &[u8] = b"\x1b[22;0t";

/// [`SAVE_TITLE`] で保存したタイトルに戻す（XTWINOPS 23）。
const RESTORE_TITLE: &[u8] = b"\x1b[23;0t";

/// `sidels --help` や、知らない引数のときに出す使い方。
const USAGE: &str = "\
使い方:
  sidels [--icons] [--width 列数]
  sidels init zsh

  --icons       ファイルの種類ごとのアイコンを付ける（Nerd Font が必要）
  --width 列数  起動したときのパネルの幅（既定は 28。区切り線のドラッグでも変えられる）
  init zsh      zsh のシェル連携を出力する（.zshrc に eval \"$(sidels init zsh)\" と書く）
";

/// 起動の引数。
struct Options {
    /// パネルにファイルの種類のアイコンを付ける（`--icons`）。
    icons: bool,
    /// 起動したときのパネルの幅（`--width`）。
    panel_width: u16,
}

/// 起動の引数を読む。知らない引数や、値の読めない `--width` なら `None`。
fn parse_options(args: &[String]) -> Option<Options> {
    let mut options = Options {
        icons: false,
        panel_width: DEFAULT_PANEL_WIDTH,
    };
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            // アイコンは Nerd Font がないと文字化けするが、フォントは外側のターミナルが決めていて調べられない。
            // なので、使いたい人だけ引数でオンにする
            "--icons" => options.icons = true,
            "--width" => options.panel_width = args.next()?.parse().ok()?,
            _ => options.panel_width = arg.strip_prefix("--width=")?.parse().ok()?,
        }
    }
    Some(options)
}

/// 別スレッドからメインループに届く知らせ。
enum AppEvent {
    /// シェルが何か出力した、またはスクロールした（描き直しが必要）。
    Output,
    /// シェルが終了した。
    Exited,
    /// F12 が押された。
    TogglePanel,
    /// パネルの読み込み係がディレクトリを読み終えた。
    PanelLoaded,
    /// 文字をクリップボードに入れてほしい。
    ///
    /// 画面への書き込みはメインループだけが行うので、入力係（選んだ文字）から頼む。
    /// 外側が OSC 52 を受け付けないときは、出力係（中のアプリがコピーした文字）からも頼む。
    Copy(String),
    /// 選択をドラッグしたまま上下の端に来た（`Some`）、または端から離れた・指を離した（`None`）。
    AutoScroll(Option<AutoScroll>),
    /// パネルとの区切り線がドラッグされた。今マウスがある列（1 始まり）。
    DragDivider(u16),
    /// パネルの上でホイールが回された。パネルの一覧をこの行数だけスクロールする（負なら上へ）。
    ScrollPanel(isize),
    /// シェル連携から、入力中の内容が届いた（`line` は入力欄の中身、`word` はカーソル直前の単語）。
    ShellInput { line: String, word: String },
    /// 中のアプリが出した制御文字列を、外側のターミナルにそのまま送ってほしい。
    ///
    /// クリップボードへの書き込み（OSC 52）、タイトル、今いる場所（OSC 7）。出力係から頼む。
    PassThrough(Vec<u8>),
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["init", "zsh"] => {
            print!("{}", shell::ZSH_INIT);
            return Ok(());
        }
        ["-h" | "--help"] => {
            print!("{USAGE}");
            return Ok(());
        }
        _ => {}
    }
    let Some(options) = parse_options(&args) else {
        eprint!("{USAGE}");
        std::process::exit(2);
    };

    // rawモード + 別画面に切り替える。パニックしても自動で元に戻る
    let mut terminal = ratatui::init();
    // 外側のターミナルに、貼り付けた文字を印で囲んで送ってもらう。
    execute!(io::stdout(), EnableBracketedPaste)?;
    // マウス操作も送ってもらう（ホイールでのスクロールなど）
    write_raw(mouse::ENABLE)?;
    write_raw(SAVE_TITLE)?;
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = write_raw(mouse::DISABLE);
        let _ = write_raw(RESTORE_TITLE);
        ratatui_hook(info);
    }));
    // run の中で ? によるエラーが起きても、必ず restore まで来る
    let result = app::run(&mut terminal, &options);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let _ = write_raw(mouse::DISABLE);
    let _ = write_raw(RESTORE_TITLE);
    ratatui::restore();
    result
}

/// 外側のターミナルに制御文字列をそのまま送る。
fn write_raw(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(bytes)?;
    stdout.flush()
}
