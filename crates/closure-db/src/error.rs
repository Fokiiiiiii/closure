use closure_core::event::GuildId;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The configuration changed since the editor loaded it. `actual` is the current revision,
    /// or `None` when the guild has no configuration yet.
    #[error("configuration revision conflict (expected {expected:?}, actual {actual:?})")]
    RevisionConflict {
        expected: Option<i64>,
        actual: Option<i64>,
    },
    #[error("id {0} does not fit a database snowflake")]
    IdOutOfRange(u64),
    #[error("database returned an invalid id {0}")]
    InvalidStoredId(i64),
    /// A stored document no longer parses, e.g. after a schema change. Other guilds are
    /// unaffected; the caller decides whether to fall back to defaults.
    #[error("stored configuration of guild {guild} cannot be read: {source}")]
    CorruptConfig {
        guild: GuildId,
        source: serde_json::Error,
    },
    #[error("{0}")]
    Invalid(String),
}
