//! The triggers of a rule: each one reads a message and says whether it trips the rule, and why.

use closure_core::event::MessageId;
use closure_sdk::text::{self, Link};
use closure_sdk::{Args, ChannelId, EventContext, MessageInfo, RoleId, UserId, duration};

use crate::config::{
    AntiraidTrigger, Attachments, Caps, CaseTrigger, CounterTrigger, Invites, LinkMode, Links,
    Patterns, Repeat, RoleList, Rule, Stickers, Target, Trigger, Words,
};
use crate::context::{Context, Source, ThreadEvent};
use crate::spam;

/// What a trigger found: a sentence in the server's language, the messages it is about, the members
/// its punishments and role changes apply to, and how many it counted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub detail: String,
    pub messages: Vec<(ChannelId, MessageId)>,
    pub targets: Vec<UserId>,
    pub count: u64,
    /// A later message of a burst that already fired: only the deletions run for it.
    pub join: bool,
}

impl Hit {
    fn of(detail: String, message: &MessageInfo) -> Self {
        Self {
            detail,
            messages: vec![(message.channel_id, message.id)],
            targets: vec![message.author.id],
            count: 1,
            join: false,
        }
    }

    /// A hit about the member of an event, and the message of it when it is a message.
    #[must_use]
    pub fn about(detail: String, context: &Context) -> Self {
        Self {
            detail,
            messages: context
                .message
                .iter()
                .map(|message| (message.channel_id, message.id))
                .collect(),
            targets: context.user_id().into_iter().collect(),
            count: 1,
            join: false,
        }
    }
}

/// What a check reads besides the message: the clock, the recent actions and whether this is the
/// real run or a dry run (which counts a burst without taking it).
pub struct Scan<'a> {
    pub ctx: &'a EventContext,
    pub recent: &'a closure_sdk::recent::RecentActions,
    pub commit: bool,
    /// Whether the message is new. Edits count for the triggers that read text, not for spam.
    pub created: bool,
}

/// Runs trigger number `index` of `rule` on a context.
pub fn check(scan: &Scan<'_>, rule: &Rule, index: usize, context: &Context) -> Option<Hit> {
    let ctx = scan.ctx;
    match &rule.triggers[index] {
        Trigger::Spam(spam) => spam::evaluate(scan, &rule.id, index, spam, context),
        Trigger::MemberJoinSpam(spam) => spam::evaluate_join(scan, &rule.id, index, spam, context),
        Trigger::MemberJoin(join) => member_join(ctx, join.only_new, join.new_threshold, context),
        Trigger::MemberLeave(_) => matches!(context.source, Source::Leave)
            .then(|| Hit::about(say(ctx, "detail-leave", &Args::new()), context)),
        Trigger::RoleAdded(list) => role_hit(ctx, list, context, true),
        Trigger::RoleRemoved(list) => role_hit(ctx, list, context, false),
        Trigger::Case(case) => case_hit(ctx, case, context, scan.ctx.bot()),
        Trigger::ThreadCreated(_) => thread_hit(ctx, context, ThreadEvent::Created),
        Trigger::ThreadArchived(_) => thread_hit(ctx, context, ThreadEvent::Archived),
        Trigger::ThreadUnarchived(_) => thread_hit(ctx, context, ThreadEvent::Unarchived),
        Trigger::ThreadDeleted(_) => thread_hit(ctx, context, ThreadEvent::Deleted),
        Trigger::Counter(counter) => counter_hit(ctx, counter, context),
        Trigger::AntiraidLevel(antiraid) => antiraid_hit(ctx, antiraid, context),
        other => {
            let message = context.message.as_ref()?;
            message_check(ctx, other, message)
        }
    }
}

/// The triggers that read the text, the attachments and the stickers of a message.
fn message_check(ctx: &EventContext, trigger: &Trigger, message: &MessageInfo) -> Option<Hit> {
    match trigger {
        Trigger::AnyMessage(_) => Some(Hit::of(say(ctx, "detail-any", &Args::new()), message)),
        Trigger::Words(words) => words_hit(ctx, words, message),
        Trigger::Regex(patterns) => regex_hit(ctx, patterns, message),
        Trigger::Links(links) => links_hit(ctx, links, message),
        Trigger::Invites(invites) => invites_hit(ctx, invites, message),
        Trigger::Attachments(files) => attachments_hit(ctx, files, message),
        Trigger::Caps(caps) => caps_hit(ctx, caps, message),
        Trigger::Repeat(repeat) => repeat_hit(ctx, repeat, message),
        Trigger::Stickers(stickers) => stickers_hit(ctx, stickers, message),
        _ => None,
    }
}

