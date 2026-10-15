//! マウス: 外側のターミナルから届くマウス操作（SGR 形式）を読み、中のアプリが頼んだ形式で書き出す

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// 外側のターミナルにマウス操作を送ってもらう。
/// 1002: 押す・離す・押したままの移動（ドラッグ）、1006: SGR 形式（座標の大きさに制限がない）
pub const ENABLE: &[u8] = b"\x1b[?1002h\x1b[?1006h";
pub const DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1002l";

const SGR_PREFIX: &[u8] = b"\x1b[<";

/// 届いたマウス操作。座標は 1 始まり
#[derive(Clone, Copy)]
pub struct MouseEvent {
    /// ボタンと修飾キーを表す番号（SGR 形式の値そのまま）
    pub code: u16,
    pub col: u16,
    pub row: u16,
    /// 押した（または動かした）なら true、離したなら false
    pub pressed: bool,
}

impl MouseEvent {
    pub fn is_wheel(&self) -> bool {
        self.code & 64 != 0
    }

    pub fn is_wheel_up(&self) -> bool {
        self.is_wheel() && self.code & 1 == 0
    }

    fn is_motion(&self) -> bool {
        self.code & 32 != 0
    }
}

/// 入力の先頭が SGR 形式のマウス操作（`ESC [ < 番号 ; 列 ; 行 M/m`）なら、それと長さを返す
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

/// 中のアプリ（vim など）が頼んだ種類・形式でマウス操作を書き出す。頼まれていない種類なら None
pub fn encode_for_app(
    event: &MouseEvent,
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

/// 入力の最後が途中で切れたマウス操作（`ESC [ <` の後に数字と `;` だけ）なら、その始まりの位置を返す。
/// 切れていなければ入力の長さを返す。ドラッグ中は操作が大量に届くので、読み込みの区切り目で切れることがある。
/// `ESC` や `ESC [` だけで終わっている場合は取っておかない。Esc キーや Alt+[ を押しただけの可能性があり、
/// 取っておくと次のキーを押すまでシェルに届かなくなる（vim で Esc が効かなくなる）
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
