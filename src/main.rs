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
    let mut panel_visible = true;
    let (left, _) = split(Rect::new(0, 0, size.width, size.height), panel_visible);
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
        let (left, right) = split(Rect::new(0, 0, size.width, size.height), panel_visible);
        let new_size = pty_size_for(left);
        if (new_size.rows, new_size.cols) != (pty_size.rows, pty_size.cols) {
            resize(master.as_ref(), &parser, new_size)?;
            pty_size = new_size;
        }

        // 隠している間はディレクトリを読みに行かない
        if panel_visible {
            panel.update();
        }

        terminal.draw(|frame| {
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

        if wait_for_event(&rx, &mut panel_visible)? {
            break;
        }
    }

    child.wait()?;
    Ok(())
}

/// 画面を左（シェル）7 : 右（パネル）3 に分ける。パネルを隠しているときはシェルが全体を使う
fn split(area: Rect, panel_visible: bool) -> (Rect, Rect) {
    if !panel_visible {
        return (area, Rect::default());
    }
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)]).areas(area);
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
fn wait_for_event(rx: &Receiver<AppEvent>, panel_visible: &mut bool) -> Result<bool> {
    let first = match rx.recv_timeout(Duration::from_millis(50)) {
        Ok(event) => Some(event),
        Err(RecvTimeoutError::Timeout) => None,
        Err(RecvTimeoutError::Disconnected) => return Ok(true),
    };
    // 出力が立て続けに届いたときは、まとめて1回だけ描き直す
    for event in first.into_iter().chain(rx.try_iter()) {
        match event {
            AppEvent::Output => {}
            AppEvent::TogglePanel => *panel_visible = !*panel_visible,
            AppEvent::Exited => return Ok(true),
        }
    }
    Ok(false)
}
