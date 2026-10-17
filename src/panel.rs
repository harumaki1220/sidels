//! 右側のファイル一覧パネル。
//!
//! シェルの現在地の中身を表示し、入力中のコマンドラインに合わせて
//! 移動先のプレビュー・絞り込み・glob ハイライトを行う。Git の管理下なら、変更のあるファイルに印を付ける。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobMatcher};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::git::{self, GitKind, GitMark};

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

/// Git 係から届く結果。
///
/// どのディレクトリを調べたかと、ファイル名ごとの印（Git の管理下でなければ `None`）。
type GitLoaded = (PathBuf, Option<HashMap<String, GitMark>>);

/// 右側のファイル一覧パネル。
pub struct FilePanel {
    /// シェルのプロセス番号。今いるディレクトリを調べるのに使う。
    shell_pid: Option<u32>,
    /// シェルが今いるディレクトリ。
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
    /// `dir` の中のファイル名ごとの Git の印。Git の管理下でなければ `None`。
    git: Option<HashMap<String, GitMark>>,
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
        let (git_tx, git_rx) = spawn_worker(git::status, on_loaded);
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
            command_line: String::new(),
            globs: Vec::new(),
            filter: String::new(),
        }
    }

    /// シェルの現在地と入力中のコマンドラインから、表示するディレクトリ・絞り込み・ハイライトを決める。
    ///
    /// `line` はカーソル行全体、`word` はカーソル直前の打ちかけの単語。
    /// 表示が変わったら `true` を返す（変わっていなければ描き直さなくてよい）。
    pub fn update(&mut self, line: &str, word: &str) -> bool {
        let title_before = self.title();
        let filter_before = self.filter.clone();
        let mut listing_changed = false;

        if let Some(cwd) = self.shell_cwd() {
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
        let block = Block::default()
            .borders(Borders::LEFT)
            .title(format!(" {} ", self.title()));

        let items: Vec<ListItem> = match &self.error {
            Some(message) => vec![ListItem::new(Line::styled(
                message.clone(),
                Style::default().fg(Color::Red),
            ))],
            None => self
                .visible_entries()
                .map(|entry| entry.to_list_item(self.git.as_ref()))
                .collect(),
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

    /// 絞り込み後の一覧。
    ///
    /// 何も残らないときは全部出す（コマンド名を打っている間に一覧が空にならないように）。
    fn visible_entries(&self) -> impl Iterator<Item = &Entry> {
        let any_match = self
            .entries
            .iter()
            .any(|e| e.name.starts_with(&self.filter));
        self.entries
            .iter()
            .filter(move |e| !any_match || e.name.starts_with(&self.filter))
    }

    /// シェルが今いるディレクトリ。
    ///
    /// Linux では `/proc/<pid>/cwd` が、そのプロセスが今いるディレクトリへのリンクになっている。
    fn shell_cwd(&self) -> Option<PathBuf> {
        let pid = self.shell_pid?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
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
        }
        self.dir = dir;
        self.last_refresh = Instant::now();
        self.entries = entries;
        self.error = error;
        (true, changed)
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

    /// パネルの見出し。
    ///
    /// ディレクトリ名だけにし、シェルの現在地以外を先に見せているときは → を付ける。
    /// 読み込みが終わるまでは前の場所の一覧のままなので、… を付けて読み込み中だと分かるようにする。
    fn title(&self) -> String {
        let name = match self.wanted.file_name() {
            Some(name) => format!("{}/", name.to_string_lossy()),
            None => self.wanted.display().to_string(), // ルート "/" のとき
        };
        let name = if self.wanted == self.cwd {
            name
        } else {
            format!("→ {name}")
        };
        if self.wanted == self.dir {
            name
        } else {
            format!("{name} …")
        }
    }
}

impl Entry {
    fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    /// パネルの 1 行にする。
    ///
    /// `git` は表示中のディレクトリの Git の印（Git の管理下でなければ `None`）。
    fn to_list_item(&self, git: Option<&HashMap<String, GitMark>>) -> ListItem<'_> {
        let (text, style) = if self.is_dir {
            (
                format!("{}/", self.name),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (self.name.clone(), Style::default())
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
        let name = Span::styled(text, style);
        let Some(marks) = git else {
            return ListItem::new(Line::from(name));
        };
        // Git の管理下では、名前の位置を揃えるため、印がなくても 2 列空ける
        let mark = match marks.get(&self.name) {
            Some(mark) => Span::styled(
                format!("{} ", mark.letter),
                Style::default().fg(mark_color(mark.kind)),
            ),
            None => Span::raw("  "),
        };
        ListItem::new(Line::from(vec![mark, name]))
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
