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
               AND NOT g.retired",
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
            }
        }

        transaction
            .commit()
            .await
            .map_err(|_| IntegrationSecretError::Storage)?;
        Ok(IdempotencyDisposition::Applied)
    }
}

fn constant_time_eq(left: &[u8], right: &[u8; 32]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(&right[..]))
}
