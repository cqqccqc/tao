//! 会话事件记录器:append-only JSONL 落盘(见 docs/design/sessions.md §1)。
//!
//! v2:fs2 并发锁(多 tao 进程同 session 互斥);redb 索引(seq→byte_offset,
//! O(1) 读 max_seq);周期 fsync(5s 后台 task);rotate(单文件 >1MB 切
//! `<id>.N.jsonl`,SessionMeta parent=旧 id 续写链)。redb 初始化失败 fallback
//! 扫全文(不阻断)。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use redb::{Database, ReadableTable, TableDefinition};
use tao_protocol::ids::SessionId;
use tao_protocol::log::{LogEvent, LogLine};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

/// redb 索引表:seq → byte_offset(当前段内偏移)。
const SEQ_OFFSET: TableDefinition<u64, u64> = TableDefinition::new("seq_offset");

/// rotate 阈值:单文件 1 MB。
const ROTATE_MAX_BYTES: u64 = 1_048_576;

/// fsync 间隔。
const FSYNC_INTERVAL: Duration = Duration::from_secs(5);

/// 事件记录器。`run_turn` 在关键点调 `record`;exec/tui 在 run_turn 外记
/// `SessionMeta`/`UserInput`/`ModeChange`(它们控制会话生命周期)。
pub trait Recorder: Send + Sync {
    fn record(&self, event: LogEvent);
    /// 下一条将分配的 seq(≈ 已记录事件数)。供 compact 的 `covers_through_seq`
    /// 对齐日志 seq,使 replay 截断点准确。
    fn current_seq(&self) -> u64 {
        0
    }
}

/// 不记录(测试 / 无持久化)。
pub struct NullRecorder;
impl Recorder for NullRecorder {
    fn record(&self, _event: LogEvent) {}
}

/// 写 JSONL 文件的记录器。
pub struct JsonlRecorder {
    /// 当前段文件(Option 以便 Drop 时显式关闭,释放 fs2 锁)。
    file: Arc<Mutex<Option<File>>>,
    /// sessions 目录(用于 rotate 路径构造)。
    dir: PathBuf,
    /// 会话 ID。
    id: SessionId,
    /// 工作目录(用于 rotate 时新文件 SessionMeta)。
    cwd: PathBuf,
    seq: AtomicU64,
    /// redb 索引(seq → byte_offset),None = 初始化失败,fallback 扫全文。
    db: Option<Database>,
    /// 后台 fsync task 的 abort handle。
    fsync_handle: Option<AbortHandle>,
    /// 当前 rotate 段号(0 = `<id>.jsonl`,N = `<id>.N.jsonl`)。
    segment: AtomicU32,
    /// 当前段文件写入字节偏移(用于 redb 索引)。
    offset: AtomicU64,
    /// 配置指纹(指令文件 hash 等),rotate 时写入新段 SessionMeta。
    config_fingerprint: String,
}

impl JsonlRecorder {
    /// 创建新会话(默认 base = ~/.tao):建目录 + 文件 + 写 `SessionMeta` 首条。
    pub fn create(cwd: &Path, config_fingerprint: String) -> std::io::Result<(Self, SessionId)> {
        let base = tao_home().ok_or_else(|| io_err("HOME 环境变量未设置"))?;
        Self::create_with_base(cwd, &base, config_fingerprint)
    }

    /// 创建新会话(指定 base,测试用)。
    pub fn create_with_base(
        cwd: &Path,
        base: &Path,
        config_fingerprint: String,
    ) -> std::io::Result<(Self, SessionId)> {
        Self::create_internal(cwd, base, None, config_fingerprint)
    }

    /// fork:新会话继承 parent(指定 base)。
    pub fn create_fork_with_base(
        cwd: &Path,
        parent: &SessionId,
        base: &Path,
        config_fingerprint: String,
    ) -> std::io::Result<(Self, SessionId)> {
        Self::create_internal(cwd, base, Some(parent.clone()), config_fingerprint)
    }

    /// fork(默认 base = ~/.tao)。
    pub fn create_fork(
        cwd: &Path,
        parent: &SessionId,
        config_fingerprint: String,
    ) -> std::io::Result<(Self, SessionId)> {
        let base = tao_home().ok_or_else(|| io_err("HOME 环境变量未设置"))?;
        Self::create_fork_with_base(cwd, parent, &base, config_fingerprint)
    }

