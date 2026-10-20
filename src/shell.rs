//! シェル側。
//!
//! PTY の出力を仮想端末（vt100）に流し込み、その画面から入力中の内容を読む。
//! 中のアプリが出したクリップボードへの書き込み（OSC 52）は、外側のターミナルに伝える。

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use portable_pty::{MasterPty, PtySize};

use crate::AppEvent;

/// 左ペインの履歴として覚えておく行数。
pub const SCROLLBACK_LINES: usize = 10_000;

/// クリップボードへの書き込み（OSC 52）の始まり。続きは `選択先 ; base64 の中身` で、BEL か ST で終わる。
const OSC52_START: &[u8] = b"\x1b]52;";

/// 途中までしか届いていない OSC 52 を取っておく上限。
///
/// 終わりが来ないまま大量の出力が続いても、メモリを使い続けないようにする。
const OSC52_MAX_LEN: usize = 1 << 20;

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
        // 前回の読み込みの最後で途中まで届いていた OSC 52。次に届いた分とつなげて探す
        let mut osc52_carry = Vec::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    parser.lock().unwrap().process(&buf[..n]);
                    // vt100 は OSC 52 を捨てるので、ここで拾って外側のターミナルに伝える。
                    // 伝えないと、Claude Code や vim が選択した文字をコピーしてもクリップボードに入らない
                    for sequence in take_osc52(&mut osc52_carry, &buf[..n]) {
                        if tx.send(AppEvent::PassThrough(sequence)).is_err() {
                            return;
                        }
                    }
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

/// 出力の中から、クリップボードへの書き込み（OSC 52）をそのままの形で取り出す。
///
/// 終わりまで届いていないものは `carry` に残し、次の読み込みとつなげて探す。
/// クリップボードの中身を読ませる問い合わせ（中身が `?`）は、中のアプリにクリップボードを覗かれるので取り出さない。
fn take_osc52(carry: &mut Vec<u8>, data: &[u8]) -> Vec<Vec<u8>> {
    let mut buf = std::mem::take(carry);
    buf.extend_from_slice(data);
    let mut found = Vec::new();
    let mut pos = 0;
    while let Some(offset) = find(&buf[pos..], OSC52_START) {
        let start = pos + offset;
        let body = start + OSC52_START.len();
        let Some((body_len, end_len)) = find_string_end(&buf[body..]) else {
            // 終わりがまだ届いていない
            if buf.len() - start <= OSC52_MAX_LEN {
                *carry = buf[start..].to_vec();
            }
            return found;
        };
        let end = body + body_len + end_len;
        if !is_clipboard_query(&buf[body..body + body_len]) {
            found.push(buf[start..end].to_vec());
        }
        pos = end;
    }
    // 最後が OSC 52 の始まりの途中（`ESC ] 5` など）で切れていたら、次の読み込みのために残す
    let tail_from = pos.max(buf.len().saturating_sub(OSC52_START.len() - 1));
    if let Some(k) = (tail_from..buf.len()).find(|&k| OSC52_START.starts_with(&buf[k..])) {
        *carry = buf[k..].to_vec();
    }
    found
}

/// `haystack` の中で `needle` が最初に現れる位置。
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 制御文字列の終わり（BEL か ST = `ESC \`）を探し、`(中身の長さ, 終わりの印の長さ)` を返す。
fn find_string_end(body: &[u8]) -> Option<(usize, usize)> {
    body.iter().enumerate().find_map(|(i, &b)| match b {
        0x07 => Some((i, 1)),
        0x1b if body.get(i + 1) == Some(&b'\\') => Some((i, 2)),
        _ => None,
    })
}

/// OSC 52 の中身（`選択先 ; データ`）が、クリップボードを読ませる問い合わせか。
fn is_clipboard_query(body: &[u8]) -> bool {
    body.splitn(2, |&b| b == b';').nth(1) == Some(b"?".as_slice())
}
