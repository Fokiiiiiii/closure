//! Guild-, module-, and integration-scoped secrets encrypted at rest by the host.
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use secrecy::SecretBox;
pub use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::event::{GuildId, UserId};
use crate::module::ModuleId;
use crate::permission::Actor;

/// Dedicated host environment variable containing a 32-byte key as 64 hexadecimal characters.
pub const MASTER_KEY_ENV: &str = "INTEGRATION_SECRETS_MASTER_KEY";
const MASTER_KEY_HEX_BYTES: usize = 64;
const INTEGRATION_ID_MAX_BYTES: usize = 32;
const GENERATION_BYTES: usize = 16;
const GENERATION_HEX_BYTES: usize = GENERATION_BYTES * 2;
const SECRET_NAME_MAX_BYTES: usize = 64;
const MAX_SECRET_BYTES: usize = 16 * 1024;
const ENVELOPE_VERSION: u8 = 1;
const GCM_TAG_BYTES: usize = 16;
const ENVELOPE_HEADER_BYTES: usize = 1 + aead::NONCE_LEN;
const MAX_ENVELOPE_BYTES: usize = ENVELOPE_HEADER_BYTES + MAX_SECRET_BYTES + GCM_TAG_BYTES;
const MUTATION_SET: u8 = 1;
const MUTATION_REMOVE: u8 = 2;
const RECEIPT_VERSION: u8 = 1;
const RECEIPT_COMPLETED: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum IntegrationSecretError {
    #[error("integration secret service is unavailable")]
    Unavailable,
    #[error("integration secret scope is invalid")]
    InvalidScope,
    #[error("integration secret value is invalid")]
    InvalidSecret,
    #[error("administrator permission is required")]
    PermissionDenied,
    #[error("integration secret generation has been retired")]
    GenerationRetired,
    #[error("integration secret storage failed")]
    Storage,
    #[error("integration secret authentication failed")]
    Authentication,
    #[error("integration secrets master key is invalid")]
    InvalidMasterKey,
    #[error("integration secret idempotency key is invalid")]
    InvalidIdempotencyKey,
    #[error("integration secret idempotency key was reused for a different action or value")]
    IdempotencyConflict,
}

/// Immutable, non-secret identity for one configured integration generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationSecretGeneration {
    guild_id: GuildId,
    module: ModuleId,
    integration_id: String,
    generation: String,
}

impl IntegrationSecretGeneration {
    pub fn new(
        guild_id: GuildId,
        module: ModuleId,
        integration_id: impl Into<String>,
        generation: impl Into<String>,
    ) -> Result<Self, IntegrationSecretError> {
        let integration_id = integration_id.into();
        let generation = generation.into();
        if guild_id.get() == 0
            || module.as_str().is_empty()
            || module.as_str().len() > 128
            || module.as_str().chars().any(char::is_control)
            || !valid_integration_id(&integration_id)
            || !valid_generation(&generation)
        {
            return Err(IntegrationSecretError::InvalidScope);
        }
        Ok(Self {
            guild_id,
            module,
            integration_id,
            generation,
        })
    }

    #[must_use]
    pub fn guild_id(&self) -> GuildId {
        self.guild_id
    }

    #[must_use]
    pub fn module(&self) -> &ModuleId {
        &self.module
    }

    #[must_use]
    pub fn integration_id(&self) -> &str {
        &self.integration_id
    }