    fn create_internal(
        cwd: &Path,
        base: &Path,
        parent: Option<SessionId>,
        config_fingerprint: String,
    ) -> std::io::Result<(Self, SessionId)> {
        let id = SessionId::new(uuid::Uuid::new_v4().to_string());
        let dir = sessions_dir(cwd, base);
        std::fs::create_dir_all(&dir)?;

        let path = dir.join(format!("{}.jsonl", id.as_ref()));
        let file = File::create(&path)?;
        // fs2 独占锁:多 tao 进程同 session 互斥。
        file.try_lock_exclusive()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // redb 索引:seq → byte_offset
        let redb_path = dir.join(format!("{}.redb", id.as_ref()));
        let db = open_redb(&redb_path);

        let file_arc = Arc::new(Mutex::new(Some(file)));
        let fsync_handle = spawn_fsync(Arc::clone(&file_arc));

        let recorder = Self {
            file: file_arc,
            dir,
            id: id.clone(),
            cwd: cwd.to_path_buf(),
            seq: AtomicU64::new(0),
            db,
            fsync_handle,
            segment: AtomicU32::new(0),
            offset: AtomicU64::new(0),
            config_fingerprint: config_fingerprint.clone(),
        };
        recorder.record(LogEvent::SessionMeta {
            id: id.clone(),
            parent,
            cwd: cwd.to_path_buf(),
            git_head: None,
            config_fingerprint,
            created_at_ms: now_ms(),
        });
        Ok((recorder, id))
    }

    /// append 打开已有会话(默认 base = ~/.tao),用于 resume。seq 从现有最大值续。
    pub fn open_existing(
        id: &SessionId,
        cwd: &Path,
        config_fingerprint: String,
    ) -> std::io::Result<Self> {
        let base = tao_home().ok_or_else(|| io_err("HOME 环境变量未设置"))?;
        Self::open_existing_with_base(id, cwd, &base, config_fingerprint)
    }

    pub fn open_existing_with_base(
        id: &SessionId,
        cwd: &Path,
        base: &Path,
        config_fingerprint: String,
    ) -> std::io::Result<Self> {
        let dir = sessions_dir(cwd, base);
        let segment = find_latest_segment(&dir, id);
        let path = dir.join(segment_filename(id, segment));

        let file = OpenOptions::new().append(true).read(true).open(&path)?;
        // fs2 独占锁
        file.try_lock_exclusive()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

        // redb 索引:优先从索引 O(1) 读 max_seq,失败 fallback 扫全文
        let redb_path = dir.join(format!("{}.redb", id.as_ref()));
        let db = open_redb(&redb_path);
        let max_seq = read_max_seq_from_db(db.as_ref())
            .or_else(|| read_max_seq(&path))
            .unwrap_or(0);

        let file_arc = Arc::new(Mutex::new(Some(file)));
        let fsync_handle = spawn_fsync(Arc::clone(&file_arc));

        Ok(Self {
            file: file_arc,
            dir,
            id: id.clone(),
            cwd: cwd.to_path_buf(),
            seq: AtomicU64::new(max_seq + 1),
            db,
            fsync_handle,
            segment: AtomicU32::new(segment),
            offset: AtomicU64::new(file_size),
            config_fingerprint,
        })
    }

    /// 当前 JSONL 文件路径(可能因 rotate 而变化)。
    pub fn path(&self) -> PathBuf {
        let seg = self.segment.load(Ordering::SeqCst);
        self.dir.join(segment_filename(&self.id, seg))
    }

