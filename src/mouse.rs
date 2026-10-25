//! マウス操作の読み取りと書き出し。
//!
//! 外側のターミナルから届くマウス操作（SGR 形式）を読み、中のアプリが頼んだ形式で書き出す。

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// 外側のターミナルにマウス操作を送ってもらうための制御文字列。
///
/// 1002 は押す・離す・押したままの移動（ドラッグ）、1006 は SGR 形式（座標の大きさに制限がない）。
pub const ENABLE: &[u8] = b"\x1b[?1002h\x1b[?1006h";
/// [`ENABLE`] で頼んだマウス操作の送信をやめてもらう。
pub const DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1002l";

const SGR_PREFIX: &[u8] = b"\x1b[<";

/// 外側のターミナルから届いたマウス操作。
#[derive(Clone, Copy)]
pub struct MouseEvent {
    /// ボタンと修飾キーを表す番号（SGR 形式の値そのまま）。
    pub code: u16,
    /// 列（1 始まり）。
    pub col: u16,
    /// 行（1 始まり）。
    pub row: u16,
    /// 押した（または押したまま動かした）なら `true`、離したなら `false`。
    pub pressed: bool,
}

impl MouseEvent {
    /// ホイールを回した。
    pub fn is_wheel(self) -> bool {
        self.code & 64 != 0
    }

    /// ホイールを上に回した。
    pub fn is_wheel_up(self) -> bool {
        self.is_wheel() && self.code & 1 == 0
    }

    /// ボタンを押したまま動かした（ドラッグ中）。
    pub fn is_motion(self) -> bool {
        self.code & 32 != 0
    }

    /// 左ボタンの操作（押す・ドラッグ・離す）。
    pub fn is_left(self) -> bool {
        !self.is_wheel() && self.code & 3 == 0
    }
}

/// 入力の先頭が SGR 形式のマウス操作なら、それと長さを返す。
///
/// SGR 形式は `ESC [ < 番号 ; 列 ; 行 M`（押した）または `m`（離した）。
pub fn parse_sgr(input: &[u8]) -> Option<(MouseEvent, usize)> {
    let rest = input.strip_prefix(SGR_PREFIX)?;
    let end = rest.iter().position(|&b| b == b'M' || b == b'm')?;
    let body = std::str::from_utf8(&rest[..end]).ok()?;
    let mut numbers = body.split(';').map(|n| n.parse::<u16>().ok());
    let code = numbers.next()??;
    let col = numbers.next()??;
    let row = numbers.next()??;
    if numbers.next().is_some() {
        return None;
    }
    let event = MouseEvent {
        code,
        col,
        row,
        pressed: rest[end] == b'M',
    };
    Some((event, SGR_PREFIX.len() + end + 1))
}

/// 中のアプリ（vim など）が頼んだ種類・形式でマウス操作を書き出す。
///
/// 頼まれていない種類の操作（押すだけを頼まれているときの「離した」など）なら `None`。
pub fn encode_for_app(
    event: MouseEvent,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let wanted = match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => event.pressed && !event.is_motion(),
        MouseProtocolMode::PressRelease => !event.is_motion(),
        MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion => true,
    };
    if !wanted {
        return None;
    }

    let (code, col, row) = (event.code, event.col, event.row);
    if encoding == MouseProtocolEncoding::Sgr {
        let end = if event.pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{col};{row}{end}").into_bytes());
    }

    // 昔からの形式では「離した」をボタン番号 3 で表し、各値に 32 を足して 1 文字ずつ送る
    let code = if event.pressed { code } else { (code & !3) | 3 };
    let mut out = b"\x1b[M".to_vec();
    for value in [code, col, row] {
        let value = u32::from(value) + 32;
        if encoding == MouseProtocolEncoding::Utf8 {
            let mut buf = [0u8; 4];
            out.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut buf).as_bytes());
        } else {
            // 1 バイトに収まらない位置（223 列より右など）は送れない
            out.push(u8::try_from(value).ok()?);
        }
    }
    Some(out)
}

