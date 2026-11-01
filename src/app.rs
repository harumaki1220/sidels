//! メインループ。
//!
//! シェルを起動し、画面の大きさを合わせ、パネルを更新し、変わったときだけ描き直す。
//! 別スレッド（入力係・出力係・パネルの係）からの知らせは [`AppEvent`] で受け取る。

use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::Hide;
use crossterm::execute;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::DefaultTerminal;
use ratatui::layout::{Rect, Size};
use ratatui::style::Modifier;
use tui_term::widget::{Cursor, PseudoTerminal};

use crate::input::spawn_input_thread;
use crate::layout::{PanelLayout, pty_size_for, split};
use crate::panel::FilePanel;
use crate::selection::{self, AutoScroll, SharedSelection};
use crate::shell::{Parser, Relay, SCROLLBACK_LINES, read_input, resize, spawn_output_thread};
use crate::{AppEvent, Options, write_raw};

/// ウィンドウの大きさが変わってから、シェル側の大きさを合わせるまで待つ時間。
///
/// ドラッグ中に 1 列変わるたびに PTY を変えると、そのたびにシェルがプロンプトを描き直してちらつく。
/// 大きさが変わらないままこの時間がたったら（ドラッグが止まったら）、まとめて 1 回だけ変える。
const RESIZE_SETTLE: Duration = Duration::from_millis(100);

/// 選択をドラッグしたまま端に来ているとき、スクロールする間隔。
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(50);

/// 自動スクロールで 1 回に動かす行数の、最初と最大。
///
/// 端で止めている時間が長いほど速くする。少しだけ選びたいときは細かく合わせられ、
/// ずっと上まで行きたいときは待っていれば速くなる。
const AUTO_SCROLL_LINES: (usize, usize) = (2, 10);

/// 自動スクロールで 1 回に動かす行数を 1 増やすまでの時間。
const AUTO_SCROLL_SPEEDUP: Duration = Duration::from_millis(250);

/// 知らせが来なくてもメインループを回す間隔。ウィンドウの大きさの変化や自動スクロールを拾うため。
const IDLE_WAKE: Duration = Duration::from_millis(50);

/// シェルを起動し、終了するまで画面を描き続ける。
pub fn run(terminal: &mut DefaultTerminal, options: &Options) -> Result<()> {
    let mut app = App::start(terminal.size()?, options)?;
    loop {
        app.fit_to(terminal.size()?)?;
        app.update_panel();
        app.step_auto_scroll();
        if app.needs_draw {
            app.needs_draw = false;
            app.draw(terminal)?;
        }
        match app.wait_for_event()? {
            Wake::Exited => break,
            Wake::Changed => app.needs_draw = true,
            Wake::Idle => {}
        }
    }
    app.child.wait()?;
    Ok(())
}

/// メインループが持っている状態。
struct App {
    /// 中で動いているシェル。
    child: Box<dyn Child + Send + Sync>,
    /// シェルのプロセス番号。前面のプログラムがシェルかどうかの判定に使う。
    shell_pid: Option<u32>,
    /// PTY の親側。大きさを変えたり、前面のプログラムを調べたりする。
    master: Box<dyn MasterPty + Send>,
    /// 今の PTY の大きさ。
    pty_size: PtySize,
    /// シェルの出力を解釈して、今の画面をメモリ上に持つ仮想端末。
    parser: Arc<Mutex<Parser>>,
    /// シェル側の文字選択。入力係（マウス）が書き換え、描画で反転表示する。
    selection: SharedSelection,
    /// 右側のファイル一覧。
    panel: FilePanel,
    /// パネルを出すかどうかと、その幅。
    panel_layout: PanelLayout,
    /// 別スレッドからの知らせ。
    rx: Receiver<AppEvent>,
    /// 描き直しが必要か。
    ///
    /// 何も変わっていないときは描き直さない。描き直すたびに ratatui がカーソルを表示・移動し直すので、
    /// 毎回描くとカーソルの点滅がリセットされ続けてチラつく。
    needs_draw: bool,
    /// 最後に描いたときの `(画面の幅, 高さ, パネルを出しているか, パネルの幅)`。
    last_layout: Option<(u16, u16, bool, u16)>,
    /// 合わせたいシェル側の大きさと、その大きさになった時刻（[`RESIZE_SETTLE`] だけ待ってから合わせる）。
    pending_resize: Option<(PtySize, Instant)>,
    /// 選択をドラッグしたまま端に来ているときの自動スクロール。
    auto_scroll: AutoScrolling,
    /// シェル連携から届いた入力中の内容 `(行, カーソル直前の単語)`。一度でも届いたら、画面から読む代わりにこれを使う。
    reported_input: Option<(String, String)>,
}

