use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem};

/// 同じディレクトリでもこの間隔ごとに読み直す（ファイルの追加・削除を拾うため）
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);

/// 右側のファイル一覧パネル
pub struct FilePanel {
    /// シェルのプロセス番号。今いるディレクトリを調べるのに使う
    shell_pid: Option<u32>,
    /// 今表示しているディレクトリ
    dir: PathBuf,
    entries: Vec<Entry>,
    /// 読み込みに失敗したときのメッセージ
    error: Option<String>,
    last_refresh: Instant,
}

struct Entry {
    name: String,
    is_dir: bool,
}

impl FilePanel {
    pub fn new(shell_pid: Option<u32>, start_dir: PathBuf) -> Self {
        let mut panel = Self {
            shell_pid,
            dir: start_dir,
            entries: Vec::new(),
            error: None,
            last_refresh: Instant::now(),
        };
        panel.reload();
        panel
    }

    /// シェルが cd していたら追従し、一定時間たっていたら読み直す
    pub fn update(&mut self) {
        if let Some(cwd) = self.shell_cwd()
            && cwd != self.dir
        {
            self.dir = cwd;
            self.reload();
            return;
        }
        if self.last_refresh.elapsed() >= REFRESH_INTERVAL {
            self.reload();
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
            None => self.entries.iter().map(Entry::to_list_item).collect(),
        };

        frame.render_widget(List::new(items).block(block), area);
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
            }
            Err(err) => {
                self.entries.clear();
                self.error = Some(format!("読み込めません: {err}"));
            }
        }
    }

    /// パネルの見出し。ディレクトリ名だけにする
    fn title(&self) -> String {
        match self.dir.file_name() {
            Some(name) => format!("{}/", name.to_string_lossy()),
            None => self.dir.display().to_string(), // ルート "/" のとき
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
        // 隠しファイルは薄く表示する
        let style = if self.is_hidden() {
            style.add_modifier(Modifier::DIM)
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
