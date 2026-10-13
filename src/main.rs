use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout, Rect};
use tui_term::widget::{Cursor, PseudoTerminal};

mod panel;
use panel::FilePanel;

/// パネルを出し入れするキー（F12）。端末からはこのバイト列で届く
const TOGGLE_PANEL_KEY: &[u8] = b"\x1b[24~";

/// パネルの幅（左の区切り線を含む）
const PANEL_WIDTH: u16 = 28;

/// パネルを出してもシェルにこれだけの幅が残らないなら、パネルを自動で畳む
const MIN_SHELL_WIDTH: u16 = 80;

const MIN_WIDTH_FOR_PANEL: u16 = MIN_SHELL_WIDTH + PANEL_WIDTH;

/// 別スレッドからメインループに届く知らせ
enum AppEvent {
    /// シェルが何か出力した（画面を描き直す合図）
    Output,
    Exited,
    TogglePanel,
}

fn main() -> Result<()> {
    // rawモード + 別画面に切り替える。パニックしても自動で元に戻る
    let mut terminal = ratatui::init();
    // run の中で ? によるエラーが起きても、必ず restore まで来る
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

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
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let master = pair.master;

    let mut panel = FilePanel::new(child.process_id(), std::env::current_dir()?);

    // シェルの出力を解釈して今の画面をメモリ上に持つ仮想端末
    let parser = Arc::new(Mutex::new(vt100::Parser::new(
        pty_size.rows,
        pty_size.cols,
        0,
    )));

    let (tx, rx) = mpsc::channel();
    spawn_output_thread(master.try_clone_reader()?, Arc::clone(&parser), tx.clone());
    spawn_input_thread(master.take_writer()?, tx);

    loop {
        // ウィンドウの大きさが変わっていたら、PTYと仮想端末も合わせる
        let size = terminal.size()?;
        visibility.set_width(size.width);
        let panel_visible = visibility.is_visible();
        let (left, _) = split(Rect::new(0, 0, size.width, size.height), panel_visible);
        let new_size = pty_size_for(left);
        if (new_size.rows, new_size.cols) != (pty_size.rows, pty_size.cols) {
            resize(master.as_ref(), &parser, new_size)?;
            pty_size = new_size;
        }

        // 隠している間はディレクトリを読みに行かない
        if panel_visible {
            let (line, word) = read_input(&parser.lock().unwrap());
            panel.update(&line, &word);
        }

        terminal.draw(|frame| {
            // draw は直前に画面を測り直すので、size() の後にウィンドウが縮んでいることがある。
            // はみ出して書くとパニックするので、範囲はここで改めて計算する
            let (left, right) = split(frame.area(), panel_visible);
            let parser = parser.lock().unwrap();
            let screen = parser.screen();

            // 左: シェルの画面。カーソルはtui-termに描かせず、本物のカーソルを動かす
            let shell_view =
                PseudoTerminal::new(screen).cursor(Cursor::default().visibility(false));
            frame.render_widget(shell_view, left);
            if !screen.hide_cursor() {
                let (row, col) = screen.cursor_position();
                frame.set_cursor_position((left.x + col, left.y + row));
            }

            if panel_visible {
                panel.render(frame, right);
            }
        })?;

        if wait_for_event(&rx, &mut visibility)? {
            break;
        }
    }

    child.wait()?;
    Ok(())
}

/// 画面から入力中の内容を読む。返すのは (カーソル行全体, カーソル直前の打ちかけの単語)。
/// 行全体にはプロンプトも混ざるが、glob を探すだけなら問題ない。
/// 単語はカーソルから左の空白までなので、プロンプトは混ざらない。
/// vim などが別画面を使っている間は、シェルの入力ではないので空にする
fn read_input(parser: &vt100::Parser) -> (String, String) {
    let screen = parser.screen();
    if screen.alternate_screen() {
        return (String::new(), String::new());
    }
    let (row, col) = screen.cursor_position();
    let (_, cols) = screen.size();
    let line = screen
        .rows(0, cols)
        .nth(usize::from(row))
        .unwrap_or_default();

    // vt100 は行末の空白を詰めて返すので、"vim " の直後かどうかはカーソルの左のセルで判断する
    let after_space = col == 0
        || screen
            .cell(row, col - 1)
            .is_none_or(|cell| cell.contents().trim().is_empty() && !cell.is_wide_continuation());
    let word = if after_space {
        String::new()
    } else {
        let before_cursor = screen
            .rows(0, col)
            .nth(usize::from(row))
            .unwrap_or_default();
        let word = before_cursor
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default();
        word.to_string()
    };
    (line, word)
}

