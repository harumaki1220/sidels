//! 右側のファイル一覧パネル。
//!
//! シェルの現在地の中身を表示し、入力中のコマンドラインに合わせて
//! 移動先のプレビュー・絞り込み・glob ハイライトを行う。Git の管理下なら、変更のあるファイルに印を付ける。
//! 最近書き換えられたファイルには、名前の後ろに ● を付ける。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use globset::{GlobBuilder, GlobMatcher};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::git::{self, GitKind, GitMark, GitStatus};

/// 同じディレクトリを読み直す間隔。
///
/// ファイルの追加・削除を拾うため、場所が変わらなくても読み直す。
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// 別のディレクトリに切り替えるとき、読み込みを待つ時間。
///
/// 普通のディレクトリはこの間に読み終わるので、見出しの「…」がチラつかない。
/// /mnt/c のような遅い場所でも、シェルの描画が止まるのはこの時間まで。
const SWITCH_WAIT: Duration = Duration::from_millis(20);

/// 読み込み係から届く結果。
///
/// どのディレクトリを読んだかと、中身またはエラーメッセージ。
type Loaded = (PathBuf, Result<Vec<Entry>, String>);

/// この時間以内に書き換えられたファイルを「最近変わった」として ● を付ける。
///
/// Claude Code に頼んだ作業が終わってから確かめるくらいの間は残る長さにしている。
const RECENT_WINDOW: Duration = Duration::from_secs(5 * 60);

/// 更新時刻を調べないディレクトリの、ファイル数の上限。
///
/// 更新時刻はファイルごとに問い合わせる必要があり、数が多いと時間がかかるため。
const MTIME_MAX_ENTRIES: usize = 1000;

/// 更新時刻係から届く結果。
///
/// どのディレクトリを調べたかと、ファイル名ごとの更新時刻（調べなかったなら `None`）。
type MtimeLoaded = (PathBuf, Option<HashMap<String, SystemTime>>);

/// Git 係から届く結果。
///
/// どのディレクトリを調べたかと、その Git の状態（Git の管理下でなければ `None`）。
type GitLoaded = (PathBuf, Option<GitStatus>);

/// 右側のファイル一覧パネル。
pub struct FilePanel {
    /// シェルのプロセス番号。前面のプログラムの場所が分からないときに、代わりに場所を調べる。
    shell_pid: Option<u32>,
    /// 前面のプログラム（普段はシェル）が今いるディレクトリ。
    cwd: PathBuf,
    /// 表示したいディレクトリ。打ちかけのパスがあればその先、なければ `cwd`。
    wanted: PathBuf,
    /// `entries` を読んだディレクトリ。`wanted` の読み込みが終わるまでは前の場所のまま。
    dir: PathBuf,
    /// `dir` の中身。
    entries: Vec<Entry>,
    /// 読み込みに失敗したときのメッセージ。
    error: Option<String>,
    /// 最後に読み込み結果を受け取った時刻。
    last_refresh: Instant,
    /// 読み込み係への依頼。
    load_tx: Sender<PathBuf>,
    /// 読み込み係からの結果。
    load_rx: Receiver<Loaded>,
    /// 最後に依頼して、まだ結果が届いていないディレクトリ。
    pending: Option<PathBuf>,
    /// Git 係への依頼。
    git_tx: Sender<PathBuf>,
    /// Git 係からの結果。
    git_rx: Receiver<GitLoaded>,
    /// Git 係に依頼して、まだ結果が届いていない。
    ///
    /// 大きなリポジトリでは `git status` が遅いので、終わる前に次を頼まない。
    git_pending: bool,
    /// `dir` の Git の状態（ブランチと、ファイル名ごとの印）。Git の管理下でなければ `None`。
    git: Option<GitStatus>,
    /// 更新時刻係への依頼。
    mtime_tx: Sender<PathBuf>,
    /// 更新時刻係からの結果。
    mtime_rx: Receiver<MtimeLoaded>,
    /// 更新時刻係に依頼して、まだ結果が届いていない。
    mtime_pending: bool,
    /// `dir` の中のファイル名ごとの更新時刻。調べていなければ `None`。
    mtimes: Option<HashMap<String, SystemTime>>,
    /// 最近（[`RECENT_WINDOW`] 以内に）書き換えられたファイルの名前。
    recent: HashSet<String>,
    /// 最後に受け取ったコマンドライン。変わったときだけ glob を作り直す。
    command_line: String,
    /// コマンドライン中の glob のうち、`dir` を対象とするもの。
    globs: Vec<GlobMatcher>,
    /// 打ちかけの名前。これで始まるものだけを表示する。
    filter: String,
}

