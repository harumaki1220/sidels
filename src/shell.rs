//! シェル側。
//!
//! PTY の出力を仮想端末（vt100）に流し込み、その画面から入力中の内容を読む。
//! 中のアプリが出したクリップボードへの書き込み（OSC 52）、タイトル、今いる場所（OSC 7）は、外側のターミナルに伝える。

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{MasterPty, PtySize};

use crate::AppEvent;

/// 左ペインの履歴として覚えておく行数。
pub const SCROLLBACK_LINES: usize = 10_000;

/// シェルの出力を解釈して、今の画面をメモリ上に持つ仮想端末。
pub type Parser = vt100::Parser<Relay>;

/// 仮想端末が受け取った制御文字列のうち、外側のターミナルにも伝えるものを拾う。
///
/// vt100 は画面を作るのに使わないものを捨てるので、ここで拾ってメインループに書き込みを頼む。
pub struct Relay {
    tx: Sender<AppEvent>,
}

impl Relay {
    pub fn new(tx: Sender<AppEvent>) -> Self {
        Self { tx }
    }

    fn pass_through(&self, sequence: Vec<u8>) {
        // 送れないのはメインループが終わるところなので、捨ててよい
        let _ = self.tx.send(AppEvent::PassThrough(sequence));
    }
}

impl vt100::Callbacks for Relay {
    // 伝えないと、Claude Code や vim が選択した文字をコピーしてもクリップボードに入らない。
    // 中身を読ませる問い合わせ（paste_from_clipboard）は、中のアプリにクリップボードを覗かれるので伝えない
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        self.pass_through([b"\x1b]52;", ty, b";", data, b"\x07"].concat());
    }

    // OSC 0（アイコン名とタイトル）でも呼ばれる。アイコン名は今のターミナルでは使われないので、タイトルだけ伝える
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.pass_through([b"\x1b]2;", title, b"\x07"].concat());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if is_relayed_osc(params) {
            self.pass_through([b"\x1b]", params.join(&b';').as_slice(), b"\x07"].concat());
        }
    }
}

/// vt100 が扱わない OSC のうち、外側のターミナルに伝えるものか。
///
/// - OSC 7、OSC 9;9: シェルの今いる場所。ターミナルが「同じフォルダで新しいタブを開く」に使う
///   （OSC 9;9 は Windows Terminal の形）
/// - OSC 0 / 2: タイトルに `;` が入っていると vt100 が区切ってしまい、ここに来る
fn is_relayed_osc(params: &[&[u8]]) -> bool {
    matches!(params, [b"0" | b"2" | b"7", ..] | [b"9", b"9", ..])
}

/// PTY と仮想端末の大きさを変える。
pub fn resize(master: &dyn MasterPty, parser: &Mutex<Parser>, size: PtySize) -> Result<()> {
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
    parser: Arc<Mutex<Parser>>,
    tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // 外側に伝える制御文字列は、ここで解釈する間に Relay がメインループに送る
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
pub fn read_input(parser: &Parser) -> (String, String) {
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn passed_through(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        let mut parser = Parser::new_with_callbacks(24, 80, 0, Relay::new(tx));
        for chunk in chunks {
            parser.process(chunk);
        }
        drop(parser);
        rx.iter()
            .filter_map(|event| match event {
                AppEvent::PassThrough(sequence) => Some(sequence),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn relays_clipboard_write_split_across_reads() {
        let sequences = passed_through(&[b"abc\x1b]5", b"2;c;aGVs", b"bG8=\x1b\\def"]);
        assert_eq!(sequences, vec![b"\x1b]52;c;aGVsbG8=\x07".to_vec()]);
    }

    #[test]
    fn relays_title_and_cwd() {
        let sequences = passed_through(&[
            b"\x1b]0;vim main.rs\x07",
            b"\x1b]2;a;b\x1b\\",
            b"\x1b]7;file://host/home\x07",
            b"\x1b]9;9;/home\x07",
            b"\x1b]1;icon\x07\x1b]9;notify\x07",
        ]);
        assert_eq!(
            sequences,
            [
                b"\x1b]2;vim main.rs\x07".as_slice(),
                b"\x1b]2;a;b\x07",
                b"\x1b]7;file://host/home\x07",
                b"\x1b]9;9;/home\x07",
            ]
        );
    }

    #[test]
    fn does_not_relay_clipboard_query() {
        assert!(passed_through(&[b"\x1b]52;c;?\x07"]).is_empty());
    }
}
