//! # tao-apply-patch
//!
//! apply-patch DSL:语法/语义分离(见 docs/design/tools.md §3)。
//!
//! v1 实现:
//! - **解析层** `parse`:DSL → `Vec<Hunk>`,纯文法,有错即拒。
//! - **寻址层** `apply`:L1 文本 fuzz(归一化空白后滑动窗口匹配);L2 tree-sitter
//!   AST 锚定留后续(避免重依赖;L1 对绝大多数编辑足够,失败即拒不猜测)。
//! - **执行层**:全部 hunk 在内存中应用成功后才写盘(寻址事务性;写盘用临时
//!   文件 + rename,单文件原子)。输出 similar 生成的 unified diff。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

// ---- 数据类型 ----

/// 一个补丁块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub action: HunkAction,
    pub path: PathBuf,
    /// Update:寻址上下文(`@@` 锚 + ` ` context 行),归一化匹配。
    pub seek_context: Vec<String>,
    /// Update:待移除行(`-`)。
    pub remove: Vec<String>,
    /// Update:待新增行(`+`);Add 时是文件全文。
    pub add: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkAction {
    Add,
    Update,
    Delete,
    Move { to: PathBuf },
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("期望 `*** Begin Patch` 作为首行")]
    ExpectedBegin,
    #[error("意外的文件结束")]
    UnexpectedEof,
    #[error("`@@` 锚出现在 hunk 之外")]
    AnchorOutsideHunk,
    #[error("`+`/`-`/context 行出现在 hunk 之外: {0:?}")]
    LineOutsideHunk(String),
    #[error("`*** Move File` 格式错误(应为 `*** Move File: <from> to <to>`): {0:?}")]
    BadMove(String),
    #[error("无法识别的行: {0:?}")]
    UnknownLine(String),
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("IO 错误({path}): {err}", path = .0.display(), err = .1)]
    Io(PathBuf, #[source] std::io::Error),
    #[error("文件不存在: {0}")]
    NotFound(PathBuf),
    #[error("文件已存在(Add 要求新文件): {0}")]
    AlreadyExists(PathBuf),
    #[error("Update hunk 为空(无 context/remove)")]
    EmptyHunk(PathBuf),
    #[error("寻址上下文在文件中多次匹配(歧义),请增加更多上下文: {0}")]
    Ambiguous(PathBuf),
    #[error("寻址上下文在文件中未找到: {0}")]
    NotFoundInFile(PathBuf),
}

// ---- 解析层 ----

