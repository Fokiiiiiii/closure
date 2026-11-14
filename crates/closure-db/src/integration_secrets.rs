use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use closure_core::event::{GuildId, UserId};
use closure_core::integration_secrets::{
    IdempotencyDisposition, IntegrationSecretBackend, IntegrationSecretError,
    IntegrationSecretGeneration, IntegrationSecretMutation, IntegrationSecretScope,
    SealedIntegrationSecret,
};
use closure_core::module::ModuleId;
use serde_json::Value;
use sqlx::Row;
use subtle::ConstantTimeEq;

use crate::Db;
use crate::error::DbError;
use crate::ids::to_db;

/// A secret name an edit drops from an integration that keeps its generation.
pub(crate) struct RemovedSecret {
    generation: IntegrationSecretGeneration,
    secret_name: String,
}

/// The names each integration of `document` lists, keyed by ID, for integrations with a generation.
fn integrations_by_id(document: &Value) -> BTreeMap<String, (String, BTreeSet<String>)> {
    document
        .pointer("/modules/tickets/config/integrations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|integration| {
            let id = integration.get("id")?.as_str()?;
            let generation = integration
                .get("generation")?
                .as_str()
                .filter(|generation| !generation.is_empty())?;
            let names = integration
                .get("secrets")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            Some((id.to_owned(), (generation.to_owned(), names)))
        })
        .collect()
}

/// Names that `next` drops from an integration whose generation `previous` shares. A generation change
/// is retirement's job, so those integrations are left out here.
pub(crate) fn removed_secret_names(
    guild: GuildId,
    previous: Option<&Value>,
    next: &Value,
) -> Result<Vec<RemovedSecret>, crate::error::DbError> {
    let Some(previous) = previous else {
        return Ok(Vec::new());
    };
    let module = ModuleId::new("tickets")
        .map_err(|error| crate::error::DbError::Invalid(error.to_string()))?;
    let after = integrations_by_id(next);
    let mut removed = Vec::new();
    for (integration_id, (generation, names)) in integrations_by_id(previous) {
        let Some((next_generation, next_names)) = after.get(&integration_id) else {
            continue;
        };
        if *next_generation != generation {
            continue;
        }
        for secret_name in names.difference(next_names) {
            let scope = IntegrationSecretGeneration::new(
                guild,
                module.clone(),
                integration_id.clone(),
                generation.clone(),
            )
            .map_err(|_| {
                crate::error::DbError::Invalid(
                    "the stored ticket integration has an invalid generation".into(),
                )
            })?;
            removed.push(RemovedSecret {
                generation: scope,
                secret_name: secret_name.clone(),
            });
        }
    }
    Ok(removed)
}

/// Removes the stored values of every name `next` drops, inside the caller's save transaction.
pub(crate) async fn remove_dropped_secrets_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    guild: GuildId,
    previous: Option<&Value>,
    next: &Value,
    can_retire_secrets: bool,
) -> Result<(), crate::error::DbError> {
    for removed in removed_secret_names(guild, previous, next)? {
        remove_secret_name_in_transaction(transaction, &removed, can_retire_secrets).await?;
    }
    Ok(())
}