impl App {
    /// 左ペインの大きさで PTY を作ってシェルを起動し、入力係・出力係・パネルの係を立てる。
    fn start(size: Size, options: &Options) -> Result<Self> {
        let panel_layout = PanelLayout::new(size.width, options.panel_width);
        let (left, _) = split(
            Rect::new(0, 0, size.width, size.height),
            panel_layout.is_visible(),
            panel_layout.panel_width(),
        );
        let pty_size = pty_size_for(left);

        let pair = native_pty_system().openpty(pty_size)?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "zsh".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(std::env::current_dir()?);
        // sidels の中で動いていることをシェルに知らせる。
        // .zshrc から sidels を自動起動するときに、中のシェルでまた起動して入れ子になるのを防ぐのに使える
        cmd.env("SIDELS", "1");
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let master = pair.master;

        let (tx, rx) = mpsc::channel();

        // パネルはディレクトリを別スレッドで読む。読み終わったら描き直しを頼む
        let panel_tx = tx.clone();
        let shell_pid = child.process_id();
        let panel = FilePanel::new(
            shell_pid,
            std::env::current_dir()?,
            options.icons,
            move || {
                let _ = panel_tx.send(AppEvent::PanelLoaded);
            },
        );

        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            pty_size.rows,
            pty_size.cols,
            SCROLLBACK_LINES,
            Relay::new(tx.clone()),
        )));
        let selection = SharedSelection::default();

        spawn_output_thread(master.try_clone_reader()?, Arc::clone(&parser), tx.clone());
        spawn_input_thread(
            master.take_writer()?,
            Arc::clone(&parser),
            Arc::clone(&selection),
            tx,
        );

        Ok(Self {
            child,
            shell_pid,
            master,
            pty_size,
            parser,
            selection,
            panel,
            panel_layout,
            rx,
            needs_draw: true,
            last_layout: None,
            pending_resize: None,
            auto_scroll: AutoScrolling::default(),
            reported_input: None,
        })
    }

    /// 画面の大きさに、パネルの出し入れと PTY・仮想端末の大きさを合わせる。
    fn fit_to(&mut self, size: Size) -> Result<()> {
        self.panel_layout.set_width(size.width);
        let panel_visible = self.panel_layout.is_visible();
        let panel_width = self.panel_layout.panel_width();
        let layout = (size.width, size.height, panel_visible, panel_width);
        // F12 での出し入れは、待たずにすぐ合わせる（キーを押したのに反応が遅れると、もたついて感じる）。
        // 区切り線のドラッグは、ウィンドウの大きさを変えるときと同じく、手が止まってから合わせる
        let toggled = self.last_layout.map(|(_, _, visible, _)| visible) != Some(panel_visible);
        if self.last_layout != Some(layout) {
            self.last_layout = Some(layout);
            self.needs_draw = true;
        }
        let (left, _) = split(
            Rect::new(0, 0, size.width, size.height),
            panel_visible,
            panel_width,
        );
        let new_size = pty_size_for(left);
        let same = |a: PtySize, b: PtySize| (a.rows, a.cols) == (b.rows, b.cols);
        if same(new_size, self.pty_size) {
            self.pending_resize = None;
            return Ok(());
        }
        let settled = match self.pending_resize {
            Some((size, since)) if same(size, new_size) => since.elapsed() >= RESIZE_SETTLE,
            _ => {
                self.pending_resize = Some((new_size, Instant::now()));
                false
            }
        };
        if settled || toggled {
            resize(self.master.as_ref(), &self.parser, new_size)?;
            self.pty_size = new_size;
            self.pending_resize = None;
            self.needs_draw = true;
            // 画面の大きさが変わると選択範囲の位置がずれるので外す
            *self.selection.lock().unwrap() = None;
        }
        Ok(())
    }

    /// 前面のプログラムの今いる場所と、入力中のコマンドラインをパネルに伝える。
    fn update_panel(&mut self) {
        // 隠している間はディレクトリを読みに行かない。
        // 履歴をさかのぼっている間は画面に入力中の行が映っていないので、パネルは直前のまま
        let scrolled = self.parser.lock().unwrap().screen().scrollback() > 0;
        if !self.panel_layout.is_visible() || scrolled {
            return;
        }
        // 今キー入力を受け取っているプログラム（前面のプロセスグループ）。普段はシェル
        let foreground = self
            .master
            .process_group_leader()
            .and_then(|pid| u32::try_from(pid).ok());
        // シェル以外（Claude Code など）が前面のときは、打っている文字はコマンドではないので反応しない
        let (line, word) = if foreground.is_none() || foreground == self.shell_pid {
            // 画面から読むと、zsh-autosuggestions の灰色の候補なども打った文字として読んでしまう
            self.reported_input
                .clone()
                .unwrap_or_else(|| read_input(&self.parser.lock().unwrap()))
        } else {
            (String::new(), String::new())
        };
        self.needs_draw |= self.panel.update(foreground, &line, &word);
    }

    /// 選択をドラッグしたまま端に来ているなら、スクロールして選択を広げる。
    fn step_auto_scroll(&mut self) {
        if let Some((edge, lines)) = self.auto_scroll.due() {
            self.needs_draw |= scroll_selection(&self.parser, &self.selection, edge, lines);
        }
    }

    /// シェル側とパネルを描く。
    fn draw(&self, terminal: &mut DefaultTerminal) -> Result<()> {
        let panel_visible = self.panel_layout.is_visible();
        let panel_width = self.panel_layout.panel_width();
        // 入力係とのデッドロックを避けるため、選択範囲は仮想端末より先に読み取っておく
        let selected = *self.selection.lock().unwrap();
        // 書き換えの途中でカーソルが動き回るのを見せないよう、描いている間は隠す（描き終わりに ratatui が戻す）
        execute!(io::stdout(), Hide)?;
        terminal.draw(|frame| {
            // draw は直前に画面を測り直すので、size() の後にウィンドウが縮んでいることがある。
            // はみ出して書くとパニックするので、範囲はここで改めて計算する
            let (left, right) = split(frame.area(), panel_visible, panel_width);
            let parser = self.parser.lock().unwrap();
            let screen = parser.screen();

            // 左: シェルの画面。カーソルはtui-termに描かせず、本物のカーソルを動かす
            let shell_view =
                PseudoTerminal::new(screen).cursor(Cursor::default().visibility(false));
            frame.render_widget(shell_view, left);
            // 選択範囲は反転表示する（もともと反転している文字は元に戻して見せる）
            if let Some(selected) = selected {
                let buf = frame.buffer_mut();
                for row in 0..left.height {
                    for col in 0..left.width {
                        if selected.contains(selection::history_row(screen, row), col) {
                            buf[(left.x + col, left.y + row)]
                                .modifier
                                .toggle(Modifier::REVERSED);
                        }
                    }
                }
            }
            // 履歴をさかのぼっている間は、カーソルの位置に意味がないので隠す
            if !screen.hide_cursor() && screen.scrollback() == 0 {
                let (row, col) = screen.cursor_position();
                frame.set_cursor_position((left.x + col, left.y + row));
            }

            if panel_visible {
                self.panel.render(frame, right);
            }
        })?;
        Ok(())
    }

    /// 次の知らせを待って処理する。
    ///
    /// 何も来なくても [`IDLE_WAKE`] ごとに戻り、ウィンドウサイズの変化を拾えるようにしている。
    fn wait_for_event(&mut self) -> Result<Wake> {
        let first = match self.rx.recv_timeout(IDLE_WAKE) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => return Ok(Wake::Idle),
            Err(RecvTimeoutError::Disconnected) => return Ok(Wake::Exited),
        };
        // 出力が立て続けに届いたときは、まとめて1回だけ描き直す
        let mut changed = false;
        for event in std::iter::once(first).chain(self.rx.try_iter()) {
            match event {
                AppEvent::Output => changed = true,
                AppEvent::TogglePanel => {
                    self.panel_layout.toggle();
                    changed = true;
                }
                // 読み込みが終わっただけでは描き直さない。中身が変わったかは panel.update が判断する
                AppEvent::PanelLoaded => {}
                AppEvent::ScrollPanel(lines) => changed |= self.panel.scroll(lines),
                AppEvent::DragDivider(col) => changed |= self.panel_layout.drag_divider(col),
                AppEvent::AutoScroll(edge) => self.auto_scroll.edge = edge,
                // パネルに反映するかは panel.update が判断する
                AppEvent::ShellInput { line, word } => self.reported_input = Some((line, word)),
                AppEvent::Copy(text) if selection::uses_pbcopy() => {
                    selection::pbcopy(text.into_bytes())
                }
                AppEvent::Copy(text) => write_raw(&selection::osc52(&text))?,
                AppEvent::PassThrough(bytes) => write_raw(&bytes)?,
                AppEvent::Exited => return Ok(Wake::Exited),
            }
        }
        Ok(if changed { Wake::Changed } else { Wake::Idle })
    }
}

