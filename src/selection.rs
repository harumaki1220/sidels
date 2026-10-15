//! シェル側だけの文字選択とコピー。
//! マウスを受け取るとターミナル標準の選択が使えなくなるので、ドラッグでの選択とコピーを自分で行う

use std::sync::{Arc, Mutex};

/// 入力係とメインループで共有する選択範囲。None なら何も選んでいない。
/// 描画側は先にこれを読み取ってから仮想端末をロックし、入力係は両方を同時にロックしない（デッドロック防止）
pub type SharedSelection = Arc<Mutex<Option<Selection>>>;

/// 選択範囲。位置は (行, 列) で、今シェル側に見えている画面の左上が (0, 0)。
/// 入力係（マウス）が書き換え、メインループ（描画）が読む
#[derive(Clone, Copy)]
pub struct Selection {
    /// ドラッグを始めた位置
    anchor: (u16, u16),
    /// 今マウスがある位置
    head: (u16, u16),
}

impl Selection {
    pub fn new(pos: (u16, u16)) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    pub fn extend(&mut self, pos: (u16, u16)) {
        self.head = pos;
    }

    /// クリックしただけで、何も選んでいない
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// 画面の上から順に並べた (始まり, 終わり)。上へドラッグしたときも同じように扱うため
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// その位置が選択範囲に入っているか。行をまたぐときは、始まりの行は右端まで、終わりの行は左端から
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let (start, end) = self.ordered();
        start <= (row, col) && (row, col) <= end
    }

    /// 選んだ文字。折り返された行はつなげ、行末の空白は落とす（vt100 に任せる）
    pub fn text(&self, screen: &vt100::Screen) -> String {
        let (start, end) = self.ordered();
        // contents_between の終わりの列は含まないので、1 つ先まで指定する
        screen.contents_between(start.0, start.1, end.0, end.1 + 1)
    }
}

/// OSC 52: 外側のターミナルにクリップボードへ書き込んでもらう制御文字列
pub fn osc52(text: &str) -> Vec<u8> {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes())).into_bytes()
}

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
