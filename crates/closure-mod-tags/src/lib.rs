//! Reusable, per-guild messages stored in module KV.
use closure_sdk::{
    AutocompleteCx, Choice, Command, CommandArgs, CommandCx, CoreError, Expect, Level, Locale,
    ModuleBuilder, PermissionSpec, SdkModule, StorageError, UserError,
};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
const TAG_PREFIX: &str = "tag:";
const MAX_TAGS: usize = 100;
const MAX_NAME_LENGTH: usize = 32;
const MAX_TEXT_LENGTH: usize = 2000;

#[derive(CommandArgs)]
struct TagName {
    /// The tag name.
    #[arg(max_length = 32)]
    name: String,
}

#[derive(CommandArgs)]
struct AddTag {
    /// The tag name.
    #[arg(max_length = 32)]
    name: String,
    /// Text to save.
    #[arg(min_length = 1, max_length = 2000)]
    text: String,
}

fn normalized_name(name: &str) -> Result<String, UserError> {
    let name = name.to_ascii_lowercase();
    if name.is_empty()
        || name.len() > MAX_NAME_LENGTH
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    {
        return Err(UserError::message("tag-invalid-name"));
    }
    Ok(name)
}

fn key(name: &str) -> String {
    format!("{TAG_PREFIX}{name}")
}

async fn show(cx: CommandCx, args: TagName) -> Result<(), UserError> {
    let name = normalized_name(&args.name)?;
    let Some(text) = cx.store().get::<String>(&key(&name)).await? else {
        return Err(UserError::message("tag-not-found"));
    };
    cx.reply(text).await
}

async fn add(cx: CommandCx, args: AddTag) -> Result<(), UserError> {
    let name = normalized_name(&args.name)?;
    if args.text.is_empty() || args.text.chars().count() > MAX_TEXT_LENGTH {
        return Err(UserError::message("tag-invalid-text"));
    }
    let key = key(&name);
    if cx.store().get::<String>(&key).await?.is_some() {
        return Err(UserError::message("tag-exists"));
    }
    // shortcut: concurrent adds can race this count; use a guild-scoped CAS reservation if observed.
    if cx.store().list(TAG_PREFIX, MAX_TAGS + 1).await?.len() >= MAX_TAGS {
        return Err(UserError::message("tag-limit"));
    }
    cx.store()
        .replace(&key, &args.text, Expect::Missing, None)
        .await
        .map_err(|error| match error {
            StorageError::Conflict => UserError::message("tag-exists"),
            error => error.into(),
        })?;
    cx.reply_t("tag-added", &closure_sdk::Args::new().with("name", name))
        .await
}

async fn remove(cx: CommandCx, args: TagName) -> Result<(), UserError> {
    let name = normalized_name(&args.name)?;
    if !cx.store().delete(&key(&name)).await? {
        return Err(UserError::message("tag-not-found"));
    }
    cx.reply_t("tag-removed", &closure_sdk::Args::new().with("name", name))
        .await
}

async fn list(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let mut names: Vec<String> = cx
        .store()
        .list(TAG_PREFIX, MAX_TAGS + 1)
        .await?
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix(TAG_PREFIX).map(str::to_owned))
        .collect();
    names.sort_unstable();
    if names.is_empty() {
        return cx
            .reply_t("tag-list-empty", &closure_sdk::Args::new())
            .await;
    }
    for chunk in names.chunks(60) {
        cx.reply(chunk.join("\n")).await?;
    }
    Ok(())
}

async fn name_choices(cx: AutocompleteCx) -> Result<Vec<Choice>, UserError> {
    let query = cx.focused().to_ascii_lowercase();
    let entries = cx.store().list(TAG_PREFIX, MAX_TAGS).await?;
    let mut names: Vec<String> = entries
        .into_iter()
        .filter_map(|(key, _)| key.strip_prefix(TAG_PREFIX).map(str::to_owned))
        .filter(|name| name.starts_with(&query))
        .collect();
    names.sort_unstable();
    Ok(names
        .into_iter()
        .take(25)
        .map(|name| Choice::new(name.clone(), name))
        .collect())
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("tags", "Tags", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::SendMessages)
            .permission(PermissionSpec::new(
                "tags.use",
                Level::GENERAL,
                "permission-tags-use",
            )?)
            .permission(PermissionSpec::new(
                "tags.manage",
                Level::MODERATOR,
                "permission-tags-manage",
            )?)
            .group(
                "tag",
                [
                    Command::<TagName>::new("show")
                        .requires("tags.use")
                        .handler(show),
                    Command::<AddTag>::new("add")
                        .requires("tags.manage")
                        .handler(add),
                    Command::<TagName>::new("remove")
                        .requires("tags.manage")
                        .handler(remove),
                    Command::<()>::new("list")
                        .requires("tags.use")
                        .handler(list),
                ],
            )
            .autocomplete("tag show", "name", name_choices)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
