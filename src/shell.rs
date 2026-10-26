//! シェル側。
//!
//! PTY の出力を仮想端末（vt100）に流し込み、その画面から入力中の内容を読む。
//! シェル連携（[`ZSH_INIT`]）を読み込んだ zsh からは、入力中の内容が直接届くので、それを使う。
//! 中のアプリが出したクリップボードへの書き込み（OSC 52）、タイトル、今いる場所（OSC 7）は、外側のターミナルに伝える。

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{MasterPty, PtySize};

use crate::{AppEvent, selection};

/// 左ペインの履歴として覚えておく行数。
pub const SCROLLBACK_LINES: usize = 10_000;

/// `sidels init zsh` が出力する、zsh 用のシェル連携。
///
/// 入力欄が変わるたびに、[`INPUT_REPORT`] で始まる OSC でカーソル位置と中身を知らせてもらう。
pub const ZSH_INIT: &str = include_str!("init.zsh");

/// シェル連携が入力中の内容を知らせる OSC の、番号と目印。
///
/// 続きは `カーソル位置（文字数）; 入力欄の中身`。どのターミナルも使っていない番号にしてあり、外側には伝えない。
const INPUT_REPORT: [&[u8]; 2] = [b"7777", b"sidels"];

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
        // 外側が OSC 52 を受け付けないときは、中身を取り出して sidels のコピーと同じ道を通す
        if selection::uses_pbcopy() {
            if let Some(text) = selection::decode_base64(data) {
                let _ = self
                    .tx
                    .send(AppEvent::Copy(String::from_utf8_lossy(&text).into_owned()));
            }
            return;
        }
        self.pass_through([b"\x1b]52;", ty, b";", data, b"\x07"].concat());
    }

    // OSC 0（アイコン名とタイトル）でも呼ばれる。アイコン名は今のターミナルでは使われないので、タイトルだけ伝える
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.pass_through([b"\x1b]2;", title, b"\x07"].concat());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if let [number, tag, cursor, buffer @ ..] = params
            && [*number, *tag] == INPUT_REPORT
        {
            if let Some((line, word)) = parse_input_report(cursor, buffer) {
                let _ = self.tx.send(AppEvent::ShellInput { line, word });
            }
            return;
        }
        if is_relayed_osc(params) {
            self.pass_through([b"\x1b]", params.join(&b';').as_slice(), b"\x07"].concat());
        }
    }
}

/// シェル連携から届いたカーソル位置と入力欄の中身を、[`read_input`] と同じ `(行, カーソル直前の単語)` にする。
///
/// 入力欄の中身に `;` が残っていると vt100 が区切ってしまうので、つなげ直す。
fn parse_input_report(cursor: &[u8], buffer: &[&[u8]]) -> Option<(String, String)> {
    let cursor: usize = std::str::from_utf8(cursor).ok()?.parse().ok()?;
    let line = String::from_utf8_lossy(&buffer.join(&b';')).into_owned();
    // zsh の CURSOR は、バイト数ではなく文字数
    let before: String = line.chars().take(cursor).collect();
    let word = before
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or_default()
        .to_string();
    Some((line, word))
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
    fn input_report_becomes_shell_input_and_is_not_relayed() {
        let (tx, rx) = mpsc::channel();
        let mut parser = Parser::new_with_callbacks(24, 80, 0, Relay::new(tx));
        parser.process("\x1b]7777;sidels;6;cd sr日 x\x07".as_bytes());
        drop(parser);
        let events: Vec<_> = rx.iter().collect();
        assert!(matches!(
            &events[..],
            [AppEvent::ShellInput { line, word }] if line == "cd sr日 x" && word == "sr日"
        ));
    }

    #[test]
    fn input_report_rejoins_semicolons_and_handles_edges() {
        let report = |cursor: &[u8], buffer: &[&[u8]]| parse_input_report(cursor, buffer);
        assert_eq!(
            report(b"9", &[b"a", b"ls *.lo"]),
            Some(("a;ls *.lo".into(), "*.lo".into()))
        );
        // 空白の直後や空の入力欄では、単語は空
        assert_eq!(
            report(b"4", &[b"vim "]),
            Some(("vim ".into(), String::new()))
        );
        assert_eq!(report(b"0", &[b""]), Some((String::new(), String::new())));
        assert_eq!(report(b"x", &[b"ls"]), None);
    }

    #[test]
    fn does_not_relay_clipboard_query() {
        assert!(passed_through(&[b"\x1b]52;c;?\x07"]).is_empty());
    }
}