/// 解析 patch 文本为 `Vec<Hunk>`。
pub fn parse(input: &str) -> Result<Vec<Hunk>, ParseError> {
    let mut lines = input.lines().peekable();
    let first = lines.next().ok_or(ParseError::UnexpectedEof)?;
    if first.trim() != "*** Begin Patch" {
        return Err(ParseError::ExpectedBegin);
    }

    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<HunkBuilder> = None;
    for line in lines {
        if line.trim() == "*** End Patch" {
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            push_current(&mut current, &mut hunks);
            current = Some(HunkBuilder::add(PathBuf::from(rest)));
        } else if let Some(rest) = line.strip_prefix("*** Update File: ") {
            push_current(&mut current, &mut hunks);
            current = Some(HunkBuilder::update(PathBuf::from(rest)));
        } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            push_current(&mut current, &mut hunks);
            hunks.push(Hunk {
                action: HunkAction::Delete,
                path: PathBuf::from(rest),
                seek_context: Vec::new(),
                remove: Vec::new(),
                add: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("*** Move File: ") {
            push_current(&mut current, &mut hunks);
            let (from, to) = rest
                .split_once(" to ")
                .ok_or_else(|| ParseError::BadMove(rest.into()))?;
            hunks.push(Hunk {
                action: HunkAction::Move {
                    to: PathBuf::from(to),
                },
                path: PathBuf::from(from),
                seek_context: Vec::new(),
                remove: Vec::new(),
                add: Vec::new(),
            });
        } else if let Some(anchor) = line.strip_prefix("@@ ") {
            let h = current.as_mut().ok_or(ParseError::AnchorOutsideHunk)?;
            h.seek_context.push(anchor.to_string());
        } else if let Some(content) = line.strip_prefix('+') {
            let h = current
                .as_mut()
                .ok_or_else(|| ParseError::LineOutsideHunk(line.to_string()))?;
            h.add.push(content.to_string());
        } else if let Some(content) = line.strip_prefix('-') {
            let h = current
                .as_mut()
                .ok_or_else(|| ParseError::LineOutsideHunk(line.to_string()))?;
            h.remove.push(content.to_string());
        } else if let Some(content) = line.strip_prefix(' ') {
            let h = current
                .as_mut()
                .ok_or_else(|| ParseError::LineOutsideHunk(line.to_string()))?;
            h.seek_context.push(content.to_string());
        } else if line.is_empty() {
            // hunk 内的裸空行:codex 要求前缀(+/-/ );裸空行忽略。
        } else {
            return Err(ParseError::UnknownLine(line.to_string()));
        }
    }
    push_current(&mut current, &mut hunks);
    Ok(hunks)
}

struct HunkBuilder {
    action: HunkAction,
    path: PathBuf,
    seek_context: Vec<String>,
    remove: Vec<String>,
    add: Vec<String>,
}

impl HunkBuilder {
    fn add(path: PathBuf) -> Self {
        Self {
            action: HunkAction::Add,
            path,
            seek_context: Vec::new(),
            remove: Vec::new(),
            add: Vec::new(),
        }
    }
    fn update(path: PathBuf) -> Self {
        Self {
            action: HunkAction::Update,
            path,
            seek_context: Vec::new(),
            remove: Vec::new(),
            add: Vec::new(),
        }
    }
    fn finish(self) -> Hunk {
        Hunk {
            action: self.action,
            path: self.path,
            seek_context: self.seek_context,
            remove: self.remove,
            add: self.add,
        }
    }
}

fn push_current(current: &mut Option<HunkBuilder>, hunks: &mut Vec<Hunk>) {
    if let Some(b) = current.take() {
        hunks.push(b.finish());
    }
}

// ---- 执行层 ----

/// 待写盘的操作(全部 hunk 在内存计算成功后才 commit)。
enum PendingOp {
    Write { path: PathBuf, new: String },
    Delete { path: PathBuf },
    Move { from: PathBuf, to: PathBuf },
}

/// 应用全部 hunk 到 `base` 目录。寻址全部成功后才写盘;返回 unified diff。
pub fn apply(hunks: &[Hunk], base: &Path) -> Result<String, ApplyError> {
    let mut ops: Vec<PendingOp> = Vec::with_capacity(hunks.len());
    let mut diff = String::new();

    for hunk in hunks {
        match hunk.action {
            HunkAction::Add => {
                let path = base.join(&hunk.path);
                if path.exists() {
                    return Err(ApplyError::AlreadyExists(path));
                }
                let new = join_lines(&hunk.add);
                diff.push_str(&format!("*** Add File: {}\n", hunk.path.display()));
                for l in &hunk.add {
                    diff.push('+');
                    diff.push_str(l);
                    diff.push('\n');
                }
                ops.push(PendingOp::Write { path, new });
            }
            HunkAction::Update => {
                let path = base.join(&hunk.path);
                let old = fs::read_to_string(&path).map_err(|e| ApplyError::Io(path.clone(), e))?;
                let new = apply_update(&path, &old, hunk)?;
                diff.push_str(&make_diff(&hunk.path, &old, &new));
                ops.push(PendingOp::Write { path, new });
            }
            HunkAction::Delete => {
                let path = base.join(&hunk.path);
                if !path.exists() {
                    return Err(ApplyError::NotFound(path));
                }
                diff.push_str(&format!("*** Delete File: {}\n", hunk.path.display()));
                ops.push(PendingOp::Delete { path });
            }
            HunkAction::Move { ref to } => {
                let from = base.join(&hunk.path);
                let to = base.join(to);
                if !from.exists() {
                    return Err(ApplyError::NotFound(from));
                }
                diff.push_str(&format!(
                    "*** Move File: {} to {}\n",
                    hunk.path.display(),
                    to.display()
                ));
                ops.push(PendingOp::Move { from, to });
            }
        }
    }

    // 全部 hunk 寻址成功 → commit 写盘(单文件临时文件 + rename 原子)
    for op in ops {
        commit_op(&op)?;
    }
    Ok(diff)
}

fn commit_op(op: &PendingOp) -> Result<(), ApplyError> {
    match op {
        PendingOp::Write { path, new } => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                fs::create_dir_all(parent).map_err(|e| ApplyError::Io(parent.to_path_buf(), e))?;
            }
            // 临时文件 + rename(原子)
            let tmp = path.with_extension("tao-tmp");
            let mut f = fs::File::create(&tmp).map_err(|e| ApplyError::Io(tmp.clone(), e))?;
            f.write_all(new.as_bytes())
                .map_err(|e| ApplyError::Io(tmp.clone(), e))?;
            f.sync_all().map_err(|e| ApplyError::Io(tmp.clone(), e))?;
            drop(f);
            fs::rename(&tmp, path).map_err(|e| ApplyError::Io(path.clone(), e))?;
        }
        PendingOp::Delete { path } => {
            fs::remove_file(path).map_err(|e| ApplyError::Io(path.clone(), e))?;
        }
        PendingOp::Move { from, to } => {
            if let Some(parent) = to.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                fs::create_dir_all(parent).map_err(|e| ApplyError::Io(parent.to_path_buf(), e))?;
            }
            fs::rename(from, to).map_err(|e| ApplyError::Io(from.clone(), e))?;
        }
    }
    Ok(())
}

