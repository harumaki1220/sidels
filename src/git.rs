//! Git の状態。
//!
//! パネルに表示しているディレクトリの各ファイルに、変更がある・新しい・`git add` 済み、などの印を付ける。

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

/// 印の種類。
///
/// ディレクトリの中に複数の変更があるときは、大きい方（後ろに書いた方）をディレクトリの印にする。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitKind {
    /// `git add` 済みの変更。
    Staged,
    /// Git がまだ知らない新しいファイル。
    Untracked,
    /// `git add` していない変更。
    Modified,
    /// マージの衝突。
    Conflict,
}

/// 1 つのファイルまたはディレクトリに付ける印。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GitMark {
    /// 印の種類（色を決める）。
    pub kind: GitKind,
    /// 表示する 1 文字（`M` `A` `D` `?` など、`git status --short` と同じ）。
    pub letter: char,
}

/// `dir` の中のファイル名ごとの印。
///
/// `dir` が Git の管理下でなければ `None`。サブディレクトリは、中に変更があればその名前に印が付く。
pub fn status(dir: &Path) -> Option<HashMap<String, GitMark>> {
    // リポジトリの一番上から見た dir の位置（"src/" など）。status のパスはここからの相対になる
    let prefix = git(dir, &["rev-parse", "--show-prefix"])?;
    let output = git(dir, &["status", "--porcelain=v1", "-z", "--", "."])?;
    Some(parse(&output, prefix.trim_end_matches('\n')))
}

/// `dir` で git コマンドを実行し、成功したら標準出力を返す。
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // 裏で動かす status がロック（index.lock）を取ると、ユーザーが同時に打った
        // git commit などが失敗することがあるので、ロックを取らせない（VS Code と同じ）
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git status --porcelain=v1 -z` の出力を、`prefix` の直下の名前ごとの印にまとめる。
///
/// 1 件は `XY パス` の形で、`X` が `git add` 済みの状態、`Y` がまだの状態。
fn parse(output: &str, prefix: &str) -> HashMap<String, GitMark> {
    let mut marks: HashMap<String, GitMark> = HashMap::new();
    let mut fields = output.split('\0');
    while let Some(field) = fields.next() {
        let bytes = field.as_bytes();
        if bytes.len() < 4 {
            continue;
        }
        let (x, y) = (char::from(bytes[0]), char::from(bytes[1]));
        // 名前の変更・コピーは、新しいパスの後ろに元のパスが続くので読み飛ばす
        if x == 'R' || x == 'C' {
            fields.next();
        }
        let Some(relative) = field[3..].strip_prefix(prefix) else {
            continue;
        };
        let name = relative.split('/').next().unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let mark = classify(x, y);
        marks
            .entry(name.to_string())
            .and_modify(|current| {
                if mark.kind > current.kind {
                    *current = mark;
                }
            })
            .or_insert(mark);
    }
    marks
}

/// `XY` の 2 文字から印を決める。
fn classify(x: char, y: char) -> GitMark {
    let (kind, letter) = match (x, y) {
        ('?', '?') => (GitKind::Untracked, '?'),
        ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => (GitKind::Conflict, 'U'),
        (_, y) if y != ' ' => (GitKind::Modified, y),
        (x, _) => (GitKind::Staged, x),
    };
    GitMark { kind, letter }
}
