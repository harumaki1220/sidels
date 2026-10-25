//! Git の状態。
//!
//! パネルに表示しているディレクトリの各ファイルに、変更がある・新しい・`git add` 済み、などの印を付ける。
//! 今のブランチと、リモートとのコミットの差も調べる。

use std::collections::{HashMap, HashSet};
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

/// ディレクトリの Git の状態。
#[derive(Clone, PartialEq, Eq)]
pub struct GitStatus {
    /// 今のブランチ。
    pub branch: Branch,
    /// ファイル名ごとの印。サブディレクトリは、中に変更があればその名前に印が付く。
    pub marks: HashMap<String, GitMark>,
    /// 直下にあるもののうち、Git で管理している（最後のコミットに入っている）ものの名前。
    pub tracked: HashSet<String>,
}

/// 今のブランチと、リモート（upstream）とのコミットの差。
#[derive(Clone, PartialEq, Eq)]
pub struct Branch {
    /// ブランチ名。ブランチに入っていない（detached HEAD）なら `None`。
    pub name: Option<String>,
    /// まだ push していないコミットの数。
    pub ahead: u32,
    /// リモートにあって、まだ取り込んでいないコミットの数（最後に fetch した時点）。
    pub behind: u32,
}

/// `dir` の Git の状態。`dir` が Git の管理下でなければ `None`。
pub fn status(dir: &Path) -> Option<GitStatus> {
    // リポジトリの一番上から見た dir の位置（"src/" など）。status のパスはここからの相対になる
    let prefix = git(dir, &["rev-parse", "--show-prefix"])?;
    // -b を付けると、最初の 1 件がブランチの情報（`## main...origin/main [ahead 1]`）になる
    let output = git(dir, &["status", "--porcelain=v1", "-b", "-z", "--", "."])?;
    let (header, rest) = output.split_once('\0').unwrap_or((&output, ""));
    // 直下で管理しているものの名前。まだコミットがなければ HEAD がないので空にする
    let tracked = git(dir, &["ls-tree", "--name-only", "-z", "HEAD"])
        .map(|names| {
            names
                .split('\0')
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Some(GitStatus {
        branch: parse_branch(header.strip_prefix("## ").unwrap_or_default()),
        marks: parse(rest, prefix.trim_end_matches('\n')),
        tracked,
    })
}

/// `git status -b` の 1 件目（`## ` の後ろ）から、ブランチ名とコミットの差を読む。
///
/// 形は `main...origin/main [ahead 2, behind 1]`、upstream がなければ `main`、
/// まだコミットがなければ `No commits yet on main`、detached HEAD なら `HEAD (no branch)`。
fn parse_branch(header: &str) -> Branch {
    let (names, counts) = header.split_once(" [").unwrap_or((header, ""));
    let name = if names == "HEAD (no branch)" {
        None
    } else {
        let names = names.strip_prefix("No commits yet on ").unwrap_or(names);
        let name = names.split_once("...").map_or(names, |(local, _)| local);
        Some(name.to_string())
    };
    let count = |label: &str| {
        counts
            .trim_end_matches(']')
            .split(", ")
            .find_map(|part| part.strip_prefix(label)?.trim().parse().ok())
            .unwrap_or(0)
    };
    Branch {
        name,
        ahead: count("ahead"),
        behind: count("behind"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(header: &str) -> (Option<String>, u32, u32) {
        let branch = parse_branch(header);
        (branch.name, branch.ahead, branch.behind)
    }

    /// 印を `(名前, 文字)` にして名前順に並べる。
    fn letters(marks: &HashMap<String, GitMark>) -> Vec<(&str, char)> {
        let mut letters: Vec<_> = marks
            .iter()
            .map(|(name, mark)| (name.as_str(), mark.letter))
            .collect();
        letters.sort();
        letters
    }

    #[test]
    fn branch_with_upstream_and_counts() {
        assert_eq!(
            branch("main...origin/main [ahead 2, behind 1]"),
            (Some("main".into()), 2, 1)
        );
        assert_eq!(
            branch("feat/x...origin/feat/x [behind 3]"),
            (Some("feat/x".into()), 0, 3)
        );
    }

    #[test]
    fn branch_without_upstream_or_commits() {
        assert_eq!(branch("main"), (Some("main".into()), 0, 0));
        assert_eq!(
            branch("No commits yet on main"),
            (Some("main".into()), 0, 0)
        );
    }

    #[test]
    fn detached_head_has_no_branch_name() {
        assert_eq!(branch("HEAD (no branch)"), (None, 0, 0));
    }

    #[test]
    fn classify_follows_git_status_short() {
        let mark = |x, y| {
            let mark = classify(x, y);
            (mark.kind, mark.letter)
        };
        assert!(mark('M', ' ') == (GitKind::Staged, 'M'));
        assert!(mark(' ', 'M') == (GitKind::Modified, 'M'));
        // add した後にまた変えたものは、まだ add していない方を見せる
        assert!(mark('M', 'M') == (GitKind::Modified, 'M'));
        assert!(mark(' ', 'D') == (GitKind::Modified, 'D'));
        assert!(mark('?', '?') == (GitKind::Untracked, '?'));
        assert!(mark('U', 'U') == (GitKind::Conflict, 'U'));
        assert!(mark('A', 'A') == (GitKind::Conflict, 'U'));
    }

    #[test]
    fn parse_groups_by_name_under_prefix() {
        let output = " M src/main.rs\0?? notes.txt\0R  new.rs\0old.rs\0";
        let marks = parse(output, "");
        assert_eq!(
            letters(&marks),
            [("new.rs", 'R'), ("notes.txt", '?'), ("src", 'M')]
        );
        // src/ を見ているときは、src の中の名前に印が付き、外のものは出ない
        let marks = parse(output, "src/");
        assert_eq!(letters(&marks), [("main.rs", 'M')]);
    }

    #[test]
    fn directory_gets_the_strongest_mark_inside() {
        let marks = parse("A  src/a.rs\0 M src/b.rs\0?? src/c.rs\0", "");
        assert!(marks["src"].kind == GitKind::Modified);
    }
}
