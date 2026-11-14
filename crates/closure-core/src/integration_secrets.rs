//! Guild-, module-, and integration-scoped secrets encrypted at rest by the host.
use std::collections::BTreeMap;
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
/// Version 1 envelopes: version, nonce, ciphertext and tag. They were all written under the one key.
const ENVELOPE_VERSION_KEYLESS: u8 = 1;
/// Version 2 envelopes: version, key ID, nonce, ciphertext and tag.
const ENVELOPE_VERSION_KEYED: u8 = 2;
/// The ID new envelopes are sealed under. Only this key exists today; rotation is not implemented.
const CURRENT_KEY_ID: u8 = 1;
const GCM_TAG_BYTES: usize = 16;
const MIN_ENVELOPE_BYTES: usize = 1 + aead::NONCE_LEN + GCM_TAG_BYTES;
const MAX_ENVELOPE_BYTES: usize = 2 + aead::NONCE_LEN + MAX_SECRET_BYTES + GCM_TAG_BYTES;
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
        parse_envelope(&bytes)?;
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
    keys: BTreeMap<u8, LessSafeKey>,
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
            keys: BTreeMap::from([(CURRENT_KEY_ID, LessSafeKey::new(unbound))]),
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
        let request_key_hmac = self.request_key_hmac(scope, actor.user, idempotency_key)?;
        let fingerprint_hmac = self.fingerprint_hmac(
            scope,
            actor.user,
            MUTATION_SET,
            idempotency_key,
            Some(secret.expose_secret()),
        )?;
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
        let request_key_hmac = self.request_key_hmac(scope, actor.user, idempotency_key)?;
        let fingerprint_hmac =
            self.fingerprint_hmac(scope, actor.user, MUTATION_REMOVE, idempotency_key, None)?;
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
        let aad = account_secret_associated_data(owner, namespace, field)?;
        let sealed = self.seal_payload(&aad, cleartext.as_bytes())?;
        let ciphertext = sealed.as_bytes().to_vec();
        drop(secret);
        Ok(ciphertext)
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
        let sealed = SealedIntegrationSecret::from_storage(envelope.to_vec())?;
        let aad = account_secret_associated_data(owner, namespace, field)?;
        self.open_payload(&aad, sealed)
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
        update_hmac_part(&mut context, namespace.as_bytes())?;
        update_hmac_part(&mut context, message)?;
        Ok(hmac_tag(context))
    }

    fn request_key_hmac(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        idempotency_key: &str,
    ) -> Result<[u8; 32], IntegrationSecretError> {
        let mut context = hmac::Context::with_key(&self.idempotency_key);
        context.update(b"closure.integration-secret.request-key.v1\\0");
        update_scope_hmac(&mut context, scope, actor)?;
        update_hmac_part(&mut context, idempotency_key.as_bytes())?;
        Ok(hmac_tag(context))
    }

    fn fingerprint_hmac(
        &self,
        scope: &IntegrationSecretScope,
        actor: UserId,
        action: u8,
        idempotency_key: &str,
        value: Option<&str>,
    ) -> Result<[u8; 32], IntegrationSecretError> {
        let mut context = hmac::Context::with_key(&self.idempotency_key);
        context.update(b"closure.integration-secret.fingerprint.v1\\0");
        update_scope_hmac(&mut context, scope, actor)?;
        context.update(&[action]);
        update_hmac_part(&mut context, idempotency_key.as_bytes())?;
        match value {
            Some(value) => {
                context.update(&[1]);
                update_hmac_part(&mut context, value.as_bytes())?;
            }
            None => context.update(&[0]),
        }
        Ok(hmac_tag(context))
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
        let aad = associated_data(scope, b"secret")?;
        self.seal_payload(&aad, cleartext.as_bytes())
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
        let aad = receipt_associated_data(scope, actor, request_key_hmac, fingerprint_hmac)?;
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
        let key = self
            .keys
            .get(&CURRENT_KEY_ID)
            .ok_or(IntegrationSecretError::Unavailable)?;
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut *buffer,
        )
        .map_err(|_| IntegrationSecretError::Authentication)?;

        let mut envelope = Vec::with_capacity(2 + aead::NONCE_LEN + buffer.len());
        envelope.push(ENVELOPE_VERSION_KEYED);
        envelope.push(CURRENT_KEY_ID);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&buffer);
        SealedIntegrationSecret::from_storage(envelope)
    }

    fn open(
        &self,
        scope: &IntegrationSecretScope,
        secret: SealedIntegrationSecret,
    ) -> Result<SecretString, IntegrationSecretError> {
        let aad = associated_data(scope, b"secret")?;
        self.open_payload(&aad, secret)
    }

    fn open_payload(
        &self,
        aad: &[u8],
        secret: SealedIntegrationSecret,
    ) -> Result<SecretString, IntegrationSecretError> {
        let envelope = Zeroizing::new(secret.0);
        let parts = parse_envelope(&envelope)?;
        let key = self
            .keys
            .get(&parts.key_id)
            .ok_or(IntegrationSecretError::Authentication)?;
        let nonce: [u8; aead::NONCE_LEN] = parts
            .nonce
            .try_into()
            .map_err(|_| IntegrationSecretError::Authentication)?;
        let mut buffer = Zeroizing::new(parts.ciphertext.to_vec());
        let cleartext_len = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut buffer,
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

/// The parts of a stored envelope, borrowed from it. Only versions 1 and 2 are known.
struct EnvelopeParts<'a> {
    key_id: u8,
    nonce: &'a [u8],
    ciphertext: &'a [u8],
}

