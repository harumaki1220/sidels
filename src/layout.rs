//! 画面の分け方。
//!
//! シェルとパネルの幅と、パネルを出すかどうかの判定。

use portable_pty::PtySize;
use ratatui::layout::{Constraint, Layout, Rect};

/// パネルの幅（左の区切り線を含む）の既定値。`--width` で変えられる。
pub const DEFAULT_PANEL_WIDTH: u16 = 28;

/// 区切り線をドラッグして狭められる、パネルの幅の下限。
///
/// これより狭いと、名前がほとんど省略されて一覧として役に立たない。
const MIN_PANEL_WIDTH: u16 = 20;

/// シェルに残したい最低の幅。
///
/// パネルを出すとシェルがこれより狭くなるなら、パネルを自動で畳む。区切り線のドラッグでも、これより狭めない。
const MIN_SHELL_WIDTH: u16 = 80;

/// パネルを出すかどうかと、その幅。
///
/// 出すかどうかは普段は画面幅で自動で決め、F12 が押されたらそれに従う。
/// 幅は `--width` で始め、区切り線のドラッグで変える（閉じると戻る）。
pub struct PanelLayout {
    /// F12 で決めた表示状態。`None` なら画面幅で自動判定。
    forced: Option<bool>,
    /// 画面がパネルを出せるだけ広い。
    wide: bool,
    /// 画面の幅。
    screen_width: u16,
    /// パネルの幅（左の区切り線を含む）。
    panel_width: u16,
}

impl PanelLayout {
    pub fn new(screen_width: u16, panel_width: u16) -> Self {
        let panel_width = panel_width.max(MIN_PANEL_WIDTH);
        Self {
            forced: None,
            wide: screen_width >= MIN_SHELL_WIDTH + panel_width,
            screen_width,
            panel_width,
        }
    }

    /// 画面幅を伝える。
    ///
    /// 幅がしきい値をまたいだら、F12 の指定を忘れて自動判定に戻す。
    pub fn set_width(&mut self, screen_width: u16) {
        self.screen_width = screen_width;
        let wide = screen_width >= MIN_SHELL_WIDTH + self.panel_width;
        if wide != self.wide {
            self.wide = wide;
            self.forced = None;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.forced.unwrap_or(self.wide)
    }

    pub fn panel_width(&self) -> u16 {
        self.panel_width
    }

    /// F12 が押されたとき、今の表示状態を反対にする。
    pub fn toggle(&mut self) {
        self.forced = Some(!self.is_visible());
    }

    /// 区切り線を `col` 列目（1 始まり）までドラッグしたときに、パネルの幅を合わせる。変わったら `true`。
    ///
    /// パネルは [`MIN_PANEL_WIDTH`] 未満に、シェルは [`MIN_SHELL_WIDTH`] 未満にしない。
    /// 狭い画面で両方を守れないときは、パネルの下限を優先する。
    pub fn drag_divider(&mut self, col: u16) -> bool {
        let max = self
            .screen_width
            .saturating_sub(MIN_SHELL_WIDTH)
            .max(MIN_PANEL_WIDTH);
        // 区切り線から右端までがパネル
        let width = (self.screen_width + 1)
            .saturating_sub(col)
            .clamp(MIN_PANEL_WIDTH, max);
        if width == self.panel_width {
            return false;
        }
        self.panel_width = width;
        // ドラッグで画面に収まるように縮めたので、自動で畳まれないよう判定だけ合わせる（F12 の指定は残す）
        self.wide = self.screen_width >= MIN_SHELL_WIDTH + width;
        true
    }
}

/// 画面を `(シェル, パネル)` に分ける。
///
/// パネルを右端に `panel_width` 列で置き、残りを全部シェルに渡す。パネルを隠しているときはシェルが全体を使う。
pub fn split(area: Rect, panel_visible: bool, panel_width: u16) -> (Rect, Rect) {
    if !panel_visible {
        return (area, Rect::default());
    }
    // 狭い画面で F12 により無理やり出したときも、シェルが半分を下回らないようにする
    let panel_width = panel_width.min(area.width / 2);
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(panel_width)]).areas(area);
    (left, right)
}

/// シェル側の範囲と同じ大きさの PTY の大きさ。
pub fn pty_size_for(area: Rect) -> PtySize {
    PtySize {
        rows: area.height.max(1),
        cols: area.width.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDE_ENOUGH: u16 = MIN_SHELL_WIDTH + DEFAULT_PANEL_WIDTH;

    #[test]
    fn panel_shows_only_when_wide_enough() {
        assert!(PanelLayout::new(WIDE_ENOUGH, DEFAULT_PANEL_WIDTH).is_visible());
        assert!(!PanelLayout::new(WIDE_ENOUGH - 1, DEFAULT_PANEL_WIDTH).is_visible());
        // 広いパネルを頼んだら、その分だけ広い画面が要る
        assert!(!PanelLayout::new(WIDE_ENOUGH, 40).is_visible());
    }

    #[test]
    fn toggle_is_kept_until_width_crosses_threshold() {
        let mut layout = PanelLayout::new(120, DEFAULT_PANEL_WIDTH);
        layout.toggle();
        assert!(!layout.is_visible());
        // 広いままなら、F12 で隠したことを覚えている
        layout.set_width(130);
        assert!(!layout.is_visible());
        // 狭くなってからまた広がったら、自動判定に戻る
        layout.set_width(100);
        layout.set_width(120);
        assert!(layout.is_visible());
    }

    #[test]
    fn drag_divider_keeps_both_sides_usable() {
        let mut layout = PanelLayout::new(140, DEFAULT_PANEL_WIDTH);
        // 101 列目まで動かすと、101〜140 列目の 40 列がパネル
        assert!(layout.drag_divider(101));
        assert_eq!(layout.panel_width(), 40);
        // シェルを 80 列より狭くはしない
        layout.drag_divider(10);
        assert_eq!(layout.panel_width(), 140 - MIN_SHELL_WIDTH);
        // パネルを下限より狭くはしない
        layout.drag_divider(139);
        assert_eq!(layout.panel_width(), MIN_PANEL_WIDTH);
        assert!(!layout.drag_divider(139));
        assert!(layout.is_visible());
    }

    #[test]
    fn split_keeps_panel_width_and_at_least_half_for_shell() {
        let (shell, panel) = split(Rect::new(0, 0, 120, 40), true, 28);
        assert_eq!((shell.width, panel.width), (92, 28));
        let (shell, panel) = split(Rect::new(0, 0, 40, 40), true, 28);
        assert_eq!((shell.width, panel.width), (20, 20));
        let (shell, _) = split(Rect::new(0, 0, 120, 40), false, 28);
        assert_eq!(shell.width, 120);
    }
}