    /// 截断日志:只保留 seq <= `seq` 的行,删除后续行。
    ///
    /// 跨段:扫描所有段(`<id>.jsonl`、`<id>.1.jsonl`、…),找 seq 所在段
    /// (最后一个含 seq <= 目标的行的段),截该段(保留 seq <= 目标),
    /// 删除其后所有段文件,重建 redb 索引(删 > seq 的条目),
    /// 重置 segment/offset/seq 计数器指向截断后的段。
    pub fn truncate_to_seq(&self, seq: u64) -> std::io::Result<()> {
        let latest_seg = find_latest_segment(&self.dir, &self.id);

        // 扫描所有段,找 seq 所在段(最后一个含 seq <= target 的行的段)。
        // seq 跨段单调递增,故 target_seg 之前的段全部保留不变。
        let mut target_seg = 0;
        for s in 0..=latest_seg {
            let path = self.dir.join(segment_filename(&self.id, s));
            if let Ok(content) = std::fs::read_to_string(&path) {
                let has_le = content
                    .lines()
                    .filter_map(|l| serde_json::from_str::<LogLine>(l).ok())
                    .any(|ll| ll.seq <= seq);
                if has_le {
                    target_seg = s;
                }
            }
        }

        let target_path = self.dir.join(segment_filename(&self.id, target_seg));

        // 读目标段内容,保留 seq <= target 的行
        let content = std::fs::read_to_string(&target_path)?;
        let kept: Vec<LogLine> = content
            .lines()
            .filter_map(|l| serde_json::from_str::<LogLine>(l).ok())
            .filter(|ll| ll.seq <= seq)
            .collect();

        // 关闭当前文件句柄,释放 fs2 锁,以便重写
        if let Ok(mut guard) = self.file.lock() {
            *guard = None;
        }

        // 重写目标段文件(仅保留 <= seq 的行)
        let mut out = String::new();
        for ll in &kept {
            if let Ok(json) = serde_json::to_string(ll) {
                out.push_str(&json);
                out.push('\n');
            }
        }
        std::fs::write(&target_path, &out)?;

        // 删除目标段之后的所有段文件
        for s in (target_seg + 1)..=latest_seg {
            let path = self.dir.join(segment_filename(&self.id, s));
            let _ = std::fs::remove_file(&path);
        }

        // 重新打开目标段文件 + fs2 锁
        let file = OpenOptions::new()
            .append(true)
            .read(true)
            .open(&target_path)?;
        file.try_lock_exclusive()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let new_offset = out.len() as u64;
        let new_seq = kept.last().map(|ll| ll.seq + 1).unwrap_or(0);

        // 重建 redb 索引:删除 > seq 的条目
        if let Some(db) = &self.db
            && let Ok(wtxn) = db.begin_write()
        {
            if let Ok(mut table) = wtxn.open_table(SEQ_OFFSET) {
                // 收集需要删除的 key(> seq),然后逐一删除
                let to_delete: Vec<u64> = table
                    .iter()
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|res| {
                        let (k, _) = res.ok()?;
                        if k.value() > seq {
                            Some(k.value())
                        } else {
                            None
                        }
                    })
                    .collect();
                for k in to_delete {
                    let _ = table.remove(k);
                }
            }
            let _ = wtxn.commit();
        }

        // 重置内部状态:指向截断后的段
        self.segment.store(target_seg, Ordering::SeqCst);
        self.offset.store(new_offset, Ordering::SeqCst);
        self.seq.store(new_seq, Ordering::SeqCst);

        // 放回文件句柄
        if let Ok(mut guard) = self.file.lock() {
            *guard = Some(file);
        }

        tracing::info!(
            seq,
            kept = kept.len(),
            segment = target_seg,
            "truncated recorder to seq (cross-segment)"
        );
        Ok(())
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

        if let Ok(mut guard) = self.file.lock() {
            // 写入 + flush(scope file 引用,使后续 *guard = ... 可借用)
            let (offset, new_offset) = {
                let Some(file) = guard.as_mut() else {
                    return;
                };
                let offset = self.offset.load(Ordering::SeqCst);
                let _ = file.write_all(&json);
                let _ = file.flush();
                (offset, offset + json.len() as u64)
            };
            self.offset.store(new_offset, Ordering::SeqCst);

            // redb 索引:seq → byte_offset
            if let Some(db) = &self.db
                && let Ok(wtxn) = db.begin_write()
            {
                if let Ok(mut table) = wtxn.open_table(SEQ_OFFSET) {
                    let _ = table.insert(seq, offset);
                }
                let _ = wtxn.commit();
            }

            // rotate:单文件超阈值时切段
            if new_offset > ROTATE_MAX_BYTES {
                let next_seg = self.segment.fetch_add(1, Ordering::SeqCst) + 1;
                let new_path = self.dir.join(segment_filename(&self.id, next_seg));
                if let Ok(mut new_file) = File::create(&new_path) {
                    let _ = new_file.try_lock_exclusive();
                    // 新文件首条:SessionMeta(parent = self.id)标记续写链
                    let meta_seq = self.seq.fetch_add(1, Ordering::SeqCst);
                    let meta_line = LogLine {
                        seq: meta_seq,
                        ts: now_ms(),
                        event: LogEvent::SessionMeta {
                            id: self.id.clone(),
                            parent: Some(self.id.clone()),
                            cwd: self.cwd.clone(),
                            git_head: None,
                            config_fingerprint: self.config_fingerprint.clone(),
                            created_at_ms: now_ms(),
                        },
                    };
                    let meta_len = if let Ok(mut meta_json) = serde_json::to_vec(&meta_line) {
                        meta_json.push(b'\n');
                        let _ = new_file.write_all(&meta_json);
                        let _ = new_file.flush();
                        // redb 索引:新文件 offset 0
                        if let Some(db) = &self.db
                            && let Ok(wtxn) = db.begin_write()
                        {
                            if let Ok(mut table) = wtxn.open_table(SEQ_OFFSET) {
                                let _ = table.insert(meta_seq, 0u64);
                            }
                            let _ = wtxn.commit();
                        }
                        meta_json.len() as u64
                    } else {
                        0
                    };
                    // 替换文件(旧 fd 关闭 → fs2 锁释放)
                    *guard = Some(new_file);
                    self.offset.store(meta_len, Ordering::SeqCst);
                }
            }
        }
    }

    fn current_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }
}