    #[must_use]
    pub fn generation(&self) -> &str {
        &self.generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationSecretScope {
    generation: IntegrationSecretGeneration,
    secret_name: String,
}

impl IntegrationSecretScope {
    pub fn new(
        generation: IntegrationSecretGeneration,
        secret_name: impl Into<String>,
    ) -> Result<Self, IntegrationSecretError> {
        let secret_name = secret_name.into();
        if !valid_secret_name(&secret_name) {
            return Err(IntegrationSecretError::InvalidScope);
        }
        Ok(Self {
            generation,
            secret_name,
        })
    }

    #[must_use]
    pub fn generation_scope(&self) -> &IntegrationSecretGeneration {
        &self.generation
    }

    #[must_use]
    pub fn secret_name(&self) -> &str {
        &self.secret_name
    }
}

/// An opaque ciphertext envelope. Its Debug output never prints stored bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedIntegrationSecret(Vec<u8>);

impl SealedIntegrationSecret {
    pub fn from_storage(bytes: Vec<u8>) -> Result<Self, IntegrationSecretError> {
        if bytes.len() < ENVELOPE_HEADER_BYTES + GCM_TAG_BYTES
            || bytes.len() > MAX_ENVELOPE_BYTES
            || bytes[0] != ENVELOPE_VERSION
        {
            return Err(IntegrationSecretError::Authentication);
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SealedIntegrationSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SealedIntegrationSecret([REDACTED])")
    }
}

/// Whether an idempotent mutation applied now or matched a stored receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyDisposition {
    Applied,
    Replayed,
}

/// The encrypted value to apply after the backend claims the idempotency key.
#[derive(Debug, Clone)]
pub enum IntegrationSecretMutation {
    Set(SealedIntegrationSecret),
    Remove,
}

/// Persistence sees ciphertext and keyed digests only. Retired generations are tombstoned.
#[async_trait]
pub trait IntegrationSecretBackend: Send + Sync {
    async fn load(
        &self,
        scope: &IntegrationSecretScope,
    ) -> Result<Option<SealedIntegrationSecret>, IntegrationSecretError>;

    async fn apply(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        request_key_hmac: [u8; 32],
        fingerprint_hmac: [u8; 32],
        mutation: IntegrationSecretMutation,
        encrypted_receipt: SealedIntegrationSecret,
    ) -> Result<IdempotencyDisposition, IntegrationSecretError>;
}

/// Encrypts secrets before persistence and decrypts them only for the owning module and guild.
pub struct IntegrationSecretService {
    key: LessSafeKey,
    idempotency_key: hmac::Key,
    account_hmac_key: hmac::Key,
    backend: Arc<dyn IntegrationSecretBackend>,
}

impl fmt::Debug for IntegrationSecretService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IntegrationSecretService([REDACTED])")
    }
}

impl IntegrationSecretService {
    /// Constructs a service from the dedicated 32-byte host key and ciphertext-only backend.
    pub fn new(
        master_key: &SecretString,
        backend: Arc<dyn IntegrationSecretBackend>,
    ) -> Result<Self, IntegrationSecretError> {
        let key_bytes = decode_master_key(master_key.expose_secret())?;
        let key_bytes = SecretBox::new(Box::new(key_bytes));
        let unbound = UnboundKey::new(&aead::AES_256_GCM, key_bytes.expose_secret())
            .map_err(|_| IntegrationSecretError::InvalidMasterKey)?;
        let master_hmac = hmac::Key::new(hmac::HMAC_SHA256, key_bytes.expose_secret());
        let idempotency_key = derive_hmac_key(
            &master_hmac,
            b"closure.integration-secret.idempotency-hmac-key.v1",
        );
        let account_hmac_key =
            derive_hmac_key(&master_hmac, b"closure.account-secret.token-hmac-key.v1");
        Ok(Self {
            key: LessSafeKey::new(unbound),
            idempotency_key,
            account_hmac_key,
            backend,
        })
    }

    pub async fn get(
        &self,
        scope: &IntegrationSecretScope,
    ) -> Result<Option<SecretString>, IntegrationSecretError> {
        let Some(sealed) = self.backend.load(scope).await? else {
            return Ok(None);
        };
        self.open(scope, sealed).map(Some)
    }

    pub async fn is_configured(
        &self,
        scope: &IntegrationSecretScope,
    ) -> Result<bool, IntegrationSecretError> {
        Ok(self.backend.load(scope).await?.is_some())
    }

    pub async fn set(
        &self,
        actor: &Actor,
        scope: &IntegrationSecretScope,
        secret: &SecretString,
        idempotency_key: &str,
    ) -> Result<(), IntegrationSecretError> {
        require_administrator(actor)?;
        if !valid_idempotency_key(idempotency_key) {
            return Err(IntegrationSecretError::InvalidIdempotencyKey);
        }
        let sealed = self.seal(scope, secret)?;
        let request_key_hmac = self.request_key_hmac(scope, actor.user, idempotency_key);
        let fingerprint_hmac = self.fingerprint_hmac(
            scope,
            actor.user,
            MUTATION_SET,
            idempotency_key,
            Some(secret.expose_secret()),
        );
        let receipt = self.seal_receipt(
            scope,
            actor.user,
            &request_key_hmac,
            &fingerprint_hmac,
            MUTATION_SET,
        )?;
        self.backend
            .apply(
                scope,
                actor.user,
                request_key_hmac,
                fingerprint_hmac,
                IntegrationSecretMutation::Set(sealed),
                receipt,
            )
            .await?;
        Ok(())
    }