/// 入力の最後で途中まで届いているマウス操作の、始まりの位置を返す。
///
/// 途中で切れていなければ入力の長さを返す。途中で切れているとは、`ESC [ <` の後に数字と `;` しかない状態。
/// ドラッグ中は操作が大量に届くので、読み込みの区切り目で切れることがある。
///
/// `ESC` や `ESC [` だけで終わっている場合は途中とみなさない。Esc キーや Alt+[ を押しただけの可能性があり、
/// 取っておくと次のキーを押すまでシェルに届かなくなる（vim で Esc が効かなくなる）。
pub fn incomplete_tail_start(input: &[u8]) -> usize {
    let Some(start) = input.iter().rposition(|&b| b == 0x1b) else {
        return input.len();
    };
    let tail = &input[start..];
    let is_incomplete_mouse = tail.starts_with(SGR_PREFIX)
        && tail[SGR_PREFIX.len()..]
            .iter()
            .all(|&b| b.is_ascii_digit() || b == b';');
    if is_incomplete_mouse {
        start
    } else {
        input.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(input: &[u8]) -> Option<(u16, u16, u16, bool, usize)> {
        parse_sgr(input).map(|(e, len)| (e.code, e.col, e.row, e.pressed, len))
    }

    fn event(code: u16, pressed: bool) -> MouseEvent {
        MouseEvent {
            code,
            col: 10,
            row: 5,
            pressed,
        }
    }

    #[test]
    fn parse_sgr_press_and_release() {
        assert_eq!(parsed(b"\x1b[<0;10;5Mrest"), Some((0, 10, 5, true, 10)));
        assert_eq!(parsed(b"\x1b[<64;1;2m"), Some((64, 1, 2, false, 10)));
    }

    #[test]
    fn parse_sgr_rejects_incomplete_or_malformed() {
        assert_eq!(parsed(b"\x1b[<0;10"), None);
        assert_eq!(parsed(b"\x1b[<0;10;5;1M"), None);
        assert_eq!(parsed(b"abc"), None);
    }

    #[test]
    fn incomplete_tail_is_only_a_cut_mouse_event() {
        assert_eq!(incomplete_tail_start(b"ab\x1b[<0;1"), 2);
        assert_eq!(incomplete_tail_start(b"\x1b[<0;1;1M"), 9);
        assert_eq!(incomplete_tail_start(b"ab"), 2);
        // Esc キーや Alt+[ かもしれないので取っておかない
        assert_eq!(incomplete_tail_start(b"ab\x1b"), 3);
        assert_eq!(incomplete_tail_start(b"ab\x1b["), 4);
    }

    #[test]
    fn encode_sgr_for_app() {
        let sgr = MouseProtocolEncoding::Sgr;
        let mode = MouseProtocolMode::PressRelease;
        assert_eq!(
            encode_for_app(event(0, true), mode, sgr).as_deref(),
            Some(b"\x1b[<0;10;5M".as_slice())
        );
        assert_eq!(
            encode_for_app(event(0, false), mode, sgr).as_deref(),
            Some(b"\x1b[<0;10;5m".as_slice())
        );
    }

    #[test]
    fn encode_default_marks_release_as_button_3() {
        let bytes = encode_for_app(
            event(0, false),
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
        );
        assert_eq!(bytes.as_deref(), Some(b"\x1b[M#*%".as_slice()));
    }

    #[test]
    fn encode_skips_what_the_app_did_not_ask_for() {
        let sgr = MouseProtocolEncoding::Sgr;
        assert!(encode_for_app(event(0, false), MouseProtocolMode::Press, sgr).is_none());
        // ドラッグ（32 が立っている）は ButtonMotion から
        assert!(encode_for_app(event(32, true), MouseProtocolMode::PressRelease, sgr).is_none());
        assert!(encode_for_app(event(32, true), MouseProtocolMode::ButtonMotion, sgr).is_some());
    }

    #[test]
    fn encode_default_cannot_send_far_columns() {
        let far = MouseEvent {
            col: 300,
            ..event(0, true)
        };
        let mode = MouseProtocolMode::Press;
        assert!(encode_for_app(far, mode, MouseProtocolEncoding::Default).is_none());
        assert!(encode_for_app(far, mode, MouseProtocolEncoding::Utf8).is_some());
    }
}