/// Deletes a dropped name's stored value and its idempotency receipts in the save's transaction.
/// A stored value needs the administrator permission, as retirement does.
pub(crate) async fn remove_secret_name_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    removed: &RemovedSecret,
    can_retire_secrets: bool,
) -> Result<(), crate::error::DbError> {
    let generation = &removed.generation;
    let guild_id = to_db(generation.guild_id())?;
    // The row lock serializes the removal against stores for this generation, as retirement does.
    sqlx::query(
        "SELECT retired FROM integration_secret_generations
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4
         FOR UPDATE",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .fetch_optional(&mut **transaction)
    .await?;
    let has_value = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM integration_secrets
             WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3
               AND generation = $4 AND secret_name = $5
         )",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .bind(&removed.secret_name)
    .fetch_one(&mut **transaction)
    .await?;
    if has_value && !can_retire_secrets {
        return Err(crate::error::DbError::IntegrationSecretAdministratorRequired);
    }
    sqlx::query(
        "DELETE FROM integration_secrets
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3
           AND generation = $4 AND secret_name = $5",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .bind(&removed.secret_name)
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "DELETE FROM integration_secret_receipts
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3
           AND generation = $4 AND secret_name = $5",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .bind(&removed.secret_name)
    .execute(&mut **transaction)
    .await?;
    record_tombstone_in_transaction(transaction, generation, &removed.secret_name).await?;
    Ok(())
}

/// Records that `secret_name` was dropped from `generation`. The row holds no value; while it exists
/// the name reads as having none, until a new value is stored.
async fn record_tombstone_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    generation: &IntegrationSecretGeneration,
    secret_name: &str,
) -> Result<(), crate::error::DbError> {
    let guild_id = to_db(generation.guild_id())?;
    // A name can be dropped before any value was stored, so its generation row may not exist yet.
    sqlx::query(
        "INSERT INTO integration_secret_generations
            (guild_id, module_id, integration_id, generation)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (guild_id, module_id, integration_id, generation) DO NOTHING",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "INSERT INTO integration_secret_tombstones
            (guild_id, module_id, integration_id, generation, secret_name)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (guild_id, module_id, integration_id, generation, secret_name)
         DO UPDATE SET removed_at = now()",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .bind(secret_name)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Storing a value ends the tombstone of its name, so the name reads as stored again.
async fn clear_tombstone_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    generation: &IntegrationSecretGeneration,
    secret_name: &str,
) -> Result<(), crate::error::DbError> {
    sqlx::query(
        "DELETE FROM integration_secret_tombstones
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3
           AND generation = $4 AND secret_name = $5",
    )
    .bind(to_db(generation.guild_id())?)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .bind(secret_name)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn retire_generation_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    generation: &IntegrationSecretGeneration,
    can_retire_secrets: bool,
) -> Result<(), crate::error::DbError> {
    let guild_id = to_db(generation.guild_id())?;
    sqlx::query(
        "INSERT INTO integration_secret_generations
            (guild_id, module_id, integration_id, generation)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (guild_id, module_id, integration_id, generation) DO NOTHING",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .execute(&mut **transaction)
    .await?;
    // A row lock serializes tombstoning against stores for this exact generation.
    let _retired = sqlx::query_scalar::<_, bool>(
        "SELECT retired FROM integration_secret_generations
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4
         FOR UPDATE",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .fetch_one(&mut **transaction)
    .await?;
    let has_secret = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM integration_secrets
             WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4
         )",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .fetch_one(&mut **transaction)
    .await?;
    if has_secret && !can_retire_secrets {
        return Err(crate::error::DbError::IntegrationSecretAdministratorRequired);
    }
    sqlx::query(
        "UPDATE integration_secret_generations
         SET retired = TRUE, retired_at = COALESCE(retired_at, now())
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "DELETE FROM integration_secrets
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .execute(&mut **transaction)
    .await?;
    sqlx::query(
        "DELETE FROM integration_secret_receipts
         WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4",
    )
    .bind(guild_id)
    .bind(generation.module().as_str())
    .bind(generation.integration_id())
    .bind(generation.generation())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[async_trait]
impl IntegrationSecretBackend for Db {
    async fn load(
        &self,
        scope: &IntegrationSecretScope,
    ) -> Result<Option<SealedIntegrationSecret>, IntegrationSecretError> {
        let generation = scope.generation_scope();
        let guild_id =
            to_db(generation.guild_id()).map_err(|_| IntegrationSecretError::InvalidScope)?;
        let bytes = sqlx::query(
            "SELECT s.encrypted_value
             FROM integration_secrets AS s
             JOIN integration_secret_generations AS g
               USING (guild_id, module_id, integration_id, generation)
             WHERE s.guild_id = $1
               AND s.module_id = $2
               AND s.integration_id = $3
               AND s.generation = $4
               AND s.secret_name = $5
               AND NOT g.retired
               AND NOT EXISTS (
                   SELECT 1 FROM integration_secret_tombstones AS t
                   WHERE t.guild_id = s.guild_id AND t.module_id = s.module_id
                     AND t.integration_id = s.integration_id AND t.generation = s.generation
                     AND t.secret_name = s.secret_name
               )",
        )
        .bind(guild_id)
        .bind(generation.module().as_str())
        .bind(generation.integration_id())
        .bind(generation.generation())
        .bind(scope.secret_name())
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| IntegrationSecretError::Storage)?
        .map(|row| row.try_get::<Vec<u8>, _>("encrypted_value"))
        .transpose()
        .map_err(|_| IntegrationSecretError::Storage)?;
        bytes.map(SealedIntegrationSecret::from_storage).transpose()
    }

    #[allow(clippy::too_many_lines)]
    async fn apply(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        request_key_hmac: [u8; 32],
        fingerprint_hmac: [u8; 32],
        mutation: IntegrationSecretMutation,
        encrypted_receipt: SealedIntegrationSecret,
    ) -> Result<IdempotencyDisposition, IntegrationSecretError> {
        let generation = scope.generation_scope();
        let guild_id =
            to_db(generation.guild_id()).map_err(|_| IntegrationSecretError::InvalidScope)?;
        let actor_id = to_db(actor).map_err(|_| IntegrationSecretError::InvalidScope)?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| IntegrationSecretError::Storage)?;

        sqlx::query(
            "INSERT INTO integration_secret_generations
                (guild_id, module_id, integration_id, generation)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (guild_id, module_id, integration_id, generation) DO NOTHING",
        )
        .bind(guild_id)
        .bind(generation.module().as_str())
        .bind(generation.integration_id())
        .bind(generation.generation())
        .execute(&mut *transaction)
        .await
        .map_err(|_| IntegrationSecretError::Storage)?;
        let retired = sqlx::query_scalar::<_, bool>(
            "SELECT retired FROM integration_secret_generations
             WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3 AND generation = $4
             FOR UPDATE",
        )
        .bind(guild_id)
        .bind(generation.module().as_str())
        .bind(generation.integration_id())
        .bind(generation.generation())
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| IntegrationSecretError::Storage)?;
        if retired {
            return Err(IntegrationSecretError::GenerationRetired);
        }

        sqlx::query("DELETE FROM integration_secret_receipts WHERE expires_at <= now()")
            .execute(&mut *transaction)
            .await
            .map_err(|_| IntegrationSecretError::Storage)?;
        let inserted = sqlx::query(
            "INSERT INTO integration_secret_receipts
                (guild_id, module_id, integration_id, generation, secret_name,
                 actor_id, request_key_hmac, fingerprint_hmac, encrypted_receipt, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now() + make_interval(secs => $10))
             ON CONFLICT
                (guild_id, module_id, integration_id, generation, secret_name, actor_id, request_key_hmac)
             DO NOTHING",
        )
        .bind(guild_id)
        .bind(generation.module().as_str())
        .bind(generation.integration_id())
        .bind(generation.generation())
        .bind(scope.secret_name())
        .bind(actor_id)
        .bind(&request_key_hmac[..])
        .bind(&fingerprint_hmac[..])
        .bind(encrypted_receipt.as_bytes())
        .bind(crate::REQUESTS_KEPT.as_secs_f64())
        .execute(&mut *transaction)
        .await
        .map_err(|_| IntegrationSecretError::Storage)?
        .rows_affected();
        if inserted == 0 {
            let stored_fingerprint = sqlx::query_scalar::<_, Vec<u8>>(
                "SELECT fingerprint_hmac FROM integration_secret_receipts
                 WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3
                   AND generation = $4 AND secret_name = $5 AND actor_id = $6
                   AND request_key_hmac = $7",
            )
            .bind(guild_id)
            .bind(generation.module().as_str())
            .bind(generation.integration_id())
            .bind(generation.generation())
            .bind(scope.secret_name())
            .bind(actor_id)
            .bind(&request_key_hmac[..])
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| IntegrationSecretError::Storage)?
            .ok_or(IntegrationSecretError::Storage)?;
            if !constant_time_eq(&stored_fingerprint, &fingerprint_hmac) {
                return Err(IntegrationSecretError::IdempotencyConflict);
            }
            transaction
                .commit()
                .await
                .map_err(|_| IntegrationSecretError::Storage)?;
            return Ok(IdempotencyDisposition::Replayed);
        }

        match mutation {
            IntegrationSecretMutation::Set(secret) => {
                clear_tombstone_in_transaction(&mut transaction, generation, scope.secret_name())
                    .await
                    .map_err(|_| IntegrationSecretError::Storage)?;
                sqlx::query(
                    "INSERT INTO integration_secrets
                        (guild_id, module_id, integration_id, generation, secret_name, encrypted_value)
                     VALUES ($1, $2, $3, $4, $5, $6)
                     ON CONFLICT (guild_id, module_id, integration_id, generation, secret_name)
                     DO UPDATE SET encrypted_value = EXCLUDED.encrypted_value, updated_at = now()",
                )
                .bind(guild_id)
                .bind(generation.module().as_str())
                .bind(generation.integration_id())
                .bind(generation.generation())
                .bind(scope.secret_name())
                .bind(secret.as_bytes())
                .execute(&mut *transaction)
                .await
                .map_err(|_| IntegrationSecretError::Storage)?;
            }
            IntegrationSecretMutation::Remove => {
                sqlx::query(
                    "DELETE FROM integration_secrets
                     WHERE guild_id = $1 AND module_id = $2 AND integration_id = $3
                       AND generation = $4 AND secret_name = $5",
                )
                .bind(guild_id)
                .bind(generation.module().as_str())
                .bind(generation.integration_id())
                .bind(generation.generation())
                .bind(scope.secret_name())
                .execute(&mut *transaction)
                .await
                .map_err(|_| IntegrationSecretError::Storage)?;
                record_tombstone_in_transaction(&mut transaction, generation, scope.secret_name())
                    .await
                    .map_err(|_| IntegrationSecretError::Storage)?;
            }
        }

        transaction
            .commit()
            .await
            .map_err(|_| IntegrationSecretError::Storage)?;
        Ok(IdempotencyDisposition::Applied)
    }
}

