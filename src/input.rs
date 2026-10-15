//! 入力係: キーボードとマウスの入力をシェルに渡す。
//!
//! sidels のキー（パネルの出し入れ・スクロール）、マウス操作、貼り付けの印はここで処理する。

use std::io::{self, Read, Write};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::AppEvent;
use crate::mouse::{self, MouseEvent};
use crate::selection::{Selection, SharedSelection};

/// sidels が横取りするキー。
///
/// 端末からは [`SPECIAL_KEYS`] のバイト列で届く。
#[derive(Clone, Copy)]
enum SpecialKey {
    /// F12: パネルの出し入れ。
    TogglePanel,
    /// 左ペインの履歴をスクロールする。
    Scroll(ScrollStep),
    /// マウス操作（sidels が外側のターミナルに頼んで送ってもらっている）。
    Mouse(MouseEvent),
}

#[derive(Clone, Copy)]
enum ScrollStep {
    /// Shift+PageUp: 半画面さかのぼる。
    HalfPageUp,
    /// Shift+PageDown: 半画面進む。
    HalfPageDown,
    /// Shift+↑: [`SCROLL_LINES`] 行さかのぼる（PageUp が押しにくいキーボード向け）。
    LinesUp,
    /// Shift+↓: [`SCROLL_LINES`] 行進む。
    LinesDown,
}

const SPECIAL_KEYS: &[(&[u8], SpecialKey)] = &[
    (b"\x1b[24~", SpecialKey::TogglePanel),
    (b"\x1b[5;2~", SpecialKey::Scroll(ScrollStep::HalfPageUp)),
    (b"\x1b[6;2~", SpecialKey::Scroll(ScrollStep::HalfPageDown)),
    (b"\x1b[1;2A", SpecialKey::Scroll(ScrollStep::LinesUp)),
    (b"\x1b[1;2B", SpecialKey::Scroll(ScrollStep::LinesDown)),
];

/// Shift+↑ / Shift+↓ やホイールで一度に動かす行数。
const SCROLL_LINES: usize = 3;

/// 貼り付けの開始の印。
///
/// 外側のターミナルは、貼り付けた文字を [`PASTE_START`] と [`PASTE_END`] で囲んで送ってくる。
const PASTE_START: &[u8] = b"\x1b[200~";
/// 貼り付けの終わりの印。
const PASTE_END: &[u8] = b"\x1b[201~";