/// L1 文本 fuzz:在 `old` 中定位 `seek_context + remove` 连续匹配(归一化空白),
/// 替换 remove 为 add。多匹配 ⇒ 歧义拒绝;无匹配 ⇒ 未找到拒绝。
fn apply_update(path: &Path, old: &str, hunk: &Hunk) -> Result<String, ApplyError> {
    let lines: Vec<&str> = old.lines().collect();
    let seek_n: Vec<String> = hunk.seek_context.iter().map(|s| normalize(s)).collect();
    let remove_n: Vec<String> = hunk.remove.iter().map(|s| normalize(s)).collect();

    // match_block = seek + remove(须在文件中连续出现)
    let match_block: Vec<String> = seek_n.iter().chain(remove_n.iter()).cloned().collect();
    if match_block.is_empty() {
        return Err(ApplyError::EmptyHunk(path.to_path_buf()));
    }

    let mut found: Option<usize> = None;
    for i in 0..=lines.len().saturating_sub(match_block.len()) {
        let ok = (0..match_block.len()).all(|j| normalize(lines[i + j]) == match_block[j]);
        if ok {
            if found.is_some() {
                return Err(ApplyError::Ambiguous(path.to_path_buf()));
            }
            found = Some(i);
        }
    }
    let start = found.ok_or_else(|| ApplyError::NotFoundInFile(path.to_path_buf()))?;
    let seek_len = seek_n.len();
    let block_len = match_block.len();

    // 重组:前部 + seek(原文件行保留) + add + 后部(跳过 remove)
    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len() + hunk.add.len());
    new_lines.extend(lines[..start].iter().map(|s| s.to_string()));
    new_lines.extend(lines[start..start + seek_len].iter().map(|s| s.to_string()));
    new_lines.extend(hunk.add.iter().cloned());
    new_lines.extend(lines[start + block_len..].iter().map(|s| s.to_string()));

    let mut result = new_lines.join("\n");
    if old.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// 归一化空白:trim 首尾 + 压缩内部连续空白为单空格。
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn join_lines(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    if !lines.is_empty() {
        s.push('\n');
    }
    s
}