/// [`App::wait_for_event`] の結果。
enum Wake {
    /// シェルが終了した。
    Exited,
    /// 描き直しが必要な知らせが届いた。
    Changed,
    /// 描き直しが必要な知らせは届かなかった。
    ///
    /// パネルの読み込み完了だけが届いたときも含む（描き直すかは `panel.update` が決める）。
    Idle,
}

/// 選択をドラッグしたまま端に来ているときの自動スクロールの状態。
struct AutoScrolling {
    /// 今いる端（入力係が知らせる）。端にいなければ `None`。
    edge: Option<AutoScroll>,
    /// 端に来た時刻。長くいるほど速くする。
    since: Option<Instant>,
    /// 最後にスクロールした時刻。
    last: Instant,
}

impl Default for AutoScrolling {
    fn default() -> Self {
        Self {
            edge: None,
            since: None,
            last: Instant::now(),
        }
    }
}

impl AutoScrolling {
    /// スクロールする時間になっていれば、端と動かす行数を返す。
    fn due(&mut self) -> Option<(AutoScroll, usize)> {
        // 端に来たまま列だけ動いたときは、速さを最初に戻さない
        self.since = self.edge.and(self.since.or_else(|| Some(Instant::now())));
        let (edge, since) = (self.edge?, self.since?);
        if self.last.elapsed() < AUTO_SCROLL_INTERVAL {
            return None;
        }
        self.last = Instant::now();
        let (first, max) = AUTO_SCROLL_LINES;
        let speedup = since.elapsed().as_millis() / AUTO_SCROLL_SPEEDUP.as_millis();
        let lines = (first + usize::try_from(speedup).unwrap_or(max)).min(max);
        Some((edge, lines))
    }
}

/// 自動スクロールで `lines` 行動かし、選択の終わりを端の行の `edge.col` まで広げる。
///
/// 動いたら `true`（履歴の一番上や今の画面に着いたら、もう動かない）。
fn scroll_selection(
    parser: &Mutex<Parser>,
    selection: &SharedSelection,
    edge: AutoScroll,
    lines: usize,
) -> bool {
    // 2 つのロックを同時に握らないよう、仮想端末を先に済ませる
    let head = {
        let mut parser = parser.lock().unwrap();
        let screen = parser.screen_mut();
        let before = screen.scrollback();
        let after = if edge.up {
            before + lines
        } else {
            before.saturating_sub(lines)
        };
        screen.set_scrollback(after);
        if screen.scrollback() == before {
            return false;
        }
        let row = if edge.up {
            0
        } else {
            screen.size().0.saturating_sub(1)
        };
        (selection::history_row(screen, row), edge.col)
    };
    if let Some(selected) = selection.lock().unwrap().as_mut() {
        selected.extend(head);
    }
    true
}