/// A stored secret value that no saved integration configuration names. Only its place is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanIntegrationSecret {
    pub integration_id: String,
    pub generation: String,
    pub secret_name: String,
    pub updated_at: time::OffsetDateTime,
}

type NamedIntegrations = BTreeMap<String, (String, BTreeSet<String>)>;

fn is_named(
    named: &NamedIntegrations,
    integration_id: &str,
    generation: &str,
    secret_name: &str,
) -> bool {
    named
        .get(integration_id)
        .is_some_and(|(current, names)| current == generation && names.contains(secret_name))
}

impl Db {
    /// Stored values of ticket integrations that the saved configuration no longer names.
    pub async fn list_orphan_integration_secrets(
        &self,
        guild: GuildId,
    ) -> Result<Vec<OrphanIntegrationSecret>, DbError> {
        let guild_id = to_db(guild)?;
        let document: Option<Value> =
            sqlx::query_scalar("SELECT doc FROM guild_config WHERE guild_id = $1")
                .bind(guild_id)
                .fetch_optional(&self.pool)
                .await?;
        let named = document
            .as_ref()
            .map(integrations_by_id)
            .unwrap_or_default();
        let rows = sqlx::query_as::<_, (String, String, String, time::OffsetDateTime)>(
            "SELECT s.integration_id, s.generation, s.secret_name, s.updated_at
             FROM integration_secrets AS s
             JOIN integration_secret_generations AS g
               USING (guild_id, module_id, integration_id, generation)
             WHERE s.guild_id = $1 AND s.module_id = 'tickets' AND NOT g.retired
             ORDER BY s.integration_id, s.generation, s.secret_name",
        )
        .bind(guild_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter(|(integration_id, generation, secret_name, _)| {
                !is_named(&named, integration_id, generation, secret_name)
            })
            .map(
                |(integration_id, generation, secret_name, updated_at)| OrphanIntegrationSecret {
                    integration_id,
                    generation,
                    secret_name,
                    updated_at,
                },
            )
            .collect())
    }