    pub async fn remove(
        &self,
        actor: &Actor,
        scope: &IntegrationSecretScope,
        idempotency_key: &str,
    ) -> Result<(), IntegrationSecretError> {
        require_administrator(actor)?;
        if !valid_idempotency_key(idempotency_key) {
            return Err(IntegrationSecretError::InvalidIdempotencyKey);
        }
        let request_key_hmac = self.request_key_hmac(scope, actor.user, idempotency_key);
        let fingerprint_hmac =
            self.fingerprint_hmac(scope, actor.user, MUTATION_REMOVE, idempotency_key, None);
        let receipt = self.seal_receipt(
            scope,
            actor.user,
            &request_key_hmac,
            &fingerprint_hmac,
            MUTATION_REMOVE,
        )?;
        self.backend
            .apply(
                scope,
                actor.user,
                request_key_hmac,
                fingerprint_hmac,
                IntegrationSecretMutation::Remove,
                receipt,
            )
            .await?;
        Ok(())
    }

    /// Seals an account-owned secret; the authenticated actor must be the account owner.
    pub fn seal_account_secret(
        &self,
        actor: UserId,
        owner: UserId,
        namespace: &str,
        field: &str,
        secret: SecretString,
    ) -> Result<Vec<u8>, IntegrationSecretError> {
        require_owner(actor, owner)?;
        validate_account_scope(owner, namespace, field)?;
        let cleartext = secret.expose_secret();
        if cleartext.is_empty()
            || cleartext.len() > MAX_SECRET_BYTES
            || cleartext.chars().any(char::is_control)
        {
            return Err(IntegrationSecretError::InvalidSecret);
        }
        let aad = account_secret_associated_data(owner, namespace, field);
        Ok(self
            .seal_payload(&aad, cleartext.as_bytes())?
            .as_bytes()
            .to_vec())
    }

    /// Opens an account-owned ciphertext only for the authenticated owner and original AAD scope.
    pub fn open_account_secret(
        &self,
        actor: UserId,
        owner: UserId,
        namespace: &str,
        field: &str,
        envelope: &[u8],
    ) -> Result<SecretString, IntegrationSecretError> {
        require_owner(actor, owner)?;
        validate_account_scope(owner, namespace, field)?;
        if envelope.len() < ENVELOPE_HEADER_BYTES + GCM_TAG_BYTES
            || envelope.len() > MAX_ENVELOPE_BYTES
        {
            return Err(IntegrationSecretError::Authentication);
        }
        let sealed = SealedIntegrationSecret::from_storage(envelope.to_vec())?;
        self.open_payload(
            &account_secret_associated_data(owner, namespace, field),
            sealed,
        )
    }

    /// Computes a domain-separated keyed token digest for one account namespace.
    pub fn account_token_hmac(
        &self,
        actor: UserId,
        owner: UserId,
        namespace: &str,
        message: &[u8],
    ) -> Result<[u8; 32], IntegrationSecretError> {
        require_owner(actor, owner)?;
        if !valid_account_component(namespace) {
            return Err(IntegrationSecretError::InvalidScope);
        }
        if message.is_empty() || message.len() > MAX_SECRET_BYTES {
            return Err(IntegrationSecretError::InvalidSecret);
        }
        let mut context = hmac::Context::with_key(&self.account_hmac_key);
        context.update(b"closure.account-secret.token-hmac.v1");
        context.update(&owner.get().to_be_bytes());
        update_hmac_part(&mut context, namespace.as_bytes());
        update_hmac_part(&mut context, message);
        Ok(hmac_tag(context))
    }

    fn request_key_hmac(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        idempotency_key: &str,
    ) -> [u8; 32] {
        let mut context = hmac::Context::with_key(&self.idempotency_key);
        context.update(b"closure.integration-secret.request-key.v1\\0");
        update_scope_hmac(&mut context, scope, actor);
        update_hmac_part(&mut context, idempotency_key.as_bytes());
        hmac_tag(context)
    }