/// パネルの 1 行（ファイルまたはディレクトリ）。
struct Entry {
    /// ファイル名。
    name: String,
    /// ディレクトリ（またはディレクトリへのシンボリックリンク）。
    is_dir: bool,
    /// コマンドライン中の glob にマッチしている。
    highlighted: bool,
    /// Git で消したファイル（もう実在しない）。一覧の最後に、取り消し線で出す。
    deleted: bool,
}

impl FilePanel {
    /// パネルを作り、読み込み係と Git 係のスレッドを立てる。
    ///
    /// `on_loaded` はどちらかが結果を届けるたびに呼ばれる。
    /// メインループを起こして [`FilePanel::update`] を呼んでもらう合図に使う。
    pub fn new(
        shell_pid: Option<u32>,
        start_dir: PathBuf,
        on_loaded: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let on_loaded: Arc<dyn Fn() + Send + Sync> = Arc::new(on_loaded);
        let (load_tx, load_rx) = spawn_worker(
            |dir| read_entries(dir).map_err(|err| err.to_string()),
            Arc::clone(&on_loaded),
        );
        // git status は遅いことがあるので、一覧の読み込みとは別のスレッドで調べる。
        // 同じスレッドだと、git status が終わるまで別のディレクトリへの切り替えが待たされる
        let (git_tx, git_rx) = spawn_worker(git::status, Arc::clone(&on_loaded));
        // 更新時刻もファイルごとの問い合わせで遅いことがあるので、別のスレッドで調べる
        let (mtime_tx, mtime_rx) = spawn_worker(read_mtimes, on_loaded);
        Self {
            shell_pid,
            cwd: start_dir.clone(),
            wanted: start_dir,
            // まだ何も読んでいない。最初の update で読み込みを頼む
            dir: PathBuf::new(),
            entries: Vec::new(),
            error: None,
            last_refresh: Instant::now(),
            load_tx,
            load_rx,
            pending: None,
            git_tx,
            git_rx,
            git_pending: false,
            git: None,
            mtime_tx,
            mtime_rx,
            mtime_pending: false,
            mtimes: None,
            recent: HashSet::new(),
            command_line: String::new(),
            globs: Vec::new(),
            filter: String::new(),
        }
    }