/// パネルを出すかどうか。普段は画面幅で自動で決め、F12 が押されたらそれに従う
struct PanelVisibility {
    /// F12 で決めた表示状態。None なら画面幅で自動判定
    forced: Option<bool>,
    wide: bool,
}

impl PanelVisibility {
    fn new(width: u16) -> Self {
        Self {
            forced: None,
            wide: width >= MIN_WIDTH_FOR_PANEL,
        }
    }

    /// 幅がしきい値をまたいだら、F12 の指定を忘れて自動判定に戻す
    fn set_width(&mut self, width: u16) {
        let wide = width >= MIN_WIDTH_FOR_PANEL;
        if wide != self.wide {
            self.wide = wide;
            self.forced = None;
        }
    }

    fn is_visible(&self) -> bool {
        self.forced.unwrap_or(self.wide)
    }

    fn toggle(&mut self) {
        self.forced = Some(!self.is_visible());
    }
}

/// パネルを右端に固定幅で置き、残りを全部シェルに渡す。パネルを隠しているときはシェルが全体を使う
fn split(area: Rect, panel_visible: bool) -> (Rect, Rect) {
    if !panel_visible {
        return (area, Rect::default());
    }
    // 狭い画面で F12 により無理やり出したときも、シェルが半分を下回らないようにする
    let panel_width = PANEL_WIDTH.min(area.width / 2);
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(panel_width)]).areas(area);
    (left, right)
}

fn pty_size_for(area: Rect) -> PtySize {
    PtySize {
        rows: area.height.max(1),
        cols: area.width.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn resize(master: &dyn MasterPty, parser: &Mutex<vt100::Parser>, size: PtySize) -> Result<()> {
    // PTYの大きさを変えると、シェルに画面サイズが変わったと通知される
    master.resize(size)?;
    parser
        .lock()
        .unwrap()
        .screen_mut()
        .set_size(size.rows, size.cols);
    Ok(())
}

/// シェルの出力 → 仮想端末。届くたびにメインループへ描き直しを頼む
fn spawn_output_thread(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    parser.lock().unwrap().process(&buf[..n]);
                    if tx.send(AppEvent::Output).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(AppEvent::Exited);
    });
}

/// キーボード → シェル。パネルの出し入れキーだけは抜き取ってメインループに知らせる
fn spawn_input_thread(mut writer: Box<dyn Write + Send>, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let (input, toggles) = take_toggle_keys(&buf[..n]);
                    for _ in 0..toggles {
                        if tx.send(AppEvent::TogglePanel).is_err() {
                            return;
                        }
                    }
                    if input.is_empty() {
                        continue;
                    }
                    if writer.write_all(&input).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 入力から出し入れキーを取り除き、残りのバイト列と押された回数を返す
fn take_toggle_keys(mut input: &[u8]) -> (Vec<u8>, usize) {
    let mut rest = Vec::with_capacity(input.len());
    let mut toggles = 0;
    while let Some(pos) = input
        .windows(TOGGLE_PANEL_KEY.len())
        .position(|w| w == TOGGLE_PANEL_KEY)
    {
        rest.extend_from_slice(&input[..pos]);
        toggles += 1;
        input = &input[pos + TOGGLE_PANEL_KEY.len()..];
    }
    rest.extend_from_slice(input);
    (rest, toggles)
}

/// 次の知らせを待つ。シェルが終了したら true を返す。
/// 何も来なくても 50ms ごとに戻り、ウィンドウサイズの変化を拾えるようにしている
fn wait_for_event(rx: &Receiver<AppEvent>, visibility: &mut PanelVisibility) -> Result<bool> {
    let first = match rx.recv_timeout(Duration::from_millis(50)) {
        Ok(event) => Some(event),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => return Ok(true),
    };
    // 出力が立て続けに届いたときは、まとめて1回だけ描き直す
    for event in first.into_iter().chain(rx.try_iter()) {
        match event {
            AppEvent::Output => {}
            AppEvent::TogglePanel => visibility.toggle(),
            AppEvent::Exited => return Ok(true),
        }
    }
    Ok(false)
}
