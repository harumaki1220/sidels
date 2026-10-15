//! シェル側。
//!
//! PTY の出力を仮想端末（vt100）に流し込み、その画面から入力中の内容を読む。

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{MasterPty, PtySize};

use crate::AppEvent;

/// 左ペインの履歴として覚えておく行数。
pub const SCROLLBACK_LINES: usize = 10_000;

/// PTY と仮想端末の大きさを変える。
pub fn resize(master: &dyn MasterPty, parser: &Mutex<vt100::Parser>, size: PtySize) -> Result<()> {
    // PTYの大きさを変えると、シェルに画面サイズが変わったと通知される
    master.resize(size)?;
    parser
        .lock()
        .unwrap()
        .screen_mut()
        .set_size(size.rows, size.cols);
    Ok(())
}

/// 出力係のスレッドを立てる。
///
/// シェルの出力を仮想端末に流し込み、届くたびにメインループへ描き直しを頼む。
pub fn spawn_output_thread(
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

/// 画面から入力中の内容を読む。
///
/// 返すのは `(カーソル行全体, カーソル直前の打ちかけの単語)`。
/// 行全体にはプロンプトも混ざるが、glob を探すだけなら問題ない。
/// 単語はカーソルから左の空白までなので、プロンプトは混ざらない。
/// vim などが別画面を使っている間は、シェルの入力ではないので空にする。
pub fn read_input(parser: &vt100::Parser) -> (String, String) {
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
