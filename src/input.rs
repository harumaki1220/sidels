//! 入力係: キーボード → シェル。sidels のキー（パネルの出し入れ・スクロール）と貼り付けの印を処理する

use std::io::{self, Read, Write};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::AppEvent;

/// sidels が横取りするキー。端末からはこのバイト列で届く
#[derive(Clone, Copy)]
enum SpecialKey {
    /// F12: パネルの出し入れ
    TogglePanel,
    /// 左ペインの履歴をスクロールする
    Scroll(ScrollStep),
}

#[derive(Clone, Copy)]
enum ScrollStep {
    /// Shift+PageUp / Shift+PageDown: 半画面ずつ
    HalfPageUp,
    HalfPageDown,
    /// Shift+↑ / Shift+↓: SCROLL_LINES 行ずつ（PageUp が押しにくいキーボード向け）
    LinesUp,
    LinesDown,
}

const SPECIAL_KEYS: &[(&[u8], SpecialKey)] = &[
    (b"\x1b[24~", SpecialKey::TogglePanel),
    (b"\x1b[5;2~", SpecialKey::Scroll(ScrollStep::HalfPageUp)),
    (b"\x1b[6;2~", SpecialKey::Scroll(ScrollStep::HalfPageDown)),
    (b"\x1b[1;2A", SpecialKey::Scroll(ScrollStep::LinesUp)),
    (b"\x1b[1;2B", SpecialKey::Scroll(ScrollStep::LinesDown)),
];

/// Shift+↑ / Shift+↓ で一度に動かす行数
const SCROLL_LINES: usize = 3;

/// 貼り付けの開始・終了の印。外側のターミナルが貼り付けた文字をこれで囲んで送ってくる
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// キーボード → シェル。sidels のキーだけは抜き取って自分で処理する
pub fn spawn_input_thread(
    mut writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // vim などが別画面を使っている間は、スクロールのキーもアプリに渡す
                    let (can_scroll, app_wants_paste) = {
                        let parser = parser.lock().unwrap();
                        let screen = parser.screen();
                        (!screen.alternate_screen(), screen.bracketed_paste())
                    };
                    let (input, keys) = take_special_keys(&buf[..n], can_scroll);
                    // 貼り付けの印は、中のアプリ（zsh など）が頼んでいるときだけ渡す。
                    // 頼んでいないアプリに渡すと、印がそのまま文字として入ってしまう
                    let input = if app_wants_paste {
                        input
                    } else {
                        remove_all(&remove_all(&input, PASTE_START), PASTE_END)
                    };
                    for key in keys {
                        let event = match key {
                            SpecialKey::TogglePanel => AppEvent::TogglePanel,
                            SpecialKey::Scroll(step) => {
                                scroll(&parser, step);
                                AppEvent::Output
                            }
                        };
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    if input.is_empty() {
                        continue;
                    }
                    // 普通のターミナルと同じく、何か打ったら最新の画面に戻る
                    let mut parser_guard = parser.lock().unwrap();
                    if parser_guard.screen().scrollback() > 0 {
                        parser_guard.screen_mut().set_scrollback(0);
                        let _ = tx.send(AppEvent::Output);
                    }
                    drop(parser_guard);
                    if writer.write_all(&input).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// 入力から sidels のキーを取り除き、残りのバイト列と押されたキーを返す。
/// can_scroll が false ならスクロールのキーは取り除かない
fn take_special_keys(input: &[u8], can_scroll: bool) -> (Vec<u8>, Vec<SpecialKey>) {
    let mut rest = Vec::with_capacity(input.len());
    let mut keys = Vec::new();
    let mut i = 0;
    'outer: while i < input.len() {
        for &(seq, key) in SPECIAL_KEYS {
            let is_scroll = matches!(key, SpecialKey::Scroll(_));
            if input[i..].starts_with(seq) && (can_scroll || !is_scroll) {
                keys.push(key);
                i += seq.len();
                continue 'outer;
            }
        }
        rest.push(input[i]);
        i += 1;
    }
    (rest, keys)
}

/// input から needle をすべて取り除く
fn remove_all(input: &[u8], needle: &[u8]) -> Vec<u8> {
    let mut rest = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with(needle) {
            i += needle.len();
        } else {
            rest.push(input[i]);
            i += 1;
        }
    }
    rest
}

/// 履歴をさかのぼる・進める。vt100 が履歴の範囲に収めてくれる
fn scroll(parser: &Mutex<vt100::Parser>, step: ScrollStep) {
    let mut parser = parser.lock().unwrap();
    let screen = parser.screen_mut();
    let half_page = usize::from(screen.size().0 / 2).max(1);
    let offset = screen.scrollback();
    let offset = match step {
        ScrollStep::HalfPageUp => offset + half_page,
        ScrollStep::HalfPageDown => offset.saturating_sub(half_page),
        ScrollStep::LinesUp => offset + SCROLL_LINES,
        ScrollStep::LinesDown => offset.saturating_sub(SCROLL_LINES),
    };
    screen.set_scrollback(offset);
}