    /// Deletes the stored value of a name that no saved configuration lists, with its receipts, and
    /// leaves a tombstone. Returns `false` when no such value is stored, and fails with
    /// `IntegrationSecretStillActive` while a saved configuration still lists the name.
    pub async fn delete_orphan_integration_secret(
        &self,
        guild: GuildId,
        integration_id: &str,
        generation: &str,
        secret_name: &str,
    ) -> Result<bool, DbError> {
        let module =
            ModuleId::new("tickets").map_err(|error| DbError::Invalid(error.to_string()))?;
        let scope = IntegrationSecretGeneration::new(guild, module, integration_id, generation)
            .map_err(|_| DbError::Invalid("the integration generation is invalid".into()))?;
        let guild_id = to_db(guild)?;
        let mut transaction = self.pool.begin().await?;
        // Locking the configuration row makes this check and the delete one step against a save.
        let document: Option<Value> =
            sqlx::query_scalar("SELECT doc FROM guild_config WHERE guild_id = $1 FOR UPDATE")
                .bind(guild_id)
                .fetch_optional(&mut *transaction)
                .await?;
        let named = document
            .as_ref()
            .map(integrations_by_id)
            .unwrap_or_default();
        if is_named(&named, integration_id, generation, secret_name) {
            return Err(DbError::IntegrationSecretStillActive);
        }
        let stored = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM integration_secrets
                 WHERE guild_id = $1 AND module_id = 'tickets' AND integration_id = $2
                   AND generation = $3 AND secret_name = $4
             )",
        )
        .bind(guild_id)
        .bind(integration_id)
        .bind(generation)
        .bind(secret_name)
        .fetch_one(&mut *transaction)
        .await?;
        if !stored {
            return Ok(false);
        }
        let removed = RemovedSecret {
            generation: scope,
            secret_name: secret_name.to_owned(),
        };
        remove_secret_name_in_transaction(&mut transaction, &removed, true).await?;
        transaction.commit().await?;
        Ok(true)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8; 32]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(&right[..]))
}

