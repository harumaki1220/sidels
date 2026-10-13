use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use globset::{GlobBuilder, GlobMatcher};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem};

/// 同じディレクトリでもこの間隔ごとに読み直す（ファイルの追加・削除を拾うため）
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub struct FilePanel {
    /// シェルのプロセス番号。今いるディレクトリを調べるのに使う
    shell_pid: Option<u32>,
    /// シェルが今いるディレクトリ
    cwd: PathBuf,
    /// パネルに表示しているディレクトリ。打ちかけのパスがあればその先、なければ cwd
    dir: PathBuf,
    entries: Vec<Entry>,
    /// 読み込みに失敗したときのメッセージ
    error: Option<String>,
    last_refresh: Instant,
    /// 最後に受け取ったコマンドライン。変わったときだけ glob を作り直す
    command_line: String,
    globs: Vec<GlobMatcher>,
    /// 打ちかけの名前。これで始まるものだけを表示する
    filter: String,
}

struct Entry {
    name: String,
    is_dir: bool,
    /// コマンドライン中の glob にマッチしている
    highlighted: bool,
}

impl FilePanel {
    pub fn new(shell_pid: Option<u32>, start_dir: PathBuf) -> Self {
        let mut panel = Self {
            shell_pid,
            cwd: start_dir.clone(),
            dir: start_dir,
            entries: Vec::new(),
            error: None,
            last_refresh: Instant::now(),
            command_line: String::new(),
            globs: Vec::new(),
            filter: String::new(),
        };
        panel.reload();
        panel
    }

    /// シェルの現在地と入力中のコマンドラインから、表示するディレクトリ・絞り込み・ハイライトを決める。
    /// line はカーソル行全体、word はカーソル直前の打ちかけの単語
    pub fn update(&mut self, line: &str, word: &str) {
        if let Some(cwd) = self.shell_cwd() {
            self.cwd = cwd;
        }
        let (dir, filter) = self.target(word);
        self.filter = filter;

        let dir_changed = dir != self.dir;
        let line_changed = line != self.command_line;
        self.dir = dir;
        if dir_changed || line_changed {
            self.command_line = line.to_string();
            self.globs = parse_globs(line, &self.cwd, &self.dir);
        }

        if dir_changed || self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.reload();
        } else if line_changed {
            self.mark_matches();
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::LEFT)
            .title(format!(" {} ", self.title()));

        let items: Vec<ListItem> = match &self.error {
            Some(message) => vec![ListItem::new(Line::styled(
                message.clone(),
                Style::default().fg(Color::Red),
            ))],
            None => self.visible_entries().map(Entry::to_list_item).collect(),
        };

        frame.render_widget(List::new(items).block(block), area);
    }

    /// 打ちかけの単語が「ディレクトリ/名前の途中」なら、そのディレクトリと名前の途中を返す。
    /// そうでなければ cwd と単語そのもの。glob を打っているときは絞り込まない（ハイライトに任せる）
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

    /// 絞り込み後の一覧。何も残らないときは全部出す（コマンド名を打っている間に一覧が空にならないように）
    fn visible_entries(&self) -> impl Iterator<Item = &Entry> {
        let any_match = self
            .entries
            .iter()
            .any(|e| e.name.starts_with(&self.filter));
        self.entries
            .iter()
            .filter(move |e| !any_match || e.name.starts_with(&self.filter))
    }

    /// Linux では /proc/<pid>/cwd がそのプロセスが今いるディレクトリへのリンクになっている
    fn shell_cwd(&self) -> Option<PathBuf> {
        let pid = self.shell_pid?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    fn reload(&mut self) {
        self.last_refresh = Instant::now();
        match read_entries(&self.dir) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
                self.mark_matches();
            }
            Err(err) => {
                self.entries.clear();
                self.error = Some(format!("読み込めません: {err}"));
            }
        }
    }

    fn mark_matches(&mut self) {
        for entry in &mut self.entries {
            entry.highlighted = self
                .globs
                .iter()
                .any(|glob| glob_matches(glob, &entry.name));
        }
    }

    /// パネルの見出し。ディレクトリ名だけにし、シェルの現在地以外を先に見せているときは → を付ける
    fn title(&self) -> String {
        let name = match self.dir.file_name() {
            Some(name) => format!("{}/", name.to_string_lossy()),
            None => self.dir.display().to_string(), // ルート "/" のとき
        };
        if self.dir == self.cwd {
            name
        } else {
            format!("→ {name}")
        }
    }
}

impl Entry {
    fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    fn to_list_item(&self) -> ListItem<'_> {
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
        ListItem::new(Line::styled(text, style))
    }
}

/// ディレクトリの中身を読む。並び順は ディレクトリ → ファイル、その中で名前順（大文字小文字を区別しない）
fn read_entries(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            // シンボリックリンクの先がディレクトリなら、ディレクトリとして扱う
            is_dir: entry.path().is_dir(),
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

fn is_glob(word: &str) -> bool {
    word.contains(['*', '?', '['])
}

/// "src/" や "~/foo/" のような打ちかけのディレクトリ部分を、実在するディレクトリのパスにする
fn resolve_dir(cwd: &Path, dir: &str) -> Option<PathBuf> {
    let path = match dir.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => cwd.join(dir), // dir が / で始まるなら join はそちらを使う
    };
    path.is_dir().then_some(path)
}

/// コマンドラインから、表示中のディレクトリ dir を対象とする glob を取り出す。
/// "*.log" は cwd、"src/*.rs" は cwd/src を対象とする。
/// 引用符やバックスラッシュ付きのものはシェルが展開しないので除く
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

/// zsh と同じく、隠しファイルはパターンが . で始まるときだけマッチさせる
fn glob_matches(glob: &GlobMatcher, name: &str) -> bool {
    if name.starts_with('.') && !glob.glob().glob().starts_with('.') {
        return false;
    }
    glob.is_match(name)
}