    /// 前面のプログラムの現在地と入力中のコマンドラインから、表示するディレクトリ・絞り込み・ハイライトを決める。
    ///
    /// `foreground` はターミナルの前面で動いているプログラムのプロセス番号（普段はシェル）。
    /// シェルの中で起動した Claude Code が worktree に移ったときなども、パネルが追いかけられる。
    /// `line` はカーソル行全体、`word` はカーソル直前の打ちかけの単語。
    /// 表示が変わったら `true` を返す（変わっていなければ描き直さなくてよい）。
    pub fn update(&mut self, foreground: Option<u32>, line: &str, word: &str) -> bool {
        let title_before = self.title();
        let filter_before = self.filter.clone();
        let mut listing_changed = false;

        // 前面のプログラムが終わった直後などで場所を読めなければ、シェルの場所を使う
        if let Some(cwd) = process_cwd(foreground).or_else(|| process_cwd(self.shell_pid)) {
            self.cwd = cwd;
        }
        let (wanted, filter) = self.target(word);
        self.wanted = wanted;

        let mut reloaded = false;
        while let Ok(loaded) = self.load_rx.try_recv() {
            let (accepted, changed) = self.apply(loaded);
            reloaded |= accepted;
            listing_changed |= changed;
        }

        // 別の場所に切り替えるときと、同じ場所でも一定時間たったときは、読み込みを頼む
        let switching = self.wanted != self.dir;
        let stale = self.last_refresh.elapsed() >= REFRESH_INTERVAL;
        if (switching || stale) && self.pending.as_ref() != Some(&self.wanted) {
            self.pending = Some(self.wanted.clone());
            let _ = self.load_tx.send(self.wanted.clone());
            if switching && let Ok(loaded) = self.load_rx.recv_timeout(SWITCH_WAIT) {
                let (accepted, changed) = self.apply(loaded);
                reloaded |= accepted;
                listing_changed |= changed;
            }
        }

        while let Ok((dir, marks)) = self.git_rx.try_recv() {
            self.git_pending = false;
            if dir == self.dir && marks != self.git {
                self.git = marks;
                listing_changed = true;
            }
        }
        // 一覧を読み直したら、Git の状態も調べ直す（一覧を先に出し、印はあとから付ける）
        if reloaded && !self.git_pending {
            self.git_pending = true;
            let _ = self.git_tx.send(self.dir.clone());
        }

        while let Ok((dir, mtimes)) = self.mtime_rx.try_recv() {
            self.mtime_pending = false;
            if dir == self.dir {
                self.mtimes = mtimes;
            }
        }
        if reloaded && !self.mtime_pending {
            self.mtime_pending = true;
            let _ = self.mtime_tx.send(self.dir.clone());
        }
        // 最近かどうかは毎回判定し直す（何もしなくても、時間がたてば ● が消えるように）
        let recent = self.recent_names();
        if recent != self.recent {
            self.recent = recent;
            listing_changed = true;
        }

        // 絞り込みは wanted に対するものなので、まだ前の場所を表示している間はかけない
        self.filter = if self.wanted == self.dir {
            filter
        } else {
            String::new()
        };

        let line_changed = line != self.command_line;
        if line_changed || reloaded {
            self.command_line = line.to_string();
            self.globs = parse_globs(line, &self.cwd, &self.dir);
            self.mark_matches();
        }

        listing_changed
            || line_changed
            || self.filter != filter_before
            || self.title() != title_before
    }

    /// パネルを `area` に描く。
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        // 左の区切り線の 1 列を除いた幅に、見出しを収める
        let title = self.title_line(usize::from(area.width).saturating_sub(1));
        let block = Block::default().borders(Borders::LEFT).title(title);

        let items: Vec<ListItem> = match &self.error {
            Some(message) => vec![ListItem::new(Line::styled(
                message.clone(),
                Style::default().fg(Color::Red),
            ))],
            None => {
                let inner = block.inner(area);
                let rows = usize::from(inner.height);
                // Git の印の 2 列を除いた、名前に使える幅
                let mark_width = if self.git.is_some() { 2 } else { 0 };
                let name_width = usize::from(inner.width).saturating_sub(mark_width);
                let marks = self.git.as_ref().map(|git| &git.marks);
                let item = |entry: &&Entry| {
                    entry.to_list_item(marks, self.recent.contains(&entry.name), name_width)
                };
                let deleted = self.deleted_entries();
                let entries = self.visible_entries(&deleted);
                if entries.len() <= rows {
                    entries.iter().map(item).collect()
                } else {
                    // 入りきらないときは最後の 1 行を使って、隠れている件数を知らせる
                    let shown = rows.saturating_sub(1);
                    let mut items: Vec<ListItem> = entries[..shown].iter().map(item).collect();
                    items.push(overflow_item(&entries[shown..]));
                    items
                }
            }
        };