    fn fingerprint_hmac(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        action: u8,
        idempotency_key: &str,
        value: Option<&str>,
    ) -> [u8; 32] {
        let mut context = hmac::Context::with_key(&self.idempotency_key);
        context.update(b"closure.integration-secret.fingerprint.v1\\0");
        update_scope_hmac(&mut context, scope, actor);
        context.update(&[action]);
        update_hmac_part(&mut context, idempotency_key.as_bytes());
        match value {
            Some(value) => {
                context.update(&[1]);
                update_hmac_part(&mut context, value.as_bytes());
            }
            None => context.update(&[0]),
        }
        hmac_tag(context)
    }

    fn seal(
        &self,
        scope: &IntegrationSecretScope,
        secret: &SecretString,
    ) -> Result<SealedIntegrationSecret, IntegrationSecretError> {
        let cleartext = secret.expose_secret();
        if cleartext.is_empty()
            || cleartext.len() > MAX_SECRET_BYTES
            || cleartext.chars().any(char::is_control)
        {
            return Err(IntegrationSecretError::InvalidSecret);
        }
        self.seal_payload(&associated_data(scope, b"secret"), cleartext.as_bytes())
    }

    fn seal_receipt(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        request_key_hmac: &[u8; 32],
        fingerprint_hmac: &[u8; 32],
        action: u8,
    ) -> Result<SealedIntegrationSecret, IntegrationSecretError> {
        let receipt = [RECEIPT_VERSION, action, RECEIPT_COMPLETED];
        let aad = receipt_associated_data(scope, actor, request_key_hmac, fingerprint_hmac);
        self.seal_payload(&aad, &receipt)
    }

    fn seal_payload(
        &self,
        aad: &[u8],
        cleartext: &[u8],
    ) -> Result<SealedIntegrationSecret, IntegrationSecretError> {
        let mut nonce = [0_u8; aead::NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| IntegrationSecretError::Unavailable)?;
        let mut buffer = Zeroizing::new(cleartext.to_vec());
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut *buffer,
            )
            .map_err(|_| IntegrationSecretError::Authentication)?;

        let mut envelope = Vec::with_capacity(ENVELOPE_HEADER_BYTES + buffer.len());
        envelope.push(ENVELOPE_VERSION);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&buffer);
        SealedIntegrationSecret::from_storage(envelope)
    }

    fn open(
        &self,
        scope: &IntegrationSecretScope,
        secret: SealedIntegrationSecret,
    ) -> Result<SecretString, IntegrationSecretError> {
        self.open_payload(&associated_data(scope, b"secret"), secret)
    }

    fn open_payload(
        &self,
        aad: &[u8],
        secret: SealedIntegrationSecret,
    ) -> Result<SecretString, IntegrationSecretError> {
        let envelope = Zeroizing::new(secret.0);
        if envelope.len() < ENVELOPE_HEADER_BYTES + GCM_TAG_BYTES || envelope[0] != ENVELOPE_VERSION
        {
            return Err(IntegrationSecretError::Authentication);
        }
        let nonce: [u8; aead::NONCE_LEN] = envelope[1..ENVELOPE_HEADER_BYTES]
            .try_into()
            .map_err(|_| IntegrationSecretError::Authentication)?;
        let mut buffer = Zeroizing::new(envelope[ENVELOPE_HEADER_BYTES..].to_vec());
        let cleartext_len = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut *buffer,
            )
            .map_err(|_| IntegrationSecretError::Authentication)?
            .len();
        let mut cleartext = std::mem::take(&mut *buffer);
        cleartext.truncate(cleartext_len);
        match String::from_utf8(cleartext) {
            Ok(cleartext) => Ok(SecretString::from(cleartext)),
            Err(error) => {
                let mut bytes = error.into_bytes();
                bytes.zeroize();
                Err(IntegrationSecretError::Authentication)
            }
        }
    }
}

/// A fresh opaque generation for a newly created integration. Preserve it on edits and retire it
/// only after a configuration update that removes that integration has committed.
pub fn new_integration_generation() -> Result<String, IntegrationSecretError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = [0_u8; GENERATION_BYTES];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| IntegrationSecretError::Unavailable)?;
    let mut generation = String::with_capacity(GENERATION_HEX_BYTES);
    for byte in random {
        generation.push(char::from(HEX[usize::from(byte >> 4)]));
        generation.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(generation)
}