/// 生成 unified diff(similar)。
fn make_diff(path: &Path, old: &str, new: &str) -> String {
    let p = path.display().to_string();
    similar::udiff::unified_diff(similar::Algorithm::Myers, old, new, 3, Some((&p, &p)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn hunk_update(path: &str, seek: &[&str], remove: &[&str], add: &[&str]) -> Hunk {
        Hunk {
            action: HunkAction::Update,
            path: PathBuf::from(path),
            seek_context: seek.iter().map(|s| s.to_string()).collect(),
            remove: remove.iter().map(|s| s.to_string()).collect(),
            add: add.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ---- 解析 ----

    #[test]
    fn parse_add_update_delete() {
        let input = "*** Begin Patch\n*** Add File: a.rs\n+fn main() {}\n*** Update File: b.rs\n@@ fn existing\n context\n-old\n+new\n*** Delete File: c.rs\n*** End Patch\n";
        let hunks = parse(input).unwrap();
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0].action, HunkAction::Add);
        assert_eq!(hunks[0].add, vec!["fn main() {}"]);
        assert_eq!(hunks[1].action, HunkAction::Update);
        assert_eq!(hunks[1].seek_context, vec!["fn existing", "context"]);
        assert_eq!(hunks[1].remove, vec!["old"]);
        assert_eq!(hunks[1].add, vec!["new"]);
        assert_eq!(hunks[2].action, HunkAction::Delete);
    }

    #[test]
    fn parse_move() {
        let input = "*** Begin Patch\n\
            *** Move File: a.rs to b.rs\n\
            *** End Patch\n";
        let hunks = parse(input).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].action,
            HunkAction::Move {
                to: PathBuf::from("b.rs")
            }
        );
    }

    #[test]
    fn parse_rejects_missing_begin() {
        assert!(parse("*** Add File: x\n").is_err());
    }

    // ---- apply:Update 寻址 ----

    #[test]
    fn apply_update_unique_match() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let hunk = hunk_update("f.rs", &[], &["fn b() {}"], &["fn B() {}"]);
        let diff = apply(&[hunk], dir.path()).unwrap();
        let result = fs::read_to_string(dir.path().join("f.rs")).unwrap();
        assert!(result.contains("fn B() {}"));
        assert!(!result.contains("fn b() {}"));
        assert!(diff.contains("--- f.rs"));
        assert!(diff.contains("-fn b() {}"));
        assert!(diff.contains("+fn B() {}"));
    }

    #[test]
    fn apply_update_whitespace_tolerant() {
        // 文件用 4 空格,hunk 用 2 空格 → 归一化后仍匹配
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "    fn a() {}\n").unwrap();
        let hunk = hunk_update("f.rs", &[], &["fn a() {}"], &["fn a() { return; }"]);
        apply(&[hunk], dir.path()).unwrap();
        let result = fs::read_to_string(dir.path().join("f.rs")).unwrap();
        assert!(result.contains("fn a() { return; }"));
    }

    #[test]
    fn apply_update_ambiguous_rejected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "x\nx\n").unwrap();
        // remove "x" 匹配两次 → 歧义
        let hunk = hunk_update("f.rs", &[], &["x"], &["y"]);
        let err = apply(&[hunk], dir.path()).unwrap_err();
        assert!(matches!(err, ApplyError::Ambiguous(_)));
        // 事务性:未写盘
        assert_eq!(
            fs::read_to_string(dir.path().join("f.rs")).unwrap(),
            "x\nx\n"
        );
    }

    #[test]
    fn apply_update_not_found_rejected() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "fn a() {}\n").unwrap();
        let hunk = hunk_update("f.rs", &[], &["nope"], &["y"]);
        let err = apply(&[hunk], dir.path()).unwrap_err();
        assert!(matches!(err, ApplyError::NotFoundInFile(_)));
    }

    // ---- apply:Add/Delete/Move ----

    #[test]
    fn apply_add_creates_file() {
        let dir = TempDir::new().unwrap();
        let hunk = Hunk {
            action: HunkAction::Add,
            path: PathBuf::from("new.rs"),
            seek_context: vec![],
            remove: vec![],
            add: vec!["fn main() {}".into()],
        };
        apply(&[hunk], dir.path()).unwrap();
        let result = fs::read_to_string(dir.path().join("new.rs")).unwrap();
        assert_eq!(result, "fn main() {}\n");
    }

    #[test]
    fn apply_add_rejects_existing() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("old.rs"), "x\n").unwrap();
        let hunk = Hunk {
            action: HunkAction::Add,
            path: PathBuf::from("old.rs"),
            seek_context: vec![],
            remove: vec![],
            add: vec!["y".into()],
        };
        let err = apply(&[hunk], dir.path()).unwrap_err();
        assert!(matches!(err, ApplyError::AlreadyExists(_)));
    }

    #[test]
    fn apply_delete_removes_file() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("gone.rs"), "x\n").unwrap();
        let hunk = Hunk {
            action: HunkAction::Delete,
            path: PathBuf::from("gone.rs"),
            seek_context: vec![],
            remove: vec![],
            add: vec![],
        };
        apply(&[hunk], dir.path()).unwrap();
        assert!(!dir.path().join("gone.rs").exists());
    }

    #[test]
    fn apply_move_renames() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "content\n").unwrap();
        let hunk = Hunk {
            action: HunkAction::Move {
                to: PathBuf::from("b.rs"),
            },
            path: PathBuf::from("a.rs"),
            seek_context: vec![],
            remove: vec![],
            add: vec![],
        };
        apply(&[hunk], dir.path()).unwrap();
        assert!(!dir.path().join("a.rs").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("b.rs")).unwrap(),
            "content\n"
        );
    }

    // ---- 事务性 ----

    #[test]
    fn apply_transactional_partial_failure_no_write() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("f.rs"), "keep\n").unwrap();
        // hunk1:Update 成功;hunk2:Update 寻址失败 → 整体拒绝,hunk1 不写盘
        let h1 = hunk_update("f.rs", &[], &["keep"], &["changed"]);
        let h2 = hunk_update("f.rs", &[], &["nope"], &["x"]);
        let err = apply(&[h1, h2], dir.path()).unwrap_err();
        assert!(matches!(err, ApplyError::NotFoundInFile(_)));
        // f.rs 未被 h1 改动
        assert_eq!(
            fs::read_to_string(dir.path().join("f.rs")).unwrap(),
            "keep\n"
        );
    }
}