        frame.render_widget(List::new(items).block(block), area);
    }

    /// 打ちかけの単語から、表示するディレクトリと絞り込みに使う名前を決める。
    ///
    /// 単語が「ディレクトリ/名前の途中」なら、そのディレクトリと名前の途中を返す。
    /// そうでなければ `cwd` と単語そのもの。glob を打っているときは絞り込まない（ハイライトに任せる）。
    fn target(&self, word: &str) -> (PathBuf, String) {
        let (dir, name) = match word.rfind('/') {
            Some(i) => match resolve_dir(&self.cwd, &word[..=i]) {
                Some(dir) => (dir, &word[i + 1..]),
                // 存在しないディレクトリを打っている途中は、いつもの表示に戻す
                None => return (self.cwd.clone(), String::new()),
            },
            None => (self.cwd.clone(), word),
        };
        if is_glob(name) {
            (dir, String::new())
        } else {
            (dir, name.to_string())
        }
    }

    /// 絞り込み後の一覧。実在するファイルのあとに、消したファイル（`deleted`）を並べる。
    ///
    /// 何も残らないときは全部出す（コマンド名を打っている間に一覧が空にならないように）。
    fn visible_entries<'a>(&'a self, deleted: &'a [Entry]) -> Vec<&'a Entry> {
        let all = || self.entries.iter().chain(deleted);
        let any_match = all().any(|e| e.name.starts_with(&self.filter));
        all()
            .filter(|e| !any_match || e.name.starts_with(&self.filter))
            .collect()
    }

    /// 読み込み係からの結果を反映する。
    ///
    /// 返すのは `(今表示したい場所の結果だったか, 一覧の中身が変わったか)`。
    /// 1 秒ごとの読み直しで中身が同じなら描き直さなくてよいので、変わったかどうかを分けて返す。
    fn apply(&mut self, (dir, result): Loaded) -> (bool, bool) {
        if self.pending.as_ref() == Some(&dir) {
            self.pending = None;
        }
        // 入力が進んで、もう表示したくない場所の結果は捨てる
        if dir != self.wanted {
            return (false, false);
        }
        let (entries, error) = match result {
            Ok(entries) => (entries, None),
            Err(err) => (Vec::new(), Some(format!("読み込めません: {err}"))),
        };
        let changed =
            dir != self.dir || error != self.error || !same_listing(&entries, &self.entries);
        if dir != self.dir {
            // 前の場所の印が、新しい場所の同じ名前のファイルに付かないように消しておく
            self.git = None;
            self.mtimes = None;
        }
        self.dir = dir;
        self.last_refresh = Instant::now();
        self.entries = entries;
        self.error = error;
        (true, changed)
    }

    /// Git で消したファイル（`D` の印が付いていて、もう実在しないもの）を、名前順に並べたもの。
    ///
    /// 消したファイルはディレクトリに残っていないので、`read_dir` の一覧には出てこない。
    /// Git の印から拾って、消したことに気づけるようにする。
    fn deleted_entries(&self) -> Vec<Entry> {
        let Some(git) = &self.git else {
            return Vec::new();
        };
        let mut deleted: Vec<Entry> = git
            .marks
            .iter()
            .filter(|(name, mark)| {
                mark.letter == 'D' && !self.entries.iter().any(|entry| &entry.name == *name)
            })
            .map(|(name, _)| Entry {
                name: name.clone(),
                is_dir: false,
                highlighted: false,
                deleted: true,
            })
            .collect();
        deleted.sort_by(|a, b| a.name.cmp(&b.name));
        deleted
    }

    /// 最近（[`RECENT_WINDOW`] 以内に）書き換えられたファイルの名前。
    fn recent_names(&self) -> HashSet<String> {
        let Some(mtimes) = &self.mtimes else {
            return HashSet::new();
        };
        let now = SystemTime::now();
        mtimes
            .iter()
            .filter(|(_, mtime)| {
                now.duration_since(**mtime)
                    .is_ok_and(|age| age <= RECENT_WINDOW)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// 各ファイルが glob にマッチするかを付け直す。
    fn mark_matches(&mut self) {
        for entry in &mut self.entries {
            entry.highlighted = self
                .globs
                .iter()
                .any(|glob| glob_matches(glob, &entry.name));
        }
    }

    /// パネルの見出しの部品 `(前に付ける印, ディレクトリ名, 後ろに付ける印)`。
    ///
    /// ディレクトリ名だけにし、シェルの現在地以外を先に見せているときは前に → を付ける。
    /// 読み込みが終わるまでは前の場所の一覧のままなので、後ろに … を付けて読み込み中だと分かるようにする。
    fn title_parts(&self) -> (&'static str, String, &'static str) {
        let name = match self.wanted.file_name() {
            Some(name) => format!("{}/", name.to_string_lossy()),
            None => self.wanted.display().to_string(), // ルート "/" のとき
        };
        let prefix = if self.wanted == self.cwd { "" } else { "→ " };
        let suffix = if self.wanted == self.dir { "" } else { " …" };
        (prefix, name, suffix)
    }

    /// パネルの見出し（変わったかどうかの判定に使う）。
    fn title(&self) -> String {
        let (prefix, name, suffix) = self.title_parts();
        format!("{prefix}{name}{suffix}")
    }

    /// 見出しを `width` 列に収めて作る。Git の管理下なら後ろにブランチ（`main ↑2↓1`）を付ける。
    ///
    /// ディレクトリ名が長いとブランチがはみ出して見えなくなるので、収まらないときは
    /// ディレクトリ名とブランチ名で幅を分け合い、それぞれ真ん中を省略する。
    fn title_line(&self, width: usize) -> Line<'static> {
        let (prefix, name, suffix) = self.title_parts();
        // 前後の空白と、前後の印の分
        let fixed = 2 + prefix.width() + suffix.width();
        let Some(git) = &self.git else {
            let name = fit_name(&name, false, width.saturating_sub(fixed));
            return Line::from(format!(" {prefix}{name}{suffix} "));
        };

        let branch = &git.branch;
        let branch_name = branch
            .name
            .clone()
            .unwrap_or_else(|| "(detached)".to_string());
        let ahead = (branch.ahead > 0).then(|| format!(" ↑{}", branch.ahead));
        let behind = (branch.behind > 0).then(|| {
            let gap = if branch.ahead > 0 { "" } else { " " };
            format!("{gap}↓{}", branch.behind)
        });
        let counts_width =
            ahead.as_deref().map_or(0, str::width) + behind.as_deref().map_or(0, str::width);
        // ディレクトリ名とブランチ名の間の空白 1 列も除いた、2 つの名前に使える幅
        let room = width.saturating_sub(fixed + 1 + counts_width);
        let (name_width, branch_width) = share_width(name.width(), branch_name.width(), room);

        let mut spans = vec![
            Span::raw(format!(
                " {prefix}{}{suffix} ",
                fit_name(&name, false, name_width)
            )),
            Span::styled(
                fit_name(&branch_name, false, branch_width),
                Style::default().fg(Color::Magenta),
            ),
        ];
        if let Some(ahead) = ahead {
            spans.push(Span::styled(ahead, Style::default().fg(Color::Green)));
        }
        if let Some(behind) = behind {
            spans.push(Span::styled(behind, Style::default().fg(Color::Red)));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }
}

impl Entry {
    fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    /// パネルの 1 行にする。
    ///
    /// `git` は表示中のディレクトリの Git の印（Git の管理下でなければ `None`）。
    /// `recent` なら名前の後ろに ● を付ける（最近書き換えられた）。
    /// 名前は `name_width` 列に収まるよう、長ければ真ん中を省略する。
    fn to_list_item(
        &self,
        git: Option<&HashMap<String, GitMark>>,
        recent: bool,
        name_width: usize,
    ) -> ListItem<'static> {
        // ● の 2 列（空白と ●）の分だけ、名前を短くする
        let name_width = if recent {
            name_width.saturating_sub(2)
        } else {
            name_width
        };
        let text = fit_name(&self.name, self.is_dir, name_width);
        let style = if self.is_dir {
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let style = if self.is_hidden() {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        };
        let style = if self.highlighted {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        };
        // 消したファイルは、赤い取り消し線で「もうない」ことを示す
        let style = if self.deleted {
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::CROSSED_OUT)
        } else {
            style
        };
        let mut spans = Vec::new();
        // Git の管理下では、名前の位置を揃えるため、印がなくても 2 列空ける
        if let Some(marks) = git {
            spans.push(match marks.get(&self.name) {
                Some(mark) => Span::styled(
                    format!("{} ", mark.letter),
                    Style::default().fg(mark_color(mark.kind)),
                ),
                None => Span::raw("  "),
            });
        }
        spans.push(Span::styled(text, style));
        if recent {
            spans.push(Span::styled(" ●", Style::default().fg(Color::Cyan)));
        }
        ListItem::new(Line::from(spans))
    }
}

/// 名前を `max` 列に収める（ディレクトリには最後に `/` を付ける）。
///
/// 収まらないときは真ん中を「…」で省略し、先頭と拡張子を残す（`very-long-n…report.md`）。
/// 末尾を切るだけだと、何の種類のファイルか分からなくなるため。
/// 拡張子が短ければ、名前の後ろの方も少し（使える幅の 1/3 くらいまで）残す。拡張子が長くても、末尾は半分まで。
fn fit_name(name: &str, is_dir: bool, max: usize) -> String {
    let suffix = if is_dir { "/" } else { "" };
    if name.width() + suffix.len() <= max {
        return format!("{name}{suffix}");
    }
    // 「…」と最後の `/` を除いた、名前に使える幅
    let room = max.saturating_sub(1 + suffix.len());
    if room == 0 {
        return "…".chars().take(max).collect();
    }
    // 拡張子は最後の `.` から後ろ。先頭の `.` は隠しファイルの印なので拡張子ではない
    let ext_start = name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len());
    let tail_room = name[ext_start..]
        .width()
        .max(room / 3)
        // 拡張子がとても長くても、先頭が見えるように末尾は半分まで
        .min(room / 2);
    let tail = take_end(name, tail_room);
    let head = take_start(name, room - tail.width());
    format!("{head}…{tail}{suffix}")
}

