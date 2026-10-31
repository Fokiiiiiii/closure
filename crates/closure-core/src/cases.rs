//! Records of moderation (warnings, timeouts, kicks, bans, ...), numbered per guild.
//!
//! The numbers belong to the guild, not to a module: every module that holds the capability
//! `cases` writes into the same sequence, so modules agree on a number without calling each
//! other. A module holds a [`Cases`] bound to itself and the guild of the event and sees only
//! that guild's cases. A case is never removed; it can be revoked, once.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::{Capability, CapabilitySet};
use crate::event::{GuildId, UserId};
use crate::module::ModuleId;

/// The longest reason, in characters.
pub const MAX_REASON_CHARS: usize = 512;
/// The most cases one listing returns.
pub const MAX_PAGE: usize = 25;
/// How many cases a guild may have.
pub const MAX_CASES: i64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CaseError {
    #[error("the module is not granted the capability `cases`")]
    NotGranted,
    #[error("invalid kind: {0}")]
    InvalidKind(String),
    #[error("invalid reason: {0}")]
    InvalidReason(String),
    #[error("no such case")]
    NotFound,
    #[error("the case is already revoked")]
    AlreadyRevoked,
    #[error("the guild already has {limit} cases")]
    QuotaExceeded { limit: i64 },
    #[error("no case records are available here")]
    Unavailable,
    #[error("case records failed: {0}")]
    Backend(String),
}

/// What a case is about, such as `warn`, `timeout` or `kick`: 1 to 32 of `a-z`, `0-9`, `_` and `-`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CaseKind(String);

