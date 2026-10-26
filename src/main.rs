use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::Hide;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use tui_term::widget::{Cursor, PseudoTerminal};

mod git;
mod input;
mod layout;
mod mouse;
mod panel;
mod selection;
mod shell;

use input::spawn_input_thread;
use layout::{PanelVisibility, pty_size_for, split};
use panel::FilePanel;
use selection::{AutoScroll, SharedSelection};
use shell::{Parser, Relay, SCROLLBACK_LINES, read_input, resize, spawn_output_thread};

/// ウィンドウの大きさが変わってから、シェル側の大きさを合わせるまで待つ時間。
///
/// ドラッグ中に 1 列変わるたびに PTY を変えると、そのたびにシェルがプロンプトを描き直してちらつく。
/// 大きさが変わらないままこの時間がたったら（ドラッグが止まったら）、まとめて 1 回だけ変える。
const RESIZE_SETTLE: Duration = Duration::from_millis(100);

/// 選択をドラッグしたまま端に来ているとき、スクロールする間隔。
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// 自動スクロールで 1 回に動かす行数の、最初と最大。
///
/// 端で止めている時間が長いほど速くする。少しだけ選びたいときは細かく合わせられ、
/// ずっと上まで行きたいときは待っていれば速くなる。
const AUTO_SCROLL_LINES: (usize, usize) = (2, 10);

/// 自動スクロールで 1 回に動かす行数を 1 増やすまでの時間。
const AUTO_SCROLL_SPEEDUP: Duration = Duration::from_millis(250);

/// 外側のターミナルのタイトルを保存する（XTWINOPS 22）。
///
/// 中のアプリが変えたタイトルを、終わるときに元に戻すため。対応していないターミナルでは無視される。
const SAVE_TITLE: &[u8] = b"\x1b[22;0t";

/// [`SAVE_TITLE`] で保存したタイトルに戻す（XTWINOPS 23）。
const RESTORE_TITLE: &[u8] = b"\x1b[23;0t";

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
        [] => {}
        ["init", "zsh"] => {
            print!("{}", shell::ZSH_INIT);
            return Ok(());
        }
        _ => {
            eprintln!(
                "使い方: sidels（起動）/ sidels init zsh（.zshrc で eval するシェル連携を出力）"
            );
            std::process::exit(2);
        }
    }

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
    let result = run(&mut terminal);
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