/// 先頭から、幅が `max` 列を超えない分だけ取る。
fn take_start(s: &str, max: usize) -> &str {
    let mut width = 0;
    for (i, c) in s.char_indices() {
        width += c.width().unwrap_or(0);
        if width > max {
            return &s[..i];
        }
    }
    s
}

/// 末尾から、幅が `max` 列を超えない分だけ取る。
fn take_end(s: &str, max: usize) -> &str {
    let mut width = 0;
    for (i, c) in s.char_indices().rev() {
        width += c.width().unwrap_or(0);
        if width > max {
            return &s[i + c.len_utf8()..];
        }
    }
    s
}

/// パネルに入りきらなかった分を知らせる 1 行（「… 他 N 件」）。
///
/// 隠れた中に glob にマッチするものがあれば、その件数も目立たせて出す。
/// `rm *.log` で消えるファイルが見えないところにある、というのを見逃さないため。
fn overflow_item(hidden: &[&Entry]) -> ListItem<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans = vec![Span::styled(format!("… 他 {} 件", hidden.len()), dim)];
    let matched = hidden.iter().filter(|entry| entry.highlighted).count();
    if matched > 0 {
        spans.push(Span::styled(
            format!("（{matched} 件該当）"),
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    ListItem::new(Line::from(spans))
}

/// 幅 `a` と `b` の 2 つを、合わせて `room` 列に収めるように分ける。
///
/// 収まるならそのまま。収まらなければ、短い方は削らずに残りを長い方に回し、
/// どちらも半分より長ければ半分ずつにする。
fn share_width(a: usize, b: usize, room: usize) -> (usize, usize) {
    if a + b <= room {
        return (a, b);
    }
    let half = room / 2;
    if a <= half {
        (a, room - a)
    } else if b <= half {
        (room - b, b)
    } else {
        (room - half, half)
    }
}

/// Git の印の色。`git status` の色分けに合わせ、`git add` 済みは緑、まだの変更は黄色にする。
fn mark_color(kind: GitKind) -> Color {
    match kind {
        GitKind::Staged => Color::Green,
        GitKind::Untracked => Color::Red,
        GitKind::Modified => Color::Yellow,
        GitKind::Conflict => Color::Magenta,
    }
}

/// プロセスが今いるディレクトリ。
///
/// Linux では `/proc/<pid>/cwd` が、そのプロセスが今いるディレクトリへのリンクになっている。
fn process_cwd(pid: Option<u32>) -> Option<PathBuf> {
    let pid = pid?;
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// ディレクトリの中のファイル名ごとの更新時刻。
///
/// Windows 側のフォルダ（`/mnt/` の下）と、ファイルが [`MTIME_MAX_ENTRIES`] を超えるディレクトリでは調べない
/// （ファイルごとの問い合わせが遅く、/mnt/c の大きなフォルダでは数秒かかるため）。
/// `.git` は Git の操作のたびに書き換わって、いつも ● が付いてしまうので除く。
fn read_mtimes(dir: &Path) -> Option<HashMap<String, SystemTime>> {
    if dir.starts_with("/mnt") {
        return None;
    }
    let entries: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .collect();
    if entries.len() > MTIME_MAX_ENTRIES {
        return None;
    }
    let mtimes = entries
        .iter()
        .filter(|entry| entry.file_name() != ".git")
        .filter_map(|entry| {
            let mtime = entry.metadata().ok()?.modified().ok()?;
            Some((entry.file_name().to_string_lossy().into_owned(), mtime))
        })
        .collect();
    Some(mtimes)
}

/// 依頼されたディレクトリを裏で調べる係のスレッドを立てる。
///
/// /mnt/c のような遅い場所を調べても、メインループが止まらないようにするため。
/// 調べ終わるたびに `notify` を呼ぶ。
fn spawn_worker<R: Send + 'static>(
    work: fn(&Path) -> R,
    notify: Arc<dyn Fn() + Send + Sync>,
) -> (Sender<PathBuf>, Receiver<(PathBuf, R)>) {
    let (request_tx, request_rx) = mpsc::channel::<PathBuf>();
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(mut dir) = request_rx.recv() {
            // 調べている間に依頼が溜まっていたら、最新のものだけ調べる
            while let Ok(newer) = request_rx.try_recv() {
                dir = newer;
            }
            let result = work(&dir);
            if result_tx.send((dir, result)).is_err() {
                return;
            }
            notify();
        }
    });
    (request_tx, result_rx)
}

