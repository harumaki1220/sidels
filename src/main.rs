use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::DefaultTerminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Borders};
use tui_term::widget::{Cursor, PseudoTerminal};

/// 別スレッドからメインループに届く知らせ
enum AppEvent {
    /// シェルが何か出力した（画面を描き直す合図）
    Output,
    /// シェルが終了した
    Exited,
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
    // 最初の画面の大きさから、左ペインの大きさを決める
    let size = terminal.size()?;
    let (left, _) = split(Rect::new(0, 0, size.width, size.height));
    let mut pty_size = pty_size_for(left);

    // 偽物の端末を左ペインの大きさで作り、中でシェルを起動する
    let pair = native_pty_system().openpty(pty_size)?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "zsh".to_string());
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(std::env::current_dir()?);
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);
    let master = pair.master;

    // シェルの出力を解釈して今の画面をメモリ上に持つ仮想端末
    let parser = Arc::new(Mutex::new(vt100::Parser::new(
        pty_size.rows,
        pty_size.cols,
        0,
    )));

    let (tx, rx) = mpsc::channel();
    spawn_output_thread(master.try_clone_reader()?, Arc::clone(&parser), tx);
    spawn_input_thread(master.take_writer()?);

    loop {
        // ウィンドウの大きさが変わっていたら、PTYと仮想端末も合わせる
        let size = terminal.size()?;
        let (left, right) = split(Rect::new(0, 0, size.width, size.height));
        let new_size = pty_size_for(left);
        if (new_size.rows, new_size.cols) != (pty_size.rows, pty_size.cols) {
            resize(master.as_ref(), &parser, new_size)?;
            pty_size = new_size;
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

            // 右: ファイル一覧パネル（中身はまだ空）
            let panel = Block::default().borders(Borders::LEFT).title(" files ");
            frame.render_widget(panel, right);
        })?;

        if wait_for_event(&rx)? {
            break;
        }
    }

    child.wait()?;
    Ok(())
}

/// 画面を左（シェル）7 : 右（パネル）3 に分ける
fn split(area: Rect) -> (Rect, Rect) {
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

/// キーボード → シェル
fn spawn_input_thread(mut writer: Box<dyn Write + Send>) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 次の知らせを待つ。シェルが終了したら true を返す。
/// 何も来なくても 50ms ごとに戻り、ウィンドウサイズの変化を拾えるようにしている
fn wait_for_event(rx: &Receiver<AppEvent>) -> Result<bool> {
    match rx.recv_timeout(Duration::from_millis(50)) {
        Ok(AppEvent::Exited) | Err(RecvTimeoutError::Disconnected) => return Ok(true),
        Ok(AppEvent::Output) | Err(RecvTimeoutError::Timeout) => {}
    }
    // 出力が立て続けに届いたときは、まとめて1回だけ描き直す
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::Exited = event {
            return Ok(true);
        }
    }
    Ok(false)
}