/// A member joined. With `only_new`, the account must have been made within `threshold`.
fn member_join(
    ctx: &EventContext,
    only_new: bool,
    threshold: std::time::Duration,
    context: &Context,
) -> Option<Hit> {
    if !matches!(context.source, Source::Join) {
        return None;
    }
    if !only_new {
        return Some(Hit::about(say(ctx, "detail-join", &Args::new()), context));
    }
    let made = duration::snowflake_time(context.user_id()?.get());
    let age = ctx.now() - made;
    let fresh = age >= 0 && u64::try_from(age).is_ok_and(|age| age < threshold.as_secs());
    fresh.then(|| {
        let args = Args::new().with(
            "age",
            duration::format_in(
                ctx,
                ctx.locale,
                std::time::Duration::from_secs(u64::try_from(age).unwrap_or(0)),
            ),
        );
        Hit::about(say(ctx, "detail-join-new", &args), context)
    })
}

/// Roles added to (or removed from) the member, when one of the roles of the trigger is among them.
fn role_hit(ctx: &EventContext, list: &RoleList, context: &Context, added: bool) -> Option<Hit> {
    let Source::RoleChange {
        added: gained,
        removed: taken,
    } = &context.source
    else {
        return None;
    };
    let changed = if added { gained } else { taken };
    let role = changed.iter().find(|role| list.roles.contains(role))?;
    let args = Args::new().with("role", role_mention(*role));
    let key = if added {
        "detail-role-added"
    } else {
        "detail-role-removed"
    };
    Some(Hit::about(say(ctx, key, &args), context))
}

fn role_mention(role: RoleId) -> String {
    format!("<@&{}>", role.get())
}

/// A case of one of the kinds the trigger lists; the bot's own cases only with `include_own`.
fn case_hit(
    ctx: &EventContext,
    trigger: &CaseTrigger,
    context: &Context,
    bot: Option<UserId>,
) -> Option<Hit> {
    let Source::Case(case) = &context.source else {
        return None;
    };
    if !trigger.include_own && bot == Some(case.moderator) {
        return None;
    }
    if !trigger.kinds.is_empty()
        && !trigger
            .kinds
            .iter()
            .any(|kind| kind.as_str() == case.kind.as_str())
    {
        return None;
    }
    let args = Args::new().with("kind", case.kind.as_str());
    Some(Hit::about(say(ctx, "detail-case", &args), context))
}

fn thread_hit(ctx: &EventContext, context: &Context, event: ThreadEvent) -> Option<Hit> {
    if !matches!(&context.source, Source::Thread(found) if *found == event) {
        return None;
    }
    let key = match event {
        ThreadEvent::Created => "detail-thread-created",
        ThreadEvent::Archived => "detail-thread-archived",
        ThreadEvent::Unarchived => "detail-thread-unarchived",
        ThreadEvent::Deleted => "detail-thread-deleted",
    };
    Some(Hit::about(say(ctx, key, &Args::new()), context))
}

/// A counter trigger fired (or its reverse did), as the trigger names it.
fn counter_hit(ctx: &EventContext, trigger: &CounterTrigger, context: &Context) -> Option<Hit> {
    let Source::Counter {
        counter,
        trigger: fired,
        reverse,
    } = &context.source
    else {
        return None;
    };
    if *counter != trigger.counter || *fired != trigger.trigger || *reverse != trigger.reverse {
        return None;
    }
    let args = Args::new()
        .with("counter", counter.as_str())
        .with("trigger", fired.as_str());
    Some(Hit::about(say(ctx, "detail-counter", &args), context))
}

/// The antiraid level changed to one the trigger names, from one it names.
fn antiraid_hit(ctx: &EventContext, trigger: &AntiraidTrigger, context: &Context) -> Option<Hit> {
    let Source::Antiraid { level, old } = &context.source else {
        return None;
    };
    if (!trigger.levels.is_empty() && !trigger.levels.contains(level))
        || (!trigger.from.is_empty() && !trigger.from.contains(old))
    {
        return None;
    }
    let args = Args::new()
        .with("level", level.as_str())
        .with("old", old.as_str());
    Some(Hit::about(say(ctx, "detail-antiraid", &args), context))
}