impl Drop for JsonlRecorder {
    fn drop(&mut self) {
        // 停止后台 fsync task
        if let Some(handle) = self.fsync_handle.take() {
            handle.abort();
        }
        // 显式关闭文件,释放 fs2 锁(即使 task 的 Arc 尚未释放)
        if let Ok(mut guard) = self.file.lock() {
            *guard = None;
        }
    }
}

// ---- redb 索引 ----

/// 打开(或创建)redb 数据库。失败返回 None(不阻断,fallback 扫全文)。
fn open_redb(path: &Path) -> Option<Database> {
    match Database::create(path) {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!("redb 初始化失败,fallback 扫全文: {e}");
            None
        }
    }
}

/// 从 redb 索引 O(1) 读 max_seq(最大 key)。
fn read_max_seq_from_db(db: Option<&Database>) -> Option<u64> {
    let db = db?;
    let txn = db.begin_read().ok()?;
    let table = txn.open_table(SEQ_OFFSET).ok()?;
    match table.last() {
        Ok(Some((k, _))) => Some(k.value()),
        Ok(None) => None,
        Err(_) => None,
    }
}

// ---- rotate 辅助 ----

/// 段文件名:`<id>.jsonl`(seg 0)或 `<id>.N.jsonl`(seg N)。
fn segment_filename(id: &SessionId, segment: u32) -> String {
    if segment == 0 {
        format!("{}.jsonl", id.as_ref())
    } else {
        format!("{}.{}.jsonl", id.as_ref(), segment)
    }
}

/// 扫描目录,找到最新段号(`<id>.jsonl` = 0,`<id>.N.jsonl` = N)。
fn find_latest_segment(dir: &Path, id: &SessionId) -> u32 {
    let mut max_seg = 0;
    // 原始文件 <id>.jsonl 视为段 0
    if !dir.join(segment_filename(id, 0)).exists() {
        // 连原始文件都不存在,返回 0(调用方会报错)
        return 0;
    }
    for seg in 1..=999 {
        if dir.join(segment_filename(id, seg)).exists() {
            max_seg = seg;
        } else {
            break;
        }
    }
    max_seg
}

// ---- 全文扫描 fallback ----

fn read_max_seq(path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<LogLine>(l).ok())
        .map(|ll| ll.seq)
        .max()
}

// ---- 路径辅助 ----

/// `<base>/projects/<slug>/sessions`
fn sessions_dir(cwd: &Path, base: &Path) -> PathBuf {
    base.join("projects").join(slugify(cwd)).join("sessions")
}

/// ~/.tao/projects/<slug>/sessions/<id>.jsonl
fn session_path(cwd: &Path, id: &SessionId, base: &Path) -> PathBuf {
    sessions_dir(cwd, base).join(format!("{}.jsonl", id.as_ref()))
}