fn require_administrator(actor: &Actor) -> Result<(), IntegrationSecretError> {
    if actor.owner || actor.administrator {
        Ok(())
    } else {
        Err(IntegrationSecretError::PermissionDenied)
    }
}

fn valid_integration_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= INTEGRATION_ID_MAX_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn valid_generation(generation: &str) -> bool {
    generation.len() == GENERATION_HEX_BYTES
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= SECRET_NAME_MAX_BYTES
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

fn decode_master_key(input: &str) -> Result<[u8; 32], IntegrationSecretError> {
    if input.len() != MASTER_KEY_HEX_BYTES || !input.is_ascii() {
        return Err(IntegrationSecretError::InvalidMasterKey);
    }
    let mut key = [0_u8; 32];
    for (index, pair) in input.as_bytes().chunks_exact(2).enumerate() {
        key[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Ok(key)
}

fn hex_value(byte: u8) -> Result<u8, IntegrationSecretError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(IntegrationSecretError::InvalidMasterKey),
    }
}

fn associated_data(scope: &IntegrationSecretScope, purpose: &[u8]) -> Vec<u8> {
    let generation = scope.generation_scope();
    let mut aad = b"closure.integration-secret.aad.v1".to_vec();
    append_aad_part(&mut aad, purpose);
    aad.extend_from_slice(&generation.guild_id().get().to_be_bytes());
    for part in [
        generation.module().as_str(),
        generation.integration_id(),
        generation.generation(),
        scope.secret_name(),
    ] {
        append_aad_part(&mut aad, part.as_bytes());
    }
    aad
}

fn receipt_associated_data(
    scope: &IntegrationSecretScope,
    actor: UserId,
    request_key_hmac: &[u8; 32],
    fingerprint_hmac: &[u8; 32],
) -> Vec<u8> {
    let mut aad = associated_data(scope, b"receipt");
    aad.extend_from_slice(&actor.get().to_be_bytes());
    aad.extend_from_slice(request_key_hmac);
    aad.extend_from_slice(fingerprint_hmac);
    aad
}

fn append_aad_part(aad: &mut Vec<u8>, part: &[u8]) {
    aad.extend_from_slice(&(part.len() as u32).to_be_bytes());
    aad.extend_from_slice(part);
}

fn valid_idempotency_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn update_scope_hmac(context: &mut hmac::Context, scope: &IntegrationSecretScope, actor: UserId) {
    let generation = scope.generation_scope();
    context.update(&generation.guild_id().get().to_be_bytes());
    update_hmac_part(context, generation.module().as_str().as_bytes());
    update_hmac_part(context, generation.integration_id().as_bytes());
    update_hmac_part(context, generation.generation().as_bytes());
    update_hmac_part(context, scope.secret_name().as_bytes());
    context.update(&actor.get().to_be_bytes());
}

fn update_hmac_part(context: &mut hmac::Context, part: &[u8]) {
    context.update(&(part.len() as u32).to_be_bytes());
    context.update(part);
}

fn hmac_tag(context: hmac::Context) -> [u8; 32] {
    let tag = context.sign();
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(tag.as_ref());
    digest
}

fn derive_hmac_key(master: &hmac::Key, domain: &[u8]) -> hmac::Key {
    let derived = hmac::sign(master, domain);
    hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref())
}

fn require_owner(actor: UserId, owner: UserId) -> Result<(), IntegrationSecretError> {
    if owner.get() != 0 && actor == owner {
        Ok(())
    } else {
        Err(IntegrationSecretError::PermissionDenied)
    }
}

fn validate_account_scope(
    owner: UserId,
    namespace: &str,
    field: &str,
) -> Result<(), IntegrationSecretError> {
    if owner.get() == 0 || !valid_account_component(namespace) || !valid_account_component(field) {
        Err(IntegrationSecretError::InvalidScope)
    } else {
        Ok(())
    }
}

fn valid_account_component(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn account_secret_associated_data(owner: UserId, namespace: &str, field: &str) -> Vec<u8> {
    let mut aad = b"closure.account-secret.aad.v1".to_vec();
    aad.extend_from_slice(&owner.get().to_be_bytes());
    append_aad_part(&mut aad, namespace.as_bytes());
    append_aad_part(&mut aad, field.as_bytes());
    aad
}