fn parse_envelope(bytes: &[u8]) -> Result<EnvelopeParts<'_>, IntegrationSecretError> {
    if bytes.len() < MIN_ENVELOPE_BYTES || bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(IntegrationSecretError::Authentication);
    }
    // Version 1 has no key ID byte, so its nonce starts one byte earlier than version 2's.
    let (key_id, nonce_start) = match bytes[0] {
        ENVELOPE_VERSION_KEYLESS => (CURRENT_KEY_ID, 1),
        ENVELOPE_VERSION_KEYED => (bytes[1], 2),
        _ => return Err(IntegrationSecretError::Authentication),
    };
    let body_start = nonce_start + aead::NONCE_LEN;
    if bytes.len() < body_start + GCM_TAG_BYTES {
        return Err(IntegrationSecretError::Authentication);
    }
    Ok(EnvelopeParts {
        key_id,
        nonce: &bytes[nonce_start..body_start],
        ciphertext: &bytes[body_start..],
    })
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
    for (index, pair) in input.as_bytes().as_chunks::<2>().0.iter().enumerate() {
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

fn associated_data(
    scope: &IntegrationSecretScope,
    purpose: &[u8],
) -> Result<Vec<u8>, IntegrationSecretError> {
    let generation = scope.generation_scope();
    let mut aad = b"closure.integration-secret.aad.v1".to_vec();
    append_aad_part(&mut aad, purpose)?;
    aad.extend_from_slice(&generation.guild_id().get().to_be_bytes());
    for part in [
        generation.module().as_str(),
        generation.integration_id(),
        generation.generation(),
        scope.secret_name(),
    ] {
        append_aad_part(&mut aad, part.as_bytes())?;
    }
    Ok(aad)
}

fn receipt_associated_data(
    scope: &IntegrationSecretScope,
    actor: UserId,
    request_key_hmac: &[u8; 32],
    fingerprint_hmac: &[u8; 32],
) -> Result<Vec<u8>, IntegrationSecretError> {
    let mut aad = associated_data(scope, b"receipt")?;
    aad.extend_from_slice(&actor.get().to_be_bytes());
    aad.extend_from_slice(request_key_hmac);
    aad.extend_from_slice(fingerprint_hmac);
    Ok(aad)
}

fn append_aad_part(aad: &mut Vec<u8>, part: &[u8]) -> Result<(), IntegrationSecretError> {
    let length = u32::try_from(part.len()).map_err(|_| IntegrationSecretError::InvalidScope)?;
    aad.extend_from_slice(&length.to_be_bytes());
    aad.extend_from_slice(part);
    Ok(())
}

fn valid_idempotency_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn update_scope_hmac(
    context: &mut hmac::Context,
    scope: &IntegrationSecretScope,
    actor: UserId,
) -> Result<(), IntegrationSecretError> {
    let generation = scope.generation_scope();
    context.update(&generation.guild_id().get().to_be_bytes());
    update_hmac_part(context, generation.module().as_str().as_bytes())?;
    update_hmac_part(context, generation.integration_id().as_bytes())?;
    update_hmac_part(context, generation.generation().as_bytes())?;
    update_hmac_part(context, scope.secret_name().as_bytes())?;
    context.update(&actor.get().to_be_bytes());
    Ok(())
}

fn update_hmac_part(
    context: &mut hmac::Context,
    part: &[u8],
) -> Result<(), IntegrationSecretError> {
    let length = u32::try_from(part.len()).map_err(|_| IntegrationSecretError::InvalidSecret)?;
    context.update(&length.to_be_bytes());
    context.update(part);
    Ok(())
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

fn account_secret_associated_data(
    owner: UserId,
    namespace: &str,
    field: &str,
) -> Result<Vec<u8>, IntegrationSecretError> {
    let mut aad = b"closure.account-secret.aad.v1".to_vec();
    aad.extend_from_slice(&owner.get().to_be_bytes());
    append_aad_part(&mut aad, namespace.as_bytes())?;
    append_aad_part(&mut aad, field.as_bytes())?;
    Ok(aad)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MASTER_KEY: &str =
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const TEST_GENERATION: &str = "0123456789abcdef0123456789abcdef";

    struct NoBackend;

    #[async_trait]
    impl IntegrationSecretBackend for NoBackend {
        async fn load(
            &self,
            _scope: &IntegrationSecretScope,
        ) -> Result<Option<SealedIntegrationSecret>, IntegrationSecretError> {
            Err(IntegrationSecretError::Unavailable)
        }

        async fn apply(
            &self,
            _scope: &IntegrationSecretScope,
            _actor: UserId,
            _request_key_hmac: [u8; 32],
            _fingerprint_hmac: [u8; 32],
            _mutation: IntegrationSecretMutation,
            _encrypted_receipt: SealedIntegrationSecret,
        ) -> Result<IdempotencyDisposition, IntegrationSecretError> {
            Err(IntegrationSecretError::Unavailable)
        }
    }

    fn service() -> IntegrationSecretService {
        let master = SecretString::from(String::from(TEST_MASTER_KEY));
        IntegrationSecretService::new(&master, Arc::new(NoBackend)).unwrap()
    }

    fn scope(secret_name: &str) -> IntegrationSecretScope {
        let generation = IntegrationSecretGeneration::new(
            GuildId::new(1),
            ModuleId::new("tickets").unwrap(),
            "crm",
            TEST_GENERATION,
        )
        .unwrap();
        IntegrationSecretScope::new(generation, secret_name).unwrap()
    }

    /// The envelope written before the key ID existed: version 1, nonce, ciphertext and tag.
    fn version_one_envelope(
        scope: &IntegrationSecretScope,
        cleartext: &[u8],
    ) -> SealedIntegrationSecret {
        let key_bytes = decode_master_key(TEST_MASTER_KEY).unwrap();
        let key = LessSafeKey::new(UnboundKey::new(&aead::AES_256_GCM, &key_bytes).unwrap());
        let nonce = [7_u8; aead::NONCE_LEN];
        let aad = associated_data(scope, b"secret").unwrap();
        let mut buffer = cleartext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad.as_slice()),
            &mut buffer,
        )
        .unwrap();
        let mut envelope = vec![1_u8];
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&buffer);
        SealedIntegrationSecret::from_storage(envelope).unwrap()
    }

    /// A version 2 envelope sealed under the test key, so only the key ID decides whether it opens.
    fn version_two_envelope(
        scope: &IntegrationSecretScope,
        key_id: u8,
        cleartext: &[u8],
    ) -> SealedIntegrationSecret {
        let key_bytes = decode_master_key(TEST_MASTER_KEY).unwrap();
        let key = LessSafeKey::new(UnboundKey::new(&aead::AES_256_GCM, &key_bytes).unwrap());
        let nonce = [9_u8; aead::NONCE_LEN];
        let aad = associated_data(scope, b"secret").unwrap();
        let mut buffer = cleartext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad.as_slice()),
            &mut buffer,
        )
        .unwrap();
        let mut envelope = vec![2_u8, key_id];
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&buffer);
        SealedIntegrationSecret::from_storage(envelope).unwrap()
    }

    #[test]
    fn version_one_envelopes_stored_before_the_key_id_still_open() {
        let service = service();
        let scope = scope("token");
        let sealed = version_one_envelope(&scope, b"old-value");
        let opened = service.open(&scope, sealed).unwrap();
        assert_eq!(opened.expose_secret(), "old-value");
    }

    #[test]
    fn new_envelopes_are_version_two_with_the_current_key_id() {
        let service = service();
        let scope = scope("token");
        let sealed = service
            .seal(&scope, &SecretString::from(String::from("new-value")))
            .unwrap();
        assert_eq!(sealed.as_bytes()[0], 2);
        assert_eq!(sealed.as_bytes()[1], 1);
        let opened = service.open(&scope, sealed).unwrap();
        assert_eq!(opened.expose_secret(), "new-value");
    }

    #[test]
    fn an_unknown_key_id_is_rejected() {
        let service = service();
        let scope = scope("token");
        let forged = version_two_envelope(&scope, 9, b"new-value");
        assert!(matches!(
            service.open(&scope, forged),
            Err(IntegrationSecretError::Authentication)
        ));
    }

    #[test]
    fn a_secret_opened_under_another_scope_is_rejected() {
        let service = service();
        let sealed = service
            .seal(&scope("token"), &SecretString::from(String::from("value")))
            .unwrap();
        assert!(matches!(
            service.open(&scope("extra"), sealed),
            Err(IntegrationSecretError::Authentication)
        ));
    }

    #[test]
    fn an_unknown_envelope_version_is_rejected() {
        let mut bytes = vec![3_u8];
        bytes.extend_from_slice(&[0_u8; 40]);
        assert!(matches!(
            SealedIntegrationSecret::from_storage(bytes),
            Err(IntegrationSecretError::Authentication)
        ));
    }
}