/// ディレクトリの中身を読む。
///
/// 並び順は ディレクトリ → ファイル、それぞれ隠しファイルは後ろ、その中で名前順（大文字小文字を区別しない）。
fn read_entries(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            // 種類は read_dir の結果から分かるので、ファイルごとに調べ直さない（/mnt/c では特に遅い）。
            // シンボリックリンクだけはリンク先を見て、ディレクトリならディレクトリとして扱う
            is_dir: match entry.file_type() {
                Ok(t) if t.is_symlink() => entry.path().is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => false,
            },
            highlighted: false,
            deleted: false,
        })
        .collect();

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.is_hidden().cmp(&b.is_hidden()))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// 名前と種類が同じ並びか（ハイライトの状態は比べない）。
fn same_listing(a: &[Entry], b: &[Entry]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.name == y.name && x.is_dir == y.is_dir)
}

/// シェルが glob として展開しそうな単語か。
fn is_glob(word: &str) -> bool {
    word.contains(['*', '?', '['])
}

/// `src/` や `~/foo/` のような打ちかけのディレクトリ部分を、実在するディレクトリのパスにする。
fn resolve_dir(cwd: &Path, dir: &str) -> Option<PathBuf> {
    let path = match dir.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => cwd.join(dir), // dir が / で始まるなら join はそちらを使う
    };
    path.is_dir().then_some(path)
}

