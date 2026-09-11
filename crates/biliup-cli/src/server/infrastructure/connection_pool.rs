use crate::server::errors::{AppError, AppResult};
use error_stack::ResultExt;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Pool, Sqlite};
use std::path::Path;
use tracing::info;

/// SQLite连接池类型别名
pub type ConnectionPool = Pool<Sqlite>;

/// 连接管理器
/// 负责管理SQLite数据库连接池的创建和配置
pub struct ConnectionManager;

impl ConnectionManager {
    /// 创建新的数据库连接池
    ///
    /// # 参数
    /// * `path` - 数据库文件路径
    ///
    /// # 返回
    /// 返回配置好的SQLite连接池
    pub async fn new_pool(path: &str) -> AppResult<ConnectionPool> {
        // 创建所有父级目录（如果不存在）
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)
                .change_context(AppError::Unknown)
                .attach_with(|| path.to_string())?; // 创建 data/ 目录
        }

        let db_url = format!("sqlite://{path}");

        // 创建数据库文件（如果不存在）
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .change_context(AppError::Unknown)?;

        // 创建连接池，最大连接数设为2
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .change_context(AppError::Custom(
                "error while initializing the database connection pool".to_string(),
            ))?;

        // 运行数据库迁移，确保数据库结构是最新的
        info!("migrations enabled, running...");
        sqlx::migrate!()
            .run(&pool)
            .await
            .change_context(AppError::Custom(
                "error while running database migrations".to_string(),
            ))?;

        Ok(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectionManager;

    #[tokio::test]
    async fn identity_migration_fails_closed_on_multiple_existing_administrators() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("data.sqlite3");
        let pool = ConnectionManager::new_pool(db.to_str().unwrap())
            .await
            .unwrap();
        sqlx::query("DROP INDEX uq_configuration_biliup_identity")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 3")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO configuration (key, value) VALUES ('biliup', 'first'), ('biliup', 'second')",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        assert!(
            ConnectionManager::new_pool(db.to_str().unwrap())
                .await
                .is_err()
        );

        let options = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}", db.display()))
            .await
            .unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM configuration WHERE key = 'biliup'")
                .fetch_one(&options)
                .await
                .unwrap();
        assert_eq!(
            count, 2,
            "migration must not pick an administrator implicitly"
        );
    }

    #[tokio::test]
    async fn migrates_legacy_split_on_timestamp_anomaly_records() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("data.sqlite3");
        let pool = ConnectionManager::new_pool(db.to_str().unwrap())
            .await
            .unwrap();

        // 插入旧格式配置
        sqlx::query("INSERT INTO configuration (key, value) VALUES ('config', '{\"split_on_timestamp_anomaly\": true, \"filtering_threshold\": 30}')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO livestreamers (url, remark, override) VALUES ('https://live.douyin.com/123', 'test', '{\"split_on_timestamp_anomaly\": false}')")
            .execute(&pool)
            .await
            .unwrap();

        // 模拟升级场景：回退并重新运行 migration 5
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 5")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();

        // 验证 configuration
        let cfg_val: String = sqlx::query_scalar("SELECT value FROM configuration WHERE key = 'config'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let cfg_json: serde_json::Value = serde_json::from_str(&cfg_val).unwrap();
        assert!(cfg_json.get("split_on_timestamp_anomaly").is_none());
        assert_eq!(cfg_json.get("timestamp_anomaly_threshold_ms").unwrap(), 5000);
        assert_eq!(cfg_json.get("filtering_threshold").unwrap(), 30);

        // 验证 livestreamers override
        let ov_val: String = sqlx::query_scalar("SELECT override FROM livestreamers WHERE url = 'https://live.douyin.com/123'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let ov_json: serde_json::Value = serde_json::from_str(&ov_val).unwrap();
        assert!(ov_json.get("split_on_timestamp_anomaly").is_none());
        assert_eq!(ov_json.get("timestamp_anomaly_threshold_ms").unwrap(), 0);
    }
}
