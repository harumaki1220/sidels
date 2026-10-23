//! シェル側だけの文字選択とコピー。
//!
//! マウスを受け取るとターミナル標準の選択が使えなくなるので、ドラッグでの選択とコピーを自分で行う。

use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

/// 入力係とメインループで共有する選択範囲。`None` なら何も選んでいない。
///
/// デッドロックを防ぐため、描画側は先にこれを読み取ってから仮想端末をロックし、
/// 入力係は両方を同時にロックしない。
pub type SharedSelection = Arc<Mutex<Option<Selection>>>;

/// 選択範囲。
///
/// 位置は `(行, 列)` で、今シェル側に見えている画面の左上が `(0, 0)`。
/// 入力係（マウス）が書き換え、メインループ（描画）が読む。
#[derive(Clone, Copy)]
pub struct Selection {
    /// ドラッグを始めた位置。
    anchor: (u16, u16),
    /// 今マウスがある位置。
    head: (u16, u16),
}

impl Selection {
    /// `pos` から選択を始める。
    pub fn new(pos: (u16, u16)) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    /// 選択の終わりを `pos` まで広げる（縮める）。
    pub fn extend(&mut self, pos: (u16, u16)) {
        self.head = pos;
    }

    /// クリックしただけで、何も選んでいない。
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    /// 画面の上から順に並べた `(始まり, 終わり)`。
    ///
    /// 上へドラッグしたときも、下へドラッグしたときと同じように扱うために使う。
    fn ordered(self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// その位置が選択範囲に入っているか。
    ///
    /// 行をまたぐときは、始まりの行は右端まで、終わりの行は左端からが範囲に入る。
    pub fn contains(self, row: u16, col: u16) -> bool {
        let (start, end) = self.ordered();
        start <= (row, col) && (row, col) <= end
    }

    /// 選んだ文字。
    ///
    /// 折り返された行はつなげ、行末の空白は落とす（vt100 に任せる）。
    pub fn text(self, screen: &vt100::Screen) -> String {
        let (start, end) = self.ordered();
        // contents_between の終わりの列は含まないので、1 つ先まで指定する
        screen.contents_between(start.0, start.1, end.0, end.1 + 1)
    }
}

/// 外側のターミナルが OSC 52 を受け付けないので、代わりに `pbcopy` を使うか。
///
/// macOS 標準の Terminal.app が当てはまる。
/// SSH 先では `TERM_PROGRAM` はふつう渡らないので、手元の Mac で動いているときだけ `true` になる。
pub fn uses_pbcopy() -> bool {
    cfg!(target_os = "macos")
        && std::env::var_os("TERM_PROGRAM").is_some_and(|program| program == "Apple_Terminal")
}

/// `pbcopy` で `text` をクリップボードに入れる。
///
/// 終わるのを待つとメインループが止まるので、別のスレッドで渡す。
pub fn pbcopy(text: Vec<u8>) {
    thread::spawn(move || -> io::Result<()> {
        let mut child = Command::new("pbcopy")
            // ロケールが UTF-8 でないと、pbcopy は日本語などを文字化けさせる
            .env("LC_CTYPE", "UTF-8")
            .stdin(Stdio::piped())
            // エラーを画面に書かれると描画が崩れる
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // 書き終えたら閉じて、pbcopy に終わりを知らせる
        child
            .stdin
            .take()
            .expect("stdin は piped")
            .write_all(&text)?;
        child.wait()?;
        Ok(())
    });
}

/// 外側のターミナルにクリップボードへ書き込んでもらう制御文字列（OSC 52）。
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

/// `bytes` を base64 にする（OSC 52 は中身を base64 で送る決まり）。
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // 3 バイト（24 ビット）を 6 ビットずつ 4 文字にする。足りない分は = で埋める
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// base64 を元に戻す。
///
/// 中のアプリが OSC 52 で送ってきた中身を `pbcopy` に渡すのに使う。base64 でなければ `None`。
pub fn decode_base64(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut bits = 0u32;
    let mut count = 0;
    for &c in text.iter().take_while(|&&c| c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        // 6 ビットずつためて、8 ビットたまったら 1 バイト取り出す
        bits = (bits << 6 | u32::from(value)) & 0xffff;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_what_it_encodes() {
        for text in ["", "a", "ab", "abc", "hello", "日本語のテキスト"] {
            let encoded = base64(text.as_bytes());
            assert_eq!(decode_base64(encoded.as_bytes()).unwrap(), text.as_bytes());
        }
    }

    #[test]
    fn rejects_non_base64() {
        assert_eq!(decode_base64(b"?"), None);
    }
}