/// コマンドラインから、表示中のディレクトリ `dir` を対象とする glob を取り出す。
///
/// `*.log` は `cwd`、`src/*.rs` は `cwd/src` を対象とする。
/// 引用符やバックスラッシュ付きのものはシェルが展開しないので除く。
fn parse_globs(line: &str, cwd: &Path, dir: &Path) -> Vec<GlobMatcher> {
    line.split_whitespace()
        .filter(|word| is_glob(word))
        .filter(|word| !word.contains(['\\', '\'', '"']))
        .filter_map(|word| {
            let (word_dir, pattern) = match word.rfind('/') {
                Some(i) => (resolve_dir(cwd, &word[..=i])?, &word[i + 1..]),
                None => (cwd.to_path_buf(), word),
            };
            (word_dir == dir).then_some(pattern)
        })
        .filter_map(|pattern| {
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .ok()
        })
        .map(|glob| glob.compile_matcher())
        .collect()
}

/// ファイル名が glob にマッチするか。
///
/// zsh と同じく、隠しファイルはパターンが `.` で始まるときだけマッチさせる。
fn glob_matches(glob: &GlobMatcher, name: &str) -> bool {
    if name.starts_with('.') && !glob.glob().glob().starts_with('.') {
        return false;
    }
    glob.is_match(name)
}
