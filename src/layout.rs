//! 画面の分け方。
//!
//! シェルとパネルの幅と、パネルを出すかどうかの判定。

use portable_pty::PtySize;
use ratatui::layout::{Constraint, Layout, Rect};

/// パネルの幅（左の区切り線を含む）。
const PANEL_WIDTH: u16 = 28;

/// シェルに残したい最低の幅。
///
/// パネルを出すとシェルがこれより狭くなるなら、パネルを自動で畳む。
const MIN_SHELL_WIDTH: u16 = 80;

const MIN_WIDTH_FOR_PANEL: u16 = MIN_SHELL_WIDTH + PANEL_WIDTH;

/// パネルを出すかどうか。
///
/// 普段は画面幅で自動で決め、F12 が押されたらそれに従う。
pub struct PanelVisibility {
    /// F12 で決めた表示状態。`None` なら画面幅で自動判定。
    forced: Option<bool>,
    /// 画面がパネルを出せるだけ広い。
    wide: bool,
}

impl PanelVisibility {
    pub fn new(width: u16) -> Self {
        Self {
            forced: None,
            wide: width >= MIN_WIDTH_FOR_PANEL,
        }
    }

    /// 画面幅を伝える。
    ///
    /// 幅がしきい値をまたいだら、F12 の指定を忘れて自動判定に戻す。
    pub fn set_width(&mut self, width: u16) {
        let wide = width >= MIN_WIDTH_FOR_PANEL;
        if wide != self.wide {
            self.wide = wide;
            self.forced = None;
        }
    }

    pub fn is_visible(&self) -> bool {
        self.forced.unwrap_or(self.wide)
    }

    /// F12 が押されたとき、今の表示状態を反対にする。
    pub fn toggle(&mut self) {
        self.forced = Some(!self.is_visible());
    }
}

/// 画面を `(シェル, パネル)` に分ける。
///
/// パネルを右端に固定幅で置き、残りを全部シェルに渡す。パネルを隠しているときはシェルが全体を使う。
pub fn split(area: Rect, panel_visible: bool) -> (Rect, Rect) {
    if !panel_visible {
        return (area, Rect::default());
    }
    // 狭い画面で F12 により無理やり出したときも、シェルが半分を下回らないようにする
    let panel_width = PANEL_WIDTH.min(area.width / 2);
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

    #[test]
    fn panel_shows_only_when_wide_enough() {
        assert!(PanelVisibility::new(MIN_WIDTH_FOR_PANEL).is_visible());
        assert!(!PanelVisibility::new(MIN_WIDTH_FOR_PANEL - 1).is_visible());
    }

    #[test]
    fn toggle_is_kept_until_width_crosses_threshold() {
        let mut visibility = PanelVisibility::new(120);
        visibility.toggle();
        assert!(!visibility.is_visible());
        // 広いままなら、F12 で隠したことを覚えている
        visibility.set_width(130);
        assert!(!visibility.is_visible());
        // 狭くなってからまた広がったら、自動判定に戻る
        visibility.set_width(100);
        visibility.set_width(120);
        assert!(visibility.is_visible());
    }

    #[test]
    fn split_keeps_panel_width_and_at_least_half_for_shell() {
        let (shell, panel) = split(Rect::new(0, 0, 120, 40), true);
        assert_eq!((shell.width, panel.width), (120 - PANEL_WIDTH, PANEL_WIDTH));
        let (shell, panel) = split(Rect::new(0, 0, 40, 40), true);
        assert_eq!((shell.width, panel.width), (20, 20));
        let (shell, _) = split(Rect::new(0, 0, 120, 40), false);
        assert_eq!(shell.width, 120);
    }
}