#[cfg(test)]
mod tests {
    use closure_core::integration_secrets::IntegrationSecretBackend;
    use closure_core::module::ModuleId;
    use serde_json::json;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    const GENERATION: &str = "0123456789abcdef0123456789abcdef";

    fn crm(guild: u64) -> IntegrationSecretGeneration {
        IntegrationSecretGeneration::new(
            Id::new(guild),
            ModuleId::new("tickets").unwrap(),
            "crm",
            GENERATION,
        )
        .unwrap()
    }

    async fn save_config_naming(pool: &PgPool, guild: i64, secrets: &[&str]) {
        let doc = json!({ "modules": { "tickets": { "config": { "integrations": [{
            "id": "crm",
            "auth": "bearer",
            "url": "https://example.com/hook",
            "secrets": secrets,
            "generation": GENERATION,
        }] } } } });
        sqlx::query("INSERT INTO guilds (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
            .bind(guild)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO guild_config (guild_id, revision, doc) VALUES ($1, 1, $2)
             ON CONFLICT (guild_id) DO UPDATE SET doc = EXCLUDED.doc",
        )
        .bind(guild)
        .bind(&doc)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn store_value(pool: &PgPool, guild: i64, name: &str, value: Vec<u8>) {
        sqlx::query(
            "INSERT INTO integration_secret_generations (guild_id, module_id, integration_id, generation)
             VALUES ($1, 'tickets', 'crm', $2) ON CONFLICT DO NOTHING",
        )
        .bind(guild)
        .bind(GENERATION)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO integration_secrets
                (guild_id, module_id, integration_id, generation, secret_name, encrypted_value)
             VALUES ($1, 'tickets', 'crm', $2, $3, $4)",
        )
        .bind(guild)
        .bind(GENERATION)
        .bind(name)
        .bind(value)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn stored_names(pool: &PgPool, guild: i64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT secret_name FROM integration_secrets WHERE guild_id = $1 ORDER BY secret_name",
        )
        .bind(guild)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn tombstoned_names(pool: &PgPool, guild: i64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT secret_name FROM integration_secret_tombstones WHERE guild_id = $1 ORDER BY secret_name",
        )
        .bind(guild)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn dropping_a_name_removes_its_value_and_leaves_a_tombstone(pool: PgPool) {
        store_value(&pool, 1, "token", vec![1_u8; 40]).await;
        let mut transaction = pool.begin().await.unwrap();
        let removed = RemovedSecret {
            generation: crm(1),
            secret_name: "token".into(),
        };
        remove_secret_name_in_transaction(&mut transaction, &removed, true)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        assert!(stored_names(&pool, 1).await.is_empty());
        assert_eq!(tombstoned_names(&pool, 1).await, ["token"]);
    }

    #[sqlx::test]
    async fn a_new_value_clears_the_tombstone_of_its_name(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        store_value(&pool, 1, "token", vec![1_u8; 40]).await;
        let mut transaction = pool.begin().await.unwrap();
        let removed = RemovedSecret {
            generation: crm(1),
            secret_name: "token".into(),
        };
        remove_secret_name_in_transaction(&mut transaction, &removed, true)
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        let scope = IntegrationSecretScope::new(crm(1), "token").unwrap();
        assert_eq!(db.load(&scope).await.unwrap(), None);

        let sealed = SealedIntegrationSecret::from_storage(vec![1_u8; 40]).unwrap();
        let receipt = SealedIntegrationSecret::from_storage(vec![1_u8; 40]).unwrap();
        db.apply(
            &scope,
            Id::new(9),
            [1; 32],
            [2; 32],
            IntegrationSecretMutation::Set(sealed),
            receipt,
        )
        .await
        .unwrap();

        assert!(tombstoned_names(&pool, 1).await.is_empty());
        assert!(db.load(&scope).await.unwrap().is_some());
    }

    #[sqlx::test]
    async fn the_largest_version_two_envelope_fits_the_storage_limit(pool: PgPool) {
        // 2 (version and key ID) + 12 (nonce) + 16384 (secret) + 16 (tag)
        store_value(&pool, 1, "token", vec![2_u8; 16_414]).await;
        assert_eq!(stored_names(&pool, 1).await, ["token"]);
    }

    #[sqlx::test]
    async fn orphans_are_stored_values_that_no_saved_configuration_names(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        save_config_naming(&pool, 1, &["token"]).await;
        store_value(&pool, 1, "token", vec![1_u8; 40]).await;
        store_value(&pool, 1, "extra", vec![1_u8; 40]).await;

        let orphans = db
            .list_orphan_integration_secrets(Id::new(1))
            .await
            .unwrap();

        let names: Vec<&str> = orphans
            .iter()
            .map(|orphan| orphan.secret_name.as_str())
            .collect();
        assert_eq!(names, ["extra"]);
    }

    #[sqlx::test]
    async fn deleting_an_orphan_removes_only_its_value(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        save_config_naming(&pool, 1, &["token"]).await;
        store_value(&pool, 1, "token", vec![1_u8; 40]).await;
        store_value(&pool, 1, "extra", vec![1_u8; 40]).await;

        let deleted = db
            .delete_orphan_integration_secret(Id::new(1), "crm", GENERATION, "extra")
            .await
            .unwrap();

        assert!(deleted);
        assert_eq!(stored_names(&pool, 1).await, ["token"]);
        assert_eq!(tombstoned_names(&pool, 1).await, ["extra"]);
        let again = db
            .delete_orphan_integration_secret(Id::new(1), "crm", GENERATION, "extra")
            .await
            .unwrap();
        assert!(!again);
    }

    #[sqlx::test]
    async fn a_name_the_saved_configuration_lists_is_not_deleted_as_an_orphan(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        save_config_naming(&pool, 1, &["token"]).await;
        store_value(&pool, 1, "token", vec![1_u8; 40]).await;

        let refused = db
            .delete_orphan_integration_secret(Id::new(1), "crm", GENERATION, "token")
            .await;

        assert!(matches!(
            refused,
            Err(DbError::IntegrationSecretStillActive)
        ));
        assert_eq!(stored_names(&pool, 1).await, ["token"]);
    }
}