/// シェルを起動し、終了するまで画面を描き続ける。
fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    let size = terminal.size()?;
    let mut visibility = PanelVisibility::new(size.width);
    let (left, _) = split(
        Rect::new(0, 0, size.width, size.height),
        visibility.is_visible(),
    );
    let mut pty_size = pty_size_for(left);

    // 偽物の端末を左ペインの大きさで作り、中でシェルを起動する
    let pair = native_pty_system().openpty(pty_size)?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "zsh".to_string());
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(std::env::current_dir()?);
    // sidels の中で動いていることをシェルに知らせる。
    // .zshrc から sidels を自動起動するときに、中のシェルでまた起動して入れ子になるのを防ぐのに使える
    cmd.env("SIDELS", "1");
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let master = pair.master;

    let (tx, rx) = mpsc::channel();

    // パネルはディレクトリを別スレッドで読む。読み終わったら描き直しを頼む
    let panel_tx = tx.clone();
    let shell_pid = child.process_id();
    let mut panel = FilePanel::new(shell_pid, std::env::current_dir()?, move || {
        let _ = panel_tx.send(AppEvent::PanelLoaded);
    });

    // シェルの出力を解釈して今の画面をメモリ上に持つ仮想端末
    let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
        pty_size.rows,
        pty_size.cols,
        SCROLLBACK_LINES,
        Relay::new(tx.clone()),
    )));

    // シェル側の文字選択。入力係（マウス）が書き換え、描画で反転表示する
    let selection = SharedSelection::default();

    spawn_output_thread(master.try_clone_reader()?, Arc::clone(&parser), tx.clone());
    spawn_input_thread(
        master.take_writer()?,
        Arc::clone(&parser),
        Arc::clone(&selection),
        tx,
    );

    // 何も変わっていないときは描き直さない。描き直すたびに ratatui がカーソルを表示・移動し直すので、
    // 毎回描くとカーソルの点滅がリセットされ続けてチラつく
    let mut needs_draw = true;
    let mut last_layout = None;
    // 合わせたいシェル側の大きさと、その大きさになった時刻（RESIZE_SETTLE だけ待ってから合わせる）
    let mut pending_resize: Option<(PtySize, Instant)> = None;
    // 選択をドラッグしたまま端に来ているときの自動スクロールと、始めた時刻・最後にスクロールした時刻
    let mut auto_scroll: Option<AutoScroll> = None;
    let mut auto_scroll_since = None;
    let mut last_auto_scroll = Instant::now();
    // シェル連携から届いた入力中の内容。一度でも届いたら、画面から読む代わりにこれを使う
    let mut reported_input: Option<(String, String)> = None;
    loop {
        // ウィンドウの大きさが変わっていたら、PTYと仮想端末も合わせる
        let size = terminal.size()?;
        visibility.set_width(size.width);
        let panel_visible = visibility.is_visible();
        let layout = (size.width, size.height, panel_visible);
        // F12 での出し入れは、待たずにすぐ合わせる（キーを押したのに反応が遅れると、もたついて感じる）
        let toggled = last_layout.map(|(_, _, visible)| visible) != Some(panel_visible);
        if last_layout != Some(layout) {
            last_layout = Some(layout);
            needs_draw = true;
        }
        let (left, _) = split(Rect::new(0, 0, size.width, size.height), panel_visible);
        let new_size = pty_size_for(left);
        let same = |a: PtySize, b: PtySize| (a.rows, a.cols) == (b.rows, b.cols);
        if same(new_size, pty_size) {
            pending_resize = None;
        } else {
            let settled = match pending_resize {
                Some((size, since)) if same(size, new_size) => since.elapsed() >= RESIZE_SETTLE,
                _ => {
                    pending_resize = Some((new_size, Instant::now()));
                    false
                }
            };
            if settled || toggled {
                resize(master.as_ref(), &parser, new_size)?;
                pty_size = new_size;
                pending_resize = None;
                needs_draw = true;
                // 画面の大きさが変わると選択範囲の位置がずれるので外す
                *selection.lock().unwrap() = None;
            }
        }

        // 隠している間はディレクトリを読みに行かない。
        // 履歴をさかのぼっている間は画面に入力中の行が映っていないので、パネルは直前のまま
        let scrolled = parser.lock().unwrap().screen().scrollback() > 0;
        if panel_visible && !scrolled {
            // 今キー入力を受け取っているプログラム（前面のプロセスグループ）。普段はシェル
            let foreground = master
                .process_group_leader()
                .and_then(|pid| u32::try_from(pid).ok());
            // シェル以外（Claude Code など）が前面のときは、打っている文字はコマンドではないので反応しない
            let (line, word) = if foreground.is_none() || foreground == shell_pid {
                // 画面から読むと、zsh-autosuggestions の灰色の候補なども打った文字として読んでしまう
                reported_input
                    .clone()
                    .unwrap_or_else(|| read_input(&parser.lock().unwrap()))
            } else {
                (String::new(), String::new())
            };
            needs_draw |= panel.update(foreground, &line, &word);
        }

        // 端に来たまま列だけ動いたときは、速さを最初に戻さない
        auto_scroll_since = auto_scroll.and(auto_scroll_since.or_else(|| Some(Instant::now())));
        if let (Some(edge), Some(since)) = (auto_scroll, auto_scroll_since)
            && last_auto_scroll.elapsed() >= AUTO_SCROLL_INTERVAL
        {
            last_auto_scroll = Instant::now();
            let (first, max) = AUTO_SCROLL_LINES;
            let speedup = since.elapsed().as_millis() / AUTO_SCROLL_SPEEDUP.as_millis();
            let lines = (first + usize::try_from(speedup).unwrap_or(max)).min(max);
            needs_draw |= scroll_selection(&parser, &selection, edge, lines);
        }

        if needs_draw {
            needs_draw = false;
            draw(terminal, &parser, &selection, &panel, panel_visible)?;
        }

        match wait_for_event(
            &rx,
            &mut visibility,
            &mut panel,
            &mut auto_scroll,
            &mut reported_input,
        )? {
            Wake::Exited => break,
            Wake::Changed => needs_draw = true,
            Wake::Idle => {}
        }
    }

    child.wait()?;
    Ok(())
}

/// 自動スクロールで `lines` 行動かし、選択の終わりを端の行の `edge.col` まで広げる。
///
/// 動いたら `true`（履歴の一番上や今の画面に着いたら、もう動かない）。
fn scroll_selection(
    parser: &Mutex<Parser>,
    selection: &SharedSelection,
    edge: AutoScroll,
    lines: usize,
) -> bool {
    // 2 つのロックを同時に握らないよう、仮想端末を先に済ませる
    let head = {
        let mut parser = parser.lock().unwrap();
        let screen = parser.screen_mut();
        let before = screen.scrollback();
        let after = if edge.up {
            before + lines
        } else {
            before.saturating_sub(lines)
        };
        screen.set_scrollback(after);
        if screen.scrollback() == before {
            return false;
        }
        let row = if edge.up {
            0
        } else {
            screen.size().0.saturating_sub(1)
        };
        (selection::history_row(screen, row), edge.col)
    };
    if let Some(selected) = selection.lock().unwrap().as_mut() {
        selected.extend(head);
    }
    true
}

