//! 持久化层。设计对齐参考实现（Better-Clip）的泛化 KV + JSON 文档模型：
//! 主表结构统一为 `Id(PK) + UpdateTime + Data(JSON) + ObjectType + CreatedTime`，
//! 新增对象种类无需变更表结构。查询走内存缓存，写入异步回落数据库。

use crate::config::{ClipItem, FavorItem};
use parking_lot::RwLock;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type Result<T> = std::result::Result<T, DbError>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// 内存缓存。启动时一次性从数据库加载，之后读操作全部命中内存。
#[derive(Default)]
pub struct Cache {
    pub clips: Vec<ClipItem>,
    pub favors: Vec<FavorItem>,
}

pub struct Store {
    /// SQLite 连接本身不是 Sync，必须置于互斥锁后方可跨线程共享。
    /// 写操作频率低（每次复制一次），锁竞争可忽略。
    conn: parking_lot::Mutex<Connection>,
    pub cache: Arc<RwLock<Cache>>,
    pub db_path: PathBuf,
}

/// 三类文档共用的表结构：`Id(PK) + UpdateTime + Data(JSON) + ObjectType + CreatedTime`。
/// 新增对象种类只需加一张同构表，无需改列。
const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS clip_item (
    Id          TEXT NOT NULL PRIMARY KEY,
    UpdateTime  TEXT NOT NULL,
    Data        TEXT NOT NULL,
    ObjectType  TEXT,
    CreatedTime TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS favor_item (
    Id          TEXT NOT NULL PRIMARY KEY,
    UpdateTime  TEXT NOT NULL,
    Data        TEXT NOT NULL,
    ObjectType  TEXT,
    CreatedTime TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_clip_update ON clip_item(UpdateTime DESC);
CREATE INDEX IF NOT EXISTS idx_favor_parent ON favor_item(ObjectType);
"#;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;

        let store = Self {
            conn: parking_lot::Mutex::new(conn),
            cache: Arc::new(RwLock::new(Cache::default())),
            db_path: path.to_path_buf(),
        };
        store.reload_cache()?;
        Ok(store)
    }

    /// 从数据库全量加载到内存缓存。
    pub fn reload_cache(&self) -> Result<()> {
        let clips = self.load_table::<ClipItem>("clip_item")?;
        let favors = self.load_table::<FavorItem>("favor_item")?;
        let mut c = self.cache.write();
        c.clips = clips;
        c.favors = favors;
        Ok(())
    }

    /// 通用加载：从指定表读出所有 Data 列并反序列化。
    /// 锁守卫绑定到局部变量，保证 Statement 的借用生命周期有效。
    ///
    /// `table` 只接受本模块内的字面量常量（`clip_item` / `favor_item`），
    /// 不存在外部输入，因此用 `format!` 拼表名是安全的。
    fn load_table<T: serde::de::DeserializeOwned>(&self, table: &str) -> Result<Vec<T>> {
        let guard = self.conn.lock();
        let mut stmt = guard.prepare(&format!(
            "SELECT Data FROM {table} ORDER BY UpdateTime DESC"
        ))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            if let Ok(item) = serde_json::from_str::<T>(&r?) {
                out.push(item);
            }
        }
        Ok(out)
    }

    // ---------- 剪贴板 ----------

    pub fn upsert_clip(&self, item: &ClipItem) -> Result<()> {
        let data = serde_json::to_string(item)?;
        self.conn.lock().execute(
            "INSERT INTO clip_item (Id, UpdateTime, Data, ObjectType, CreatedTime)
             VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(Id) DO UPDATE SET UpdateTime = ?2, Data = ?3",
            params![item.id, item.update_time, data, item.create_time],
        )?;
        Ok(())
    }

    pub fn delete_clips(&self, ids: &[String]) -> Result<()> {
        // 事务需要 &mut Connection，故先取守卫再借用，避免临时值提前释放
        let mut guard = self.conn.lock();
        let tx = guard.transaction()?;
        for id in ids {
            tx.execute("DELETE FROM clip_item WHERE Id = ?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_clips(&self) -> Result<()> {
        self.conn.lock().execute("DELETE FROM clip_item", [])?;
        Ok(())
    }

    // ---------- 收藏夹 ----------

    pub fn upsert_favor(&self, item: &FavorItem) -> Result<()> {
        let data = serde_json::to_string(item)?;
        self.conn.lock().execute(
            "INSERT INTO favor_item (Id, UpdateTime, Data, ObjectType, CreatedTime)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(Id) DO UPDATE SET UpdateTime = ?2, Data = ?3, ObjectType = ?4",
            params![
                item.id,
                item.update_time,
                data,
                item.parent_id,
                item.create_time
            ],
        )?;
        Ok(())
    }

    pub fn delete_favor(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM favor_item WHERE Id = ?1", params![id])?;
        Ok(())
    }
}
