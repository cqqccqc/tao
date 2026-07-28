//! 会话事件记录器:append-only JSONL 落盘(见 docs/design/sessions.md §1)。
//!
//! v1:每条 append + flush(简单,崩溃最多丢正在写的那条);无并发锁(单 writer);
//! 无 index.redb(扫描 JSONL);无 rotate(单文件)。
//! TODO:周期 fsync、`fs2` 并发锁、index.redb、rotate 续写链、config_fingerprint。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tao_protocol::ids::SessionId;
use tao_protocol::log::{LogEvent, LogLine};

/// 事件记录器。`run_turn` 在关键点调 `record`;exec/tui 在 run_turn 外记
/// `SessionMeta`/`UserInput`/`ModeChange`(它们控制会话生命周期)。
pub trait Recorder: Send + Sync {
    fn record(&self, event: LogEvent);
}

/// 不记录(测试 / 无持久化)。
pub struct NullRecorder;
impl Recorder for NullRecorder {
    fn record(&self, _event: LogEvent) {}
}

/// 写 JSONL 文件的记录器。
pub struct JsonlRecorder {
    file: Mutex<File>,
    path: PathBuf,
    seq: AtomicU64,
}

impl JsonlRecorder {
    /// 创建新会话(默认 base = ~/.tao):建目录 + 文件 + 写 `SessionMeta` 首条。
    pub fn create(cwd: &Path) -> std::io::Result<(Self, SessionId)> {
        let base = tao_home().ok_or_else(|| io_err("HOME 环境变量未设置"))?;
        Self::create_with_base(cwd, &base)
    }

    /// 创建新会话(指定 base,测试用)。
    pub fn create_with_base(cwd: &Path, base: &Path) -> std::io::Result<(Self, SessionId)> {
        Self::create_internal(cwd, base, None)
    }

    /// fork:新会话继承 parent(指定 base)。
    pub fn create_fork_with_base(
        cwd: &Path,
        parent: &SessionId,
        base: &Path,
    ) -> std::io::Result<(Self, SessionId)> {
        Self::create_internal(cwd, base, Some(parent.clone()))
    }

    /// fork(默认 base = ~/.tao)。
    pub fn create_fork(cwd: &Path, parent: &SessionId) -> std::io::Result<(Self, SessionId)> {
        let base = tao_home().ok_or_else(|| io_err("HOME 环境变量未设置"))?;
        Self::create_fork_with_base(cwd, parent, &base)
    }

    fn create_internal(
        cwd: &Path,
        base: &Path,
        parent: Option<SessionId>,
    ) -> std::io::Result<(Self, SessionId)> {
        let id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let path = session_path(cwd, &id, base);
        if let Some(parent_dir) = path.parent() {
            std::fs::create_dir_all(parent_dir)?;
        }
        let file = File::create(&path)?;
        let recorder = Self {
            file: Mutex::new(file),
            path: path.clone(),
            seq: AtomicU64::new(0),
        };
        recorder.record(LogEvent::SessionMeta {
            id: id.clone(),
            parent,
            cwd: cwd.to_path_buf(),
            git_head: None,
            config_fingerprint: String::new(),
            created_at_ms: now_ms(),
        });
        Ok((recorder, id))
    }

    /// append 打开已有会话(默认 base = ~/.tao),用于 resume。seq 从现有最大值续。
    pub fn open_existing(id: &SessionId, cwd: &Path) -> std::io::Result<Self> {
        let base = tao_home().ok_or_else(|| io_err("HOME 环境变量未设置"))?;
        Self::open_existing_with_base(id, cwd, &base)
    }

    pub fn open_existing_with_base(
        id: &SessionId,
        cwd: &Path,
        base: &Path,
    ) -> std::io::Result<Self> {
        let path = session_path(cwd, id, base);
        let max_seq = read_max_seq(&path).unwrap_or(0);
        let file = OpenOptions::new().append(true).read(true).open(&path)?;
        Ok(Self {
            file: Mutex::new(file),
            path,
            seq: AtomicU64::new(max_seq + 1),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Recorder for JsonlRecorder {
    fn record(&self, event: LogEvent) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let ts = now_ms();
        let line = LogLine { seq, ts, event };
        let mut json = match serde_json::to_vec(&line) {
            Ok(v) => v,
            Err(_) => return,
        };
        json.push(b'\n');
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(&json);
            let _ = file.flush();
        }
    }
}

fn read_max_seq(path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<LogLine>(l).ok())
        .map(|ll| ll.seq)
        .max()
}

/// ~/.tao/projects/<slug>/sessions/<id>.jsonl
fn session_path(cwd: &Path, id: &SessionId, base: &Path) -> PathBuf {
    base.join("projects")
        .join(slugify(cwd))
        .join("sessions")
        .join(format!("{}.jsonl", id.as_ref()))
}

/// 会话目录(默认 base = ~/.tao),供 CLI `sessions ls` 扫描。
pub fn session_dir(cwd: &Path) -> Option<PathBuf> {
    let base = tao_home()?;
    Some(base.join("projects").join(slugify(cwd)).join("sessions"))
}

/// 会话日志文件路径(默认 base = ~/.tao)。
pub fn session_file_path(cwd: &Path, id: &SessionId) -> Option<PathBuf> {
    let base = tao_home()?;
    Some(session_path(cwd, id, &base))
}

fn tao_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".tao"))
}

fn slugify(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .replace(['/', ' ', ':', '\\'], "-")
        .trim_matches('-')
        .to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tao_protocol::permission::PermissionMode;
    use tempfile::TempDir;

    #[test]
    fn slugify_strips_separators() {
        assert_eq!(slugify(Path::new("/tmp/foo bar")), "tmp-foo-bar");
        assert_eq!(
            slugify(Path::new("/Users/x/github/tao")),
            "Users-x-github-tao"
        );
    }

    #[test]
    fn create_writes_session_meta_and_record_appends() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let (recorder, id) = JsonlRecorder::create_with_base(&cwd, dir.path()).unwrap();
        recorder.record(LogEvent::ModeChange {
            mode: PermissionMode::Plan,
        });

        let content = std::fs::read_to_string(recorder.path()).unwrap();
        let lines: Vec<LogLine> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(matches!(lines[0].event, LogEvent::SessionMeta { .. }));
        assert_eq!(lines[0].seq, 0);
        assert!(matches!(lines[1].event, LogEvent::ModeChange { .. }));
        assert_eq!(lines[1].seq, 1);
        assert!(
            recorder
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(id.as_ref())
        );
    }

    #[test]
    fn open_existing_continues_seq() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let (r1, id) = JsonlRecorder::create_with_base(&cwd, dir.path()).unwrap();
        r1.record(LogEvent::ModeChange {
            mode: PermissionMode::Plan,
        });
        // seq 0(SessionMeta),1(ModeChange)。open 后从 2
        let r2 = JsonlRecorder::open_existing_with_base(&id, &cwd, dir.path()).unwrap();
        r2.record(LogEvent::ModeChange {
            mode: PermissionMode::Default,
        });
        let content = std::fs::read_to_string(r2.path()).unwrap();
        let last_line: LogLine = serde_json::from_str(content.lines().last().unwrap()).unwrap();
        assert_eq!(last_line.seq, 2);
    }

    #[test]
    fn read_max_seq_from_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            "{\"seq\":1,\"ts\":0,\"type\":\"mode_change\",\"mode\":\"plan\"}\n\
             {\"seq\":5,\"ts\":0,\"type\":\"mode_change\",\"mode\":\"default\"}\n",
        )
        .unwrap();
        assert_eq!(read_max_seq(&path), Some(5));
    }
}