impl CaseKind {
    pub fn new(kind: &str) -> Result<Self, CaseError> {
        let valid = (1..=32).contains(&kind.len())
            && kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if valid {
            Ok(Self(kind.to_owned()))
        } else {
            Err(CaseError::InvalidKind(kind.to_owned()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CaseKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CaseKind {
    type Error = CaseError;

    fn try_from(kind: String) -> Result<Self, Self::Error> {
        Self::new(&kind)
    }
}

impl From<CaseKind> for String {
    fn from(kind: CaseKind) -> Self {
        kind.0
    }
}

/// A case to record. Its module and guild come from the [`Cases`] handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewCase {
    pub kind: CaseKind,
    pub target: UserId,
    pub moderator: UserId,
    pub reason: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<i64>,
}

/// Who revoked a case, when and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    pub by: UserId,
    /// Unix seconds.
    pub at: i64,
    pub reason: Option<String>,
}

/// A recorded case. Its `number` is unique within its guild, from 1 on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    pub number: i64,
    pub guild_id: GuildId,
    /// The module that recorded the case.
    pub module: ModuleId,
    pub kind: CaseKind,
    pub target: UserId,
    pub moderator: UserId,
    pub reason: Option<String>,
    /// Unix seconds, by the clock of the backend.
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked: Option<Revoked>,
}

/// Where cases are kept. Every call is for one guild; the checks of [`Cases`] come first.
#[async_trait]
pub trait CaseBackend: Send + Sync {
    /// Records the case under the next number of the guild, or fails with
    /// [`CaseError::QuotaExceeded`] (taking no number).
    async fn create(
        &self,
        module: &ModuleId,
        guild: GuildId,
        case: NewCase,
    ) -> Result<Case, CaseError>;

    async fn get(&self, guild: GuildId, number: i64) -> Result<Option<Case>, CaseError>;

    /// The cases of `target`, newest first; with `before`, only those numbered below it.
    async fn list_for_target(
        &self,
        guild: GuildId,
        target: UserId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError>;

    /// Every case of the guild, newest first; with `before`, only those numbered below it.
    async fn list_recent(
        &self,
        guild: GuildId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError>;

    /// Revokes the case if it is not revoked yet, and returns it as revoked.
    async fn revoke(
        &self,
        guild: GuildId,
        number: i64,
        by: UserId,
        reason: Option<String>,
    ) -> Result<Case, CaseError>;

    /// The cases of `kind` against `target` that stand at `now` (unix seconds): not revoked, and
    /// not past their expiry. Newest first, at most `limit`. The default pages through
    /// [`Self::list_for_target`]; a backend that can filter in its own query should override it.
    async fn list_standing(
        &self,
        guild: GuildId,
        target: UserId,
        kind: &str,
        now: i64,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        let mut found = Vec::new();
        let mut before = None;
        while found.len() < limit {
            let page = self
                .list_for_target(guild, target, before, MAX_PAGE)
                .await?;
            before = page.last().map(|case| case.number);
            found.extend(page.iter().filter(|case| stands(case, kind, now)).cloned());
            if page.len() < MAX_PAGE {
                break;
            }
        }
        found.truncate(limit);
        Ok(found)
    }
}

/// The backend of a [`Cases`] that has none.
struct NoCases;

#[async_trait]
impl CaseBackend for NoCases {
    async fn create(&self, _: &ModuleId, _: GuildId, _: NewCase) -> Result<Case, CaseError> {
        Err(CaseError::Unavailable)
    }

    async fn get(&self, _: GuildId, _: i64) -> Result<Option<Case>, CaseError> {
        Err(CaseError::Unavailable)
    }

    async fn list_for_target(
        &self,
        _: GuildId,
        _: UserId,
        _: Option<i64>,
        _: usize,
    ) -> Result<Vec<Case>, CaseError> {
        Err(CaseError::Unavailable)
    }

    async fn list_recent(
        &self,
        _: GuildId,
        _: Option<i64>,
        _: usize,
    ) -> Result<Vec<Case>, CaseError> {
        Err(CaseError::Unavailable)
    }

    async fn revoke(
        &self,
        _: GuildId,
        _: i64,
        _: UserId,
        _: Option<String>,
    ) -> Result<Case, CaseError> {
        Err(CaseError::Unavailable)
    }
}

/// A module's way to record and read cases, in one guild.
#[derive(Clone)]
pub struct Cases {
    backend: Arc<dyn CaseBackend>,
    granted: bool,
    module: ModuleId,
    guild: GuildId,
}

impl fmt::Debug for Cases {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cases")
            .field("module", &self.module)
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

/// PostgreSQL cannot keep a NUL character in text, so no backend takes one.
fn check_reason(reason: Option<&str>) -> Result<(), CaseError> {
    match reason {
        Some(text) if text.chars().count() > MAX_REASON_CHARS => Err(CaseError::InvalidReason(
            format!("at most {MAX_REASON_CHARS} characters"),
        )),
        Some(text) if text.contains('\0') => {
            Err(CaseError::InvalidReason("it must not contain NUL".into()))
        }
        _ => Ok(()),
    }
}

impl Cases {
    /// The cases of `module` in `guild`; usable only if `granted` holds [`Capability::Cases`].
    #[must_use]
    pub fn new(
        backend: Arc<dyn CaseBackend>,
        granted: &CapabilitySet,
        module: ModuleId,
        guild: GuildId,
    ) -> Self {
        Self {
            backend,
            granted: granted.contains(Capability::Cases),
            module,
            guild,
        }
    }

    /// Cases that fail every call, for contexts that have no case records.
    #[must_use]
    pub fn unavailable(module: ModuleId, guild: GuildId) -> Self {
        Self {
            backend: Arc::new(NoCases),
            granted: true,
            module,
            guild,
        }
    }

    fn allowed(&self) -> Result<(), CaseError> {
        if self.granted {
            Ok(())
        } else {
            Err(CaseError::NotGranted)
        }
    }

    /// Records a case under the next number of the guild.
    pub async fn create(&self, case: NewCase) -> Result<Case, CaseError> {
        self.allowed()?;
        check_reason(case.reason.as_deref())?;
        self.backend.create(&self.module, self.guild, case).await
    }

    /// The case with `number`, whichever module recorded it.
    pub async fn get(&self, number: i64) -> Result<Option<Case>, CaseError> {
        self.allowed()?;
        self.backend.get(self.guild, number).await
    }

    /// Up to `limit` (1 to [`MAX_PAGE`]) cases of `target`, newest first, below `before` if given.
    pub async fn for_target(
        &self,
        target: UserId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        self.allowed()?;
        self.backend
            .list_for_target(self.guild, target, before, limit.clamp(1, MAX_PAGE))
            .await
    }

    /// Up to `limit` (1 to [`MAX_PAGE`]) cases of the guild, newest first, below `before` if given.
    pub async fn recent(&self, before: Option<i64>, limit: usize) -> Result<Vec<Case>, CaseError> {
        self.allowed()?;
        self.backend
            .list_recent(self.guild, before, limit.clamp(1, MAX_PAGE))
            .await
    }

    /// Revokes the case with `number`; only once.
    pub async fn revoke(
        &self,
        number: i64,
        by: UserId,
        reason: Option<String>,
    ) -> Result<Case, CaseError> {
        self.allowed()?;
        check_reason(reason.as_deref())?;
        self.backend.revoke(self.guild, number, by, reason).await
    }

    /// Every case of `kind` against `target` that stands at `now` (unix seconds), newest first, at
    /// most `limit`. Unlike [`Self::for_target`], the limit is not capped at [`MAX_PAGE`]: this
    /// checks a state, it does not show a page.
    pub async fn standing(
        &self,
        target: UserId,
        kind: &str,
        now: i64,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        self.allowed()?;
        self.backend
            .list_standing(self.guild, target, kind, now, limit)
            .await
    }
}

/// Whether `case` stands at `now`: of `kind`, not revoked, and not past its expiry.
fn stands(case: &Case, kind: &str, now: i64) -> bool {
    case.kind.as_str() == kind
        && case.revoked.is_none()
        && case.expires_at.is_none_or(|at| at > now)
}

#[derive(Debug, Default)]
struct GuildCases {
    /// The number handed out last; the next case gets the one after it.
    last: i64,
    by_number: HashMap<i64, Case>,
}

type UnixClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// [`CaseBackend`] in memory, with the same limits as the real one. For tests and for running
/// without a database.
pub struct MemoryCases {
    clock: UnixClock,
    limit: i64,
    guilds: Mutex<HashMap<GuildId, GuildCases>>,
}

impl fmt::Debug for MemoryCases {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryCases").finish_non_exhaustive()
    }
}

impl Default for MemoryCases {
    fn default() -> Self {
        Self::with_clock(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
                })
        })
    }
}

impl MemoryCases {
    /// Cases whose idea of the time (unix seconds) is `clock`, so tests can move it.
    #[must_use]
    pub fn with_clock(clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self {
            clock: Arc::new(clock),
            limit: MAX_CASES,
            guilds: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn with_limit(clock: impl Fn() -> i64 + Send + Sync + 'static, limit: i64) -> Self {
        Self {
            limit,
            ..Self::with_clock(clock)
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, GuildCases>> {
        self.guilds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn newest(
        &self,
        guild: GuildId,
        before: Option<i64>,
        limit: usize,
        keep: impl Fn(&Case) -> bool,
    ) -> Vec<Case> {
        self.lock()
            .get(&guild)
            .map(|cases| {
                let mut found: Vec<&Case> = cases
                    .by_number
                    .values()
                    .filter(|case| before.is_none_or(|before| case.number < before) && keep(case))
                    .collect();
                found.sort_by_key(|case| std::cmp::Reverse(case.number));
                found.into_iter().take(limit).cloned().collect()
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl CaseBackend for MemoryCases {
    async fn create(
        &self,
        module: &ModuleId,
        guild: GuildId,
        case: NewCase,
    ) -> Result<Case, CaseError> {
        let now = (self.clock)();
        let mut guilds = self.lock();
        let cases = guilds.entry(guild).or_default();
        let number = cases.last + 1;
        if number > self.limit {
            return Err(CaseError::QuotaExceeded { limit: self.limit });
        }
        cases.last = number;
        let stored = Case {
            number,
            guild_id: guild,
            module: module.clone(),
            kind: case.kind,
            target: case.target,
            moderator: case.moderator,
            reason: case.reason,
            created_at: now,
            expires_at: case.expires_at,
            revoked: None,
        };
        cases.by_number.insert(number, stored.clone());
        Ok(stored)
    }

    async fn get(&self, guild: GuildId, number: i64) -> Result<Option<Case>, CaseError> {
        Ok(self
            .lock()
            .get(&guild)
            .and_then(|cases| cases.by_number.get(&number))
            .cloned())
    }

    async fn list_for_target(
        &self,
        guild: GuildId,
        target: UserId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        Ok(self.newest(guild, before, limit, |case| case.target == target))
    }

    async fn list_recent(
        &self,
        guild: GuildId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        Ok(self.newest(guild, before, limit, |_| true))
    }

    async fn revoke(
        &self,
        guild: GuildId,
        number: i64,
        by: UserId,
        reason: Option<String>,
    ) -> Result<Case, CaseError> {
        let now = (self.clock)();
        let mut guilds = self.lock();
        let case = guilds
            .get_mut(&guild)
            .and_then(|cases| cases.by_number.get_mut(&number))
            .ok_or(CaseError::NotFound)?;
        if case.revoked.is_some() {
            return Err(CaseError::AlreadyRevoked);
        }
        case.revoked = Some(Revoked {
            by,
            at: now,
            reason,
        });
        Ok(case.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use twilight_model::id::Id;

    use super::*;

    fn module(name: &str) -> ModuleId {
        ModuleId::new(name).unwrap()
    }

    fn kind() -> CaseKind {
        CaseKind::new("warn").unwrap()
    }

    fn new_case(target: u64, reason: Option<&str>) -> NewCase {
        NewCase {
            kind: kind(),
            target: Id::new(target),
            moderator: Id::new(9),
            reason: reason.map(str::to_owned),
            expires_at: None,
        }
    }

    fn granted() -> CapabilitySet {
        [Capability::Cases].into_iter().collect()
    }

    fn numbers(cases: &[Case]) -> Vec<i64> {
        cases.iter().map(|case| case.number).collect()
    }

    struct Fixture {
        backend: Arc<MemoryCases>,
        now: Arc<AtomicI64>,
    }

    fn fixture() -> Fixture {
        let now = Arc::new(AtomicI64::new(1_700_000_000));
        let clock = Arc::clone(&now);
        Fixture {
            backend: Arc::new(MemoryCases::with_clock(move || {
                clock.load(Ordering::SeqCst)
            })),
            now,
        }
    }

    impl Fixture {
        fn cases(&self, module_name: &str, guild: u64) -> Cases {
            Cases::new(
                Arc::clone(&self.backend) as Arc<dyn CaseBackend>,
                &granted(),
                module(module_name),
                Id::new(guild),
            )
        }

        fn advance(&self, seconds: i64) {
            self.now.fetch_add(seconds, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn numbers_count_from_one_per_guild_and_modules_share_them() {
        let f = fixture();
        let (warn, mute, elsewhere) = (f.cases("warn", 1), f.cases("mute", 1), f.cases("warn", 2));
        assert_eq!(warn.create(new_case(5, None)).await.unwrap().number, 1);
        assert_eq!(mute.create(new_case(5, None)).await.unwrap().number, 2);
        assert_eq!(warn.create(new_case(5, None)).await.unwrap().number, 3);
        assert_eq!(elsewhere.create(new_case(5, None)).await.unwrap().number, 1);
        // The case another module made is there to read.
        assert_eq!(warn.get(2).await.unwrap().unwrap().module, module("mute"));
        assert_eq!(warn.get(4).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_case_is_read_back_whole_with_the_clock_time() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        let created = warn
            .create(NewCase {
                kind: CaseKind::new("timeout").unwrap(),
                target: Id::new(5),
                moderator: Id::new(9),
                reason: Some("spam".into()),
                expires_at: Some(1_700_600_000),
            })
            .await
            .unwrap();
        assert_eq!(
            created,
            Case {
                number: 1,
                guild_id: Id::new(1),
                module: module("warn"),
                kind: CaseKind::new("timeout").unwrap(),
                target: Id::new(5),
                moderator: Id::new(9),
                reason: Some("spam".into()),
                created_at: 1_700_000_000,
                expires_at: Some(1_700_600_000),
                revoked: None,
            }
        );
        assert_eq!(warn.get(1).await.unwrap(), Some(created));
    }

    #[tokio::test]
    async fn for_target_lists_newest_first_and_pages_with_before() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        for target in [5, 7, 5, 5, 7, 5] {
            warn.create(new_case(target, None)).await.unwrap();
        }
        let first = warn.for_target(Id::new(5), None, 2).await.unwrap();
        assert_eq!(numbers(&first), [6, 4]);
        let second = warn
            .for_target(Id::new(5), first.last().map(|case| case.number), 2)
            .await
            .unwrap();
        assert_eq!(numbers(&second), [3, 1]);
        assert!(
            warn.for_target(Id::new(5), Some(1), 2)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            numbers(&warn.for_target(Id::new(7), None, 10).await.unwrap()),
            [5, 2]
        );
    }

    #[tokio::test]
    async fn recent_lists_everyones_cases_newest_first() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        for target in [5, 6, 7] {
            warn.create(new_case(target, None)).await.unwrap();
        }
        assert_eq!(numbers(&warn.recent(None, 10).await.unwrap()), [3, 2, 1]);
        assert_eq!(numbers(&warn.recent(Some(3), 10).await.unwrap()), [2, 1]);
        assert_eq!(numbers(&warn.recent(Some(2), 1).await.unwrap()), [1]);
    }

    #[tokio::test]
    async fn a_case_is_revoked_once_and_keeps_the_first_revocation() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        warn.create(new_case(5, None)).await.unwrap();
        f.advance(60);
        let revoked = warn
            .revoke(1, Id::new(9), Some("mistake".into()))
            .await
            .unwrap();
        let expected = Some(Revoked {
            by: Id::new(9),
            at: 1_700_000_060,
            reason: Some("mistake".into()),
        });
        assert_eq!(revoked.revoked, expected);
        assert_eq!(warn.get(1).await.unwrap().unwrap().revoked, expected);
        assert_eq!(
            warn.revoke(1, Id::new(10), None).await,
            Err(CaseError::AlreadyRevoked)
        );
        assert_eq!(
            warn.revoke(2, Id::new(10), None).await,
            Err(CaseError::NotFound)
        );
        assert_eq!(warn.get(1).await.unwrap().unwrap().revoked, expected);
    }

    #[tokio::test]
    async fn reasons_are_at_most_512_characters_and_contain_no_nul() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        assert!(
            warn.create(new_case(5, Some(&"a".repeat(512))))
                .await
                .is_ok()
        );
        assert!(
            warn.create(new_case(5, Some(&"é".repeat(512))))
                .await
                .is_ok()
        );
        assert!(matches!(
            warn.create(new_case(5, Some(&"a".repeat(513)))).await,
            Err(CaseError::InvalidReason(_))
        ));
        assert!(matches!(
            warn.create(new_case(5, Some("a\0b"))).await,
            Err(CaseError::InvalidReason(_))
        ));
        // A refused case takes no number.
        assert_eq!(warn.create(new_case(5, None)).await.unwrap().number, 3);
        assert!(matches!(
            warn.revoke(1, Id::new(9), Some("x".repeat(513))).await,
            Err(CaseError::InvalidReason(_))
        ));
        assert!(matches!(
            warn.revoke(1, Id::new(9), Some("a\0".into())).await,
            Err(CaseError::InvalidReason(_))
        ));
        assert!(warn.revoke(1, Id::new(9), None).await.is_ok());
    }

    #[test]
    fn kinds_are_1_to_32_lowercase_letters_digits_underscores_or_dashes() {
        for bad in ["", "Warn", "warn!", "has space", &"k".repeat(33)] {
            assert!(
                matches!(CaseKind::new(bad), Err(CaseError::InvalidKind(_))),
                "{bad:?}"
            );
        }
        assert_eq!(
            CaseKind::new(&"k".repeat(32)).unwrap().to_string().len(),
            32
        );
        assert!(CaseKind::new("auto-mod_1").is_ok());
        assert_eq!(
            serde_json::to_string(&CaseKind::new("warn").unwrap()).unwrap(),
            "\"warn\""
        );
        assert!(serde_json::from_str::<CaseKind>("\"WARN\"").is_err());
    }

    #[tokio::test]
    async fn without_the_capability_every_call_is_refused() {
        let f = fixture();
        f.cases("warn", 1).create(new_case(5, None)).await.unwrap();
        let none = Cases::new(
            Arc::clone(&f.backend) as Arc<dyn CaseBackend>,
            &CapabilitySet::new(),
            module("warn"),
            Id::new(1),
        );
        assert_eq!(
            none.create(new_case(5, None)).await,
            Err(CaseError::NotGranted)
        );
        assert_eq!(none.get(1).await, Err(CaseError::NotGranted));
        assert_eq!(
            none.for_target(Id::new(5), None, 10).await,
            Err(CaseError::NotGranted)
        );
        assert_eq!(none.recent(None, 10).await, Err(CaseError::NotGranted));
        assert_eq!(
            none.revoke(1, Id::new(9), None).await,
            Err(CaseError::NotGranted)
        );
    }

    #[tokio::test]
    async fn unavailable_fails_with_unavailable() {
        let cases = Cases::unavailable(module("warn"), Id::new(1));
        assert_eq!(
            cases.create(new_case(5, None)).await,
            Err(CaseError::Unavailable)
        );
        assert_eq!(cases.get(1).await, Err(CaseError::Unavailable));
        assert_eq!(cases.recent(None, 10).await, Err(CaseError::Unavailable));
    }

    #[tokio::test]
    async fn page_sizes_are_clamped_to_one_through_max_page() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        for _ in 0..30 {
            warn.create(new_case(5, None)).await.unwrap();
        }
        assert_eq!(warn.for_target(Id::new(5), None, 0).await.unwrap().len(), 1);
        assert_eq!(
            warn.for_target(Id::new(5), None, 1000).await.unwrap().len(),
            MAX_PAGE
        );
        assert_eq!(warn.recent(None, 1000).await.unwrap().len(), MAX_PAGE);
    }

    #[tokio::test]
    async fn the_guild_quota_is_a_hard_limit_per_guild() {
        let backend = Arc::new(MemoryCases::with_limit(|| 1_700_000_000, 2));
        let cases = |guild: u64| {
            Cases::new(
                Arc::clone(&backend) as Arc<dyn CaseBackend>,
                &granted(),
                module("warn"),
                Id::new(guild),
            )
        };
        let (first, other) = (cases(1), cases(2));
        first.create(new_case(5, None)).await.unwrap();
        first.create(new_case(5, None)).await.unwrap();
        assert_eq!(
            first.create(new_case(5, None)).await,
            Err(CaseError::QuotaExceeded { limit: 2 })
        );
        assert_eq!(other.create(new_case(5, None)).await.unwrap().number, 1);
    }

    #[tokio::test]
    async fn cases_of_another_guild_cannot_be_read() {
        let f = fixture();
        f.cases("warn", 1).create(new_case(5, None)).await.unwrap();
        let elsewhere = f.cases("warn", 2);
        assert_eq!(elsewhere.get(1).await.unwrap(), None);
        assert!(elsewhere.recent(None, 10).await.unwrap().is_empty());
        assert!(
            elsewhere
                .for_target(Id::new(5), None, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn standing_finds_the_cases_of_one_kind_that_have_not_ended_however_old() {
        let f = fixture();
        let warn = f.cases("warn", 1);
        let ban = |expires_at: Option<i64>| NewCase {
            kind: CaseKind::new("ban").unwrap(),
            target: Id::new(5),
            moderator: Id::new(9),
            reason: None,
            expires_at,
        };
        warn.create(ban(Some(1_700_000_100))).await.unwrap();
        warn.create(ban(Some(1_699_999_999))).await.unwrap();
        warn.create(ban(None)).await.unwrap();
        warn.create(new_case(5, None)).await.unwrap();
        warn.create(ban(None)).await.unwrap();
        warn.revoke(3, Id::new(9), None).await.unwrap();

        let standing = warn
            .standing(Id::new(5), "ban", 1_700_000_000, 10)
            .await
            .unwrap();
        assert_eq!(numbers(&standing), [5, 1]);
        let newest = warn
            .standing(Id::new(5), "ban", 1_700_000_000, 1)
            .await
            .unwrap();
        assert_eq!(numbers(&newest), [5]);
        assert!(
            warn.standing(Id::new(6), "ban", 1_700_000_000, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
