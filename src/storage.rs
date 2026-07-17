use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;

pub struct Storage {
    pool: SqlitePool,
}

#[derive(sqlx::FromRow)]
pub struct PastRewrite {
    pub original: String,
    pub rewritten: String,
}

#[derive(sqlx::FromRow)]
pub struct UntransformedTruth {
    pub id: String,
    pub content: String,
    pub published: Option<String>,
}

#[derive(sqlx::FromRow)]
pub struct UnpublishedRewrite {
    pub truth_id: String,
    pub content: String,
    pub source_url: Option<String>,
    pub original_published: Option<String>,
}

impl Storage {
    pub async fn open(path: &str) -> Result<Self> {
        let url = format!("sqlite:{path}?mode=rwc");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .context("failed to open database")?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS truths (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                url TEXT,
                published TEXT,
                fetched_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS rewrites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                truth_id TEXT NOT NULL REFERENCES truths(id),
                style TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(truth_id, style)
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS publications (
                truth_id TEXT NOT NULL,
                style TEXT NOT NULL,
                platform TEXT NOT NULL,
                published_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (truth_id, style, platform),
                FOREIGN KEY (truth_id) REFERENCES truths(id)
            )",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    pub async fn truth_exists(&self, id: &str) -> Result<bool> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM truths WHERE id = ?")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0 > 0)
    }

    pub async fn insert_truth(
        &self,
        id: &str,
        content: &str,
        url: Option<&str>,
        published: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO truths (id, content, url, published) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(content)
        .bind(url)
        .bind(published)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_rewrite(&self, truth_id: &str, style: &str, content: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO rewrites (truth_id, style, content) VALUES (?, ?, ?)")
            .bind(truth_id)
            .bind(style)
            .bind(content)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_published(&self, truth_id: &str, style: &str, platform: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO publications (truth_id, style, platform) VALUES (?, ?, ?)",
        )
        .bind(truth_id)
        .bind(style)
        .bind(platform)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_recent_rewrites(
        &self,
        style: &str,
        limit: i64,
    ) -> Result<Vec<PastRewrite>> {
        let rows = sqlx::query_as::<_, PastRewrite>(
            "SELECT t.content as original, r.content as rewritten \
             FROM rewrites r \
             JOIN truths t ON r.truth_id = t.id \
             WHERE r.style = ? \
             ORDER BY r.created_at DESC \
             LIMIT ?",
        )
        .bind(style)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_rewrites_by_style(&self, style: &str) -> Result<Vec<(i64, String)>> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, content FROM rewrites WHERE style = ?")
                .bind(style)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    pub async fn update_rewrite_content(&self, id: i64, content: &str) -> Result<()> {
        sqlx::query("UPDATE rewrites SET content = ? WHERE id = ?")
            .bind(content)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_untransformed(&self, style: &str) -> Result<Vec<UntransformedTruth>> {
        let rows = sqlx::query_as::<_, UntransformedTruth>(
            "SELECT t.id, t.content, t.published \
             FROM truths t \
             WHERE NOT EXISTS (
                 SELECT 1 FROM rewrites r
                 WHERE r.truth_id = t.id AND r.style = ?
             ) \
             AND TRIM(t.content) != '' \
             AND NOT (TRIM(t.content) GLOB 'http*' AND INSTR(TRIM(t.content), ' ') = 0) \
             ORDER BY t.published ASC",
        )
        .bind(style)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Most recent publication time for a platform (UTC "YYYY-MM-DD HH:MM:SS"),
    /// or None if nothing has been published there yet. Used to rate limit.
    pub async fn last_published_at(&self, platform: &str) -> Result<Option<String>> {
        let row: (Option<String>,) =
            sqlx::query_as("SELECT MAX(published_at) FROM publications WHERE platform = ?")
                .bind(platform)
                .fetch_one(&self.pool)
                .await?;
        Ok(row.0)
    }

    pub async fn get_unpublished(
        &self,
        style: &str,
        platform: &str,
    ) -> Result<Vec<UnpublishedRewrite>> {
        let rows = sqlx::query_as::<_, UnpublishedRewrite>(
            "SELECT r.truth_id, r.content, t.url as source_url, t.published as original_published \
             FROM rewrites r \
             JOIN truths t ON r.truth_id = t.id \
             WHERE r.style = ? \
               AND NOT EXISTS (
                   SELECT 1 FROM publications p
                   WHERE p.truth_id = r.truth_id AND p.style = r.style AND p.platform = ?
               ) \
             ORDER BY t.published ASC",
        )
        .bind(style)
        .bind(platform)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