/// 入力係のスレッドを立てる。
///
/// キーボードとマウスの入力をシェルに渡す。sidels のキーとマウス操作だけは抜き取って自分で処理する。
pub fn spawn_input_thread(
    mut writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    selection: SharedSelection,
    tx: Sender<AppEvent>,
) {
    thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        // 前回の読み込みの最後で途中まで届いていたマウス操作。次に届いた分とつなげて読む
        let mut carry: Vec<u8> = Vec::new();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut data = std::mem::take(&mut carry);
                    data.extend_from_slice(&buf[..n]);
                    let complete = mouse::incomplete_tail_start(&data);
                    carry = data.split_off(complete);
                    // vim などが別画面を使っている間は、スクロールのキーもアプリに渡す
                    let (can_scroll, app_wants_paste) = {
                        let parser = parser.lock().unwrap();
                        let screen = parser.screen();
                        (!screen.alternate_screen(), screen.bracketed_paste())
                    };
                    let (input, keys) = take_special_keys(&data, can_scroll);
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
                                // 見えている範囲が動くので、選択は外す
                                clear_selection(&selection);
                                scroll(&parser, step);
                                AppEvent::Output
                            }
                            SpecialKey::Mouse(event) => {
                                match handle_mouse(event, &parser, &selection, &mut writer) {
                                    Some(event) => event,
                                    None => continue,
                                }
                            }
                        };
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    if input.is_empty() {
                        continue;
                    }
                    if clear_selection(&selection) {
                        let _ = tx.send(AppEvent::Output);
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
///
/// `can_scroll` が `false` なら、スクロールのキーは取り除かずにアプリへ渡す。
fn take_special_keys(input: &[u8], can_scroll: bool) -> (Vec<u8>, Vec<SpecialKey>) {
    let mut rest = Vec::with_capacity(input.len());
    let mut keys = Vec::new();
    let mut i = 0;
    'outer: while i < input.len() {
        // マウス操作はシェルに文字として届くと困るので、必ず抜き取る
        if let Some((event, len)) = mouse::parse_sgr(&input[i..]) {
            keys.push(SpecialKey::Mouse(event));
            i += len;
            continue;
        }
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

/// マウス操作を振り分ける。
///
/// 描き直しが必要なら、メインループへの知らせを返す。
///
/// - 中のアプリがマウスを頼んでいる（vim や htop など）: シェル側の操作だけ、アプリが頼んだ形式で渡す。
///   シェル側は画面の左上にあるので、座標はそのままでよい
/// - 左ボタン: シェル側の文字を選択する
/// - ホイール: 別画面（less など）なら ↑↓ キーとして渡し、普段は履歴をスクロールする
/// - パネルの上でのそれ以外の操作: 何もしない
fn handle_mouse(
    event: MouseEvent,
    parser: &Mutex<vt100::Parser>,
    selection: &Mutex<Option<Selection>>,
    writer: &mut Box<dyn Write + Send>,
) -> Option<AppEvent> {
    let (rows, cols, mode, encoding, alternate, app_cursor) = {
        let parser = parser.lock().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        (
            rows,
            cols,
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            screen.alternate_screen(),
            screen.application_cursor(),
        )
    };
    let in_shell = event.col <= cols && event.row <= rows;

    if mode != vt100::MouseProtocolMode::None {
        if in_shell && let Some(bytes) = mouse::encode_for_app(event, mode, encoding) {
            let _ = writer.write_all(&bytes).and_then(|()| writer.flush());
        }
        return None;
    }

    if event.is_left() {
        // パネルの上までドラッグしても、選ぶのはシェル側の端まで
        let pos = (
            event.row.clamp(1, rows.max(1)) - 1,
            event.col.clamp(1, cols.max(1)) - 1,
        );
        return handle_selection(event, in_shell, pos, parser, selection);
    }

    if !event.is_wheel() || !in_shell {
        return None;
    }
    // 見えている範囲が動くので、選択は外す
    clear_selection(selection);
    let up = event.is_wheel_up();
    if alternate {
        let key: &[u8] = match (up, app_cursor) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        let _ = writer
            .write_all(&key.repeat(SCROLL_LINES))
            .and_then(|()| writer.flush());
        return None;
    }
    let step = if up {
        ScrollStep::LinesUp
    } else {
        ScrollStep::LinesDown
    };
    scroll(parser, step);
    Some(AppEvent::Output)
}

/// 左ボタンでの選択。
///
/// 押すと始まり、ドラッグで広げ、離したら選んだ文字をコピーする。
/// 何も選ばずに離した（クリックしただけ）なら選択を外す。
fn handle_selection(
    event: MouseEvent,
    in_shell: bool,
    pos: (u16, u16),
    parser: &Mutex<vt100::Parser>,
    selection: &Mutex<Option<Selection>>,
) -> Option<AppEvent> {
    let finished = {
        let mut current = selection.lock().unwrap();
        if event.pressed && !event.is_motion() {
            // パネルの上で押したときは、選択を外すだけ
            *current = in_shell.then(|| Selection::new(pos));
            return Some(AppEvent::Output);
        }
        let selected = current.as_mut()?;
        selected.extend(pos);
        if event.pressed {
            return Some(AppEvent::Output);
        }
        if selected.is_empty() {
            *current = None;
            return Some(AppEvent::Output);
        }
        *selected
    };
    // 選択のロックを外してから仮想端末をロックする（描画側と順番が逆になるとデッドロックする）
    let text = finished.text(parser.lock().unwrap().screen());
    Some(AppEvent::Copy(text))
}

/// 選択を外す。
///
/// 選択していて外したなら `true`（描き直しが必要）。
fn clear_selection(selection: &Mutex<Option<Selection>>) -> bool {
    selection.lock().unwrap().take().is_some()
}

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

/// 履歴をさかのぼる・進める。
///
/// 履歴の範囲を超えないようにするのは vt100 がやってくれる。
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