fn words_hit(ctx: &EventContext, words: &Words, message: &MessageInfo) -> Option<Hit> {
    let lowercase = !words.case_sensitive;
    for target in &words.targets {
        let text = prepared(
            target_text(*target, message),
            words.normalize,
            words.strip_markdown,
            lowercase,
        );
        for listed in &words.words {
            let word = prepared(listed, words.normalize, false, lowercase);
            if contains_word(&text, &word, words.whole_words) {
                let args = Args::new().with("word", listed.as_str());
                return Some(Hit::of(say(ctx, "detail-words", &args), message));
            }
        }
    }
    None
}

fn regex_hit(ctx: &EventContext, patterns: &Patterns, message: &MessageInfo) -> Option<Hit> {
    for target in &patterns.targets {
        // The case is the regex's own flag; `normalize` lower-cases the text on its way.
        let text = prepared(
            target_text(*target, message),
            patterns.normalize,
            patterns.strip_markdown,
            false,
        );
        if let Some(found) = patterns.compiled.iter().find(|regex| regex.is_match(&text)) {
            let args = Args::new().with("pattern", found.as_str());
            return Some(Hit::of(say(ctx, "detail-pattern", &args), message));
        }
    }
    None
}

fn links_hit(ctx: &EventContext, links: &Links, message: &MessageInfo) -> Option<Hit> {
    for link in found_links(&message.content, links.only_real_links) {
        if !links.include.is_empty() && !links.include.iter().any(|re| re.is_match(&link.url)) {
            continue;
        }
        if links.exclude.iter().any(|re| re.is_match(&link.url)) {
            continue;
        }
        let listed = links
            .domains
            .iter()
            .any(|domain| domain_matches(&link.host, domain, links.include_subdomains));
        let (trips, key) = match links.mode {
            LinkMode::Any => (true, "detail-link"),
            LinkMode::Allowlist => (!listed, "detail-link-not-allowed"),
            LinkMode::Blocklist => (listed, "detail-link-blocked"),
        };
        if trips {
            let args = Args::new().with("host", link.host.as_str());
            return Some(Hit::of(say(ctx, key, &args), message));
        }
    }
    None
}

fn invites_hit(ctx: &EventContext, invites: &Invites, message: &MessageInfo) -> Option<Hit> {
    text::invites(&message.content)
        .into_iter()
        .find(|code| invites.block_codes.contains(code) || !invites.allow_codes.contains(code))
        .map(|code| {
            let args = Args::new().with("code", code.as_str());
            Hit::of(say(ctx, "detail-invite", &args), message)
        })
}

fn attachments_hit(ctx: &EventContext, files: &Attachments, message: &MessageInfo) -> Option<Hit> {
    let count = wide(message.attachments.len());
    let too_few = files.min_count.is_some_and(|min| count >= u64::from(min));
    let too_many = files.max_count.is_some_and(|max| count > u64::from(max));
    if too_few || too_many {
        let args = Args::new().with("count", count);
        return Some(Hit::of(say(ctx, "detail-attachments", &args), message));
    }
    for file in &message.attachments {
        let extension = extension_of(&file.filename);
        let mime = file
            .content_type
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        let blocked = files
            .blocked_extensions
            .iter()
            .any(|listed| same_extension(listed, &extension))
            || files
                .blocked_mime_types
                .iter()
                .any(|pattern| mime_matches(pattern, &mime));
        let refused = (!files.allowed_extensions.is_empty()
            && !files
                .allowed_extensions
                .iter()
                .any(|listed| same_extension(listed, &extension)))
            || (!files.allowed_mime_types.is_empty()
                && !files
                    .allowed_mime_types
                    .iter()
                    .any(|pattern| mime_matches(pattern, &mime)));
        let key = if blocked {
            "detail-attachment-type"
        } else if refused {
            "detail-attachment-not-allowed"
        } else {
            continue;
        };
        let args = Args::new().with("name", file.filename.as_str());
        return Some(Hit::of(say(ctx, key, &args), message));
    }
    None
}

fn caps_hit(ctx: &EventContext, caps: &Caps, message: &MessageInfo) -> Option<Hit> {
    let min_letters = usize::try_from(caps.min_letters).unwrap_or(usize::MAX);
    text::caps_ratio(&message.content, min_letters)
        .filter(|ratio| *ratio > caps.max_ratio)
        .map(|ratio| {
            let args = Args::new().with("percent", format!("{:.0}%", ratio * 100.0));
            Hit::of(say(ctx, "detail-caps", &args), message)
        })
}