/// シェル側とパネルを描く。
fn draw(
    terminal: &mut DefaultTerminal,
    parser: &Mutex<Parser>,
    selection: &SharedSelection,
    panel: &FilePanel,
    panel_visible: bool,
) -> Result<()> {
    // 入力係とのデッドロックを避けるため、選択範囲は仮想端末より先に読み取っておく
    let selected = *selection.lock().unwrap();
    // 書き換えの途中でカーソルが動き回るのを見せないよう、描いている間は隠す（描き終わりに ratatui が戻す）
    execute!(io::stdout(), Hide)?;
    terminal.draw(|frame| {
        // draw は直前に画面を測り直すので、size() の後にウィンドウが縮んでいることがある。
        // はみ出して書くとパニックするので、範囲はここで改めて計算する
        let (left, right) = split(frame.area(), panel_visible);
        let parser = parser.lock().unwrap();
        let screen = parser.screen();

        // 左: シェルの画面。カーソルはtui-termに描かせず、本物のカーソルを動かす
        let shell_view = PseudoTerminal::new(screen).cursor(Cursor::default().visibility(false));
        frame.render_widget(shell_view, left);
        // 選択範囲は反転表示する（もともと反転している文字は元に戻して見せる）
        if let Some(selected) = selected {
            let buf = frame.buffer_mut();
            for row in 0..left.height {
                for col in 0..left.width {
                    if selected.contains(selection::history_row(screen, row), col) {
                        buf[(left.x + col, left.y + row)]
                            .modifier
                            .toggle(Modifier::REVERSED);
                    }
                }
            }
        }
        // 履歴をさかのぼっている間は、カーソルの位置に意味がないので隠す
        if !screen.hide_cursor() && screen.scrollback() == 0 {
            let (row, col) = screen.cursor_position();
            frame.set_cursor_position((left.x + col, left.y + row));
        }

        if panel_visible {
            panel.render(frame, right);
        }
    })?;
    Ok(())
}

/// [`wait_for_event`] の結果。
enum Wake {
    /// シェルが終了した。
    Exited,
    /// 描き直しが必要な知らせが届いた。
    Changed,
    /// 描き直しが必要な知らせは届かなかった。
    ///
    /// パネルの読み込み完了だけが届いたときも含む（描き直すかは `panel.update` が決める）。
    Idle,
}

/// 次の知らせを待つ。
///
/// 何も来なくても 50ms ごとに戻り、ウィンドウサイズの変化を拾えるようにしている。
fn wait_for_event(
    rx: &Receiver<AppEvent>,
    visibility: &mut PanelVisibility,
    panel: &mut FilePanel,
    auto_scroll: &mut Option<AutoScroll>,
    reported_input: &mut Option<(String, String)>,
) -> Result<Wake> {
    let first = match rx.recv_timeout(Duration::from_millis(50)) {
        Ok(event) => event,
        Err(RecvTimeoutError::Timeout) => return Ok(Wake::Idle),
        Err(RecvTimeoutError::Disconnected) => return Ok(Wake::Exited),
    };
    // 出力が立て続けに届いたときは、まとめて1回だけ描き直す
    let mut changed = false;
    for event in std::iter::once(first).chain(rx.try_iter()) {
        match event {
            AppEvent::Output => changed = true,
            AppEvent::TogglePanel => {
                visibility.toggle();
                changed = true;
            }
            // 読み込みが終わっただけでは描き直さない。中身が変わったかは panel.update が判断する
            AppEvent::PanelLoaded => {}
            AppEvent::ScrollPanel(lines) => changed |= panel.scroll(lines),
            AppEvent::AutoScroll(edge) => *auto_scroll = edge,
            // パネルに反映するかは panel.update が判断する
            AppEvent::ShellInput { line, word } => *reported_input = Some((line, word)),
            AppEvent::Copy(text) if selection::uses_pbcopy() => {
                selection::pbcopy(text.into_bytes())
            }
            AppEvent::Copy(text) => write_raw(&selection::osc52(&text))?,
            AppEvent::PassThrough(bytes) => write_raw(&bytes)?,
            AppEvent::Exited => return Ok(Wake::Exited),
        }
    }
    Ok(if changed { Wake::Changed } else { Wake::Idle })
}