/// 会话目录(默认 base = ~/.tao),供 CLI `sessions ls` 扫描。
pub fn session_dir(cwd: &Path) -> Option<PathBuf> {
    let base = tao_home()?;
    Some(sessions_dir(cwd, &base))
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

// ---- fsync 后台 task ----

/// 若当前线程有 tokio runtime,spawn 一个每 5s sync_all 的后台 task。
/// 无 runtime(同步测试)则不启动。返回 AbortHandle 以便 Drop 时停止。
fn spawn_fsync(file: Arc<Mutex<Option<File>>>) -> Option<AbortHandle> {
    match Handle::try_current() {
        Ok(handle) => {
            let task = handle.spawn(async move {
                loop {
                    tokio::time::sleep(FSYNC_INTERVAL).await;
                    if let Ok(guard) = file.lock()
                        && let Some(f) = guard.as_ref()
                    {
                        let _ = f.sync_all();
                    }
                }
            });
            Some(task.abort_handle())
        }
        Err(_) => None,
    }
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
        let (recorder, id) =
            JsonlRecorder::create_with_base(&cwd, dir.path(), String::new()).unwrap();
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
        let (r1, id) = JsonlRecorder::create_with_base(&cwd, dir.path(), String::new()).unwrap();
        r1.record(LogEvent::ModeChange {
            mode: PermissionMode::Plan,
        });
        // seq 0(SessionMeta),1(ModeChange)。释放 fs2 锁后 open 从 2 续。
        drop(r1);
        let r2 =
            JsonlRecorder::open_existing_with_base(&id, &cwd, dir.path(), String::new()).unwrap();
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

    #[test]
    fn truncate_cross_segment() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let (recorder, id) =
            JsonlRecorder::create_with_base(&cwd, dir.path(), String::new()).unwrap();
        // seq 0: SessionMeta, 1: ModeChange(plan), 2: ModeChange(default)
        recorder.record(LogEvent::ModeChange {
            mode: PermissionMode::Plan,
        });
        recorder.record(LogEvent::ModeChange {
            mode: PermissionMode::Default,
        });

        let seg0_path = recorder.path();
        let sessions_dir = seg0_path.parent().unwrap();

        // 模拟 rotate:手动创建段 1 文件(seqs 3–5)
        let seg1_path = sessions_dir.join(format!("{}.1.jsonl", id.as_ref()));
        let meta_line = LogLine {
            seq: 3,
            ts: 0,
            event: LogEvent::SessionMeta {
                id: id.clone(),
                parent: Some(id.clone()),
                cwd: cwd.clone(),
                git_head: None,
                config_fingerprint: String::new(),
                created_at_ms: 0,
            },
        };
        let mode1 = LogLine {
            seq: 4,
            ts: 0,
            event: LogEvent::ModeChange {
                mode: PermissionMode::Plan,
            },
        };
        let mode2 = LogLine {
            seq: 5,
            ts: 0,
            event: LogEvent::ModeChange {
                mode: PermissionMode::Default,
            },
        };
        let seg1_content = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&meta_line).unwrap(),
            serde_json::to_string(&mode1).unwrap(),
            serde_json::to_string(&mode2).unwrap(),
        );
        std::fs::write(&seg1_path, &seg1_content).unwrap();

        // 截断到 seq 1(段 0)——应删除段 1
        recorder.truncate_to_seq(1).unwrap();

        // 段 0 应仅保留 seqs 0, 1(seq 2 已删)
        let content = std::fs::read_to_string(&seg0_path).unwrap();
        let lines: Vec<LogLine> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].seq, 0);
        assert_eq!(lines[1].seq, 1);

        // 段 1 应已删除
        assert!(!seg1_path.exists());

        // recorder 状态:指向段 0,next seq = 2
        assert_eq!(recorder.segment.load(Ordering::SeqCst), 0);
        assert_eq!(recorder.current_seq(), 2);
    }

    #[test]
    fn truncate_to_current_segment_latest_seq() {
        // 单段场景:截断到最后一条 seq(应保留全部,no-op)
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let (recorder, _id) =
            JsonlRecorder::create_with_base(&cwd, dir.path(), String::new()).unwrap();
        recorder.record(LogEvent::ModeChange {
            mode: PermissionMode::Plan,
        });
        let before = recorder.current_seq();
        recorder.truncate_to_seq(1).unwrap();
        // next seq 不变(截断到 seq 1 = 保留全部 2 条)
        assert_eq!(recorder.current_seq(), before);
        assert_eq!(recorder.segment.load(Ordering::SeqCst), 0);
    }
}