fn repeat_hit(ctx: &EventContext, repeat: &Repeat, message: &MessageInfo) -> Option<Hit> {
    let run = wide(text::longest_run(&message.content));
    (run > u64::from(repeat.max_run)).then(|| {
        let args = Args::new().with("count", run);
        Hit::of(say(ctx, "detail-repeat", &args), message)
    })
}

fn stickers_hit(ctx: &EventContext, stickers: &Stickers, message: &MessageInfo) -> Option<Hit> {
    let count = u64::from(message.sticker_count);
    (count > u64::from(stickers.max)).then(|| {
        let args = Args::new().with("count", count);
        Hit::of(say(ctx, "detail-stickers", &args), message)
    })
}

/// The text a target gives the matcher.
pub fn target_text(target: Target, message: &MessageInfo) -> &str {
    match target {
        Target::Message => &message.content,
        Target::Username => &message.author.name,
    }
}

/// The text as the matcher sees it: markup dropped when asked, then folded to lower case (or
/// folded by `normalize`, which lower-cases too).
pub fn prepared(text: &str, normalize: bool, strip_markdown: bool, lowercase: bool) -> String {
    let stripped = if strip_markdown {
        text.chars()
            .filter(|ch| !matches!(*ch, '*' | '_' | '~' | '|' | '`' | '\\'))
            .collect()
    } else {
        text.to_owned()
    };
    if normalize {
        text::normalize(&stripped)
    } else if lowercase {
        stripped.to_lowercase()
    } else {
        stripped
    }
}

/// Whether `word` is in `text`. A whole word stands with no letter or digit next to it.
pub fn contains_word(text: &str, word: &str, whole: bool) -> bool {
    if word.is_empty() {
        return false;
    }
    if !whole {
        return text.contains(word);
    }
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// The links of a text. Without `only_real`, a bare `example.org` counts too.
fn found_links(content: &str, only_real: bool) -> Vec<Link> {
    let mut found = text::links(content);
    if !only_real {
        found.extend(bare_links(content));
    }
    found
}

fn bare_links(content: &str) -> Vec<Link> {
    content
        .split(|ch: char| ch.is_whitespace() || ch == '<' || ch == '>')
        .filter(|token| !token.contains("://") && !token.to_ascii_lowercase().starts_with("www."))
        .filter_map(|token| {
            let token = token.trim_end_matches(|ch: char| ".,;:!?)]}'\"".contains(ch));
            let end = token.find(['/', ':', '?', '#']).unwrap_or(token.len());
            let host = token[..end].to_ascii_lowercase();
            is_domain_name(&host).then(|| Link {
                url: token.to_owned(),
                host,
            })
        })
        .collect()
}

/// Whether `host` looks like a domain name: at least two labels, and a letters-only ending of two
/// or more characters.
fn is_domain_name(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let labels_ok = labels.iter().all(|label| {
        !label.is_empty()
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    });
    labels.len() >= 2
        && labels_ok
        && labels
            .last()
            .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|ch| ch.is_ascii_alphabetic()))
}

/// Whether `host` is `domain` or, when `subdomains` is set, a subdomain of it.
fn domain_matches(host: &str, domain: &str, subdomains: bool) -> bool {
    let domain = domain.trim().trim_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return false;
    }
    host == domain
        || (subdomains
            && host
                .strip_suffix(domain.as_str())
                .is_some_and(|rest| rest.ends_with('.')))
}

fn extension_of(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

fn same_extension(listed: &str, extension: &str) -> bool {
    !extension.is_empty()
        && listed
            .trim_start_matches('.')
            .eq_ignore_ascii_case(extension)
}

/// Whether a content type matches a pattern: an exact type, or `type/*` for a whole family.
fn mime_matches(pattern: &str, mime: &str) -> bool {
    if mime.is_empty() {
        return false;
    }
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_suffix("/*") {
        Some(family) => mime.split('/').next() == Some(family),
        None => pattern == mime,
    }
}

fn say(ctx: &EventContext, key: &str, args: &Args) -> String {
    ctx.t_in(ctx.locale, key, args)
}

/// A count as the wide integer the messages use; a count never reaches the limit of `u64`.
pub fn wide(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The window of a spam trigger as the text says it, in the server's language.
pub fn window_text(ctx: &EventContext, window: std::time::Duration) -> String {
    duration::format_in(ctx, ctx.locale, window)
}
