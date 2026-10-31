//! Spam triggers: what a member sent within a window, measured and compared with an amount, and
//! acted on once per burst.
//!
//! Each spam trigger of a rule keeps its own kind in [`RecentActions`], so that one rule's burst
//! never takes another rule's messages. A message is recorded before any rule is checked, so the
//! message that trips a burst counts too.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use closure_sdk::recent::{Burst, Claim, Recent, RecentFilter};
use closure_sdk::text;
use closure_sdk::{Args, MessageInfo, UserId};

use crate::config::{JoinSpam, Measure, Rule, Spam, Trigger};
use crate::context::{Context, Source};
use crate::triggers::{Hit, Scan, window_text};

/// The kind a spam trigger is recorded under.
pub fn kind(rule: &str, index: usize) -> String {
    format!("{rule}#{index}")
}

/// Records what each windowed spam trigger of an in-scope rule counts in the context: a message
/// for the message triggers, a join for the join triggers.
pub fn record(scan: &Scan<'_>, rule: &Rule, context: &Context, now: i64) {
    let guild = scan.ctx.guild_id;
    for (index, trigger) in rule.triggers.iter().enumerate() {
        match trigger {
            Trigger::Spam(spam) => {
                // A single message is measured where it is; nothing needs keeping.
                let Some(message) = context.message.as_ref() else {
                    continue;
                };
                if spam.within.is_none() || !context.created {
                    continue;
                }
                let count = measure(spam.measure, message);
                if count == 0 {
                    continue;
                }
                scan.recent.record(
                    guild,
                    Recent {
                        kind: kind(&rule.id, index),
                        subject: message.author.id.get(),
                        group: Some(message.channel_id.get().to_string()),
                        count,
                        at: now,
                        messages: vec![(message.channel_id, message.id)],
                        tag: fingerprint(&message.content),
                    },
                );
            }
            Trigger::MemberJoinSpam(_) => {
                let (Source::Join, Some(who)) = (&context.source, context.user.as_ref()) else {
                    continue;
                };
                scan.recent.record(
                    guild,
                    Recent {
                        kind: kind(&rule.id, index),
                        subject: who.id.get(),
                        group: None,
                        count: 1,
                        at: now,
                        messages: Vec::new(),
                        tag: None,
                    },
                );
            }
            _ => {}
        }
    }
}

/// Decides whether a spam trigger trips on `message`. Only a new message counts.
///
/// A burst that tripped marks the member's key as handled for the window: until it passes, the
/// messages of that member join the burst (they are deleted, with no second punishment).
pub fn evaluate(
    scan: &Scan<'_>,
    rule: &str,
    index: usize,
    spam: &Spam,
    context: &Context,
) -> Option<Hit> {
    let message = context.message.as_ref()?;
    if !scan.created {
        return None;
    }
    let ctx = scan.ctx;
    let Some(within) = spam.within else {
        let count = u64::from(measure(spam.measure, message));
        if count < u64::from(spam.amount) {
            return None;
        }
        let args = Args::new()
            .with("what", measure_label(ctx, spam.measure))
            .with("count", count);
        let detail = ctx.t_in(ctx.locale, "detail-spam-message", &args);
        return Some(Hit {
            detail,
            messages: vec![(message.channel_id, message.id)],
            targets: vec![message.author.id],
            count,
            join: false,
        });
    };

    let kind = kind(rule, index);
    let author = message.author.id;
    let key = handled_key(&kind, message, spam.per_channel);
    let now = ctx.now();
    let filter = RecentFilter {
        group: spam
            .per_channel
            .then(|| message.channel_id.get().to_string()),
        distinct_groups_at_least: spam
            .distinct_channels
            .map(|distinct| usize::try_from(distinct).unwrap_or(usize::MAX)),
        distinct_subjects_at_least: None,
    };
    let burst = Burst {
        kind: &kind,
        subject: Some(author.get()),
        within,
        filter: &filter,
        handled_key: &key,
        handled_until: now + i64::try_from(within.as_secs()).unwrap_or(i64::MAX),
    };
    let counts = |entries: &[&Recent]| {
        let count = tally(entries, spam);
        (count >= u64::from(spam.amount)).then_some(count)
    };
    // Checked and taken under one lock, so that two evaluations of the same burst cannot both fire.
    let claim = if scan.commit {
        scan.recent
            .claim_burst(message.guild_id, now, &burst, counts)
    } else {
        scan.recent
            .inspect_burst(message.guild_id, now, &burst, counts)
    };
    match claim {
        Claim::Handled => Some(Hit {
            detail: ctx.t_in(ctx.locale, "detail-burst", &Args::new()),
            messages: vec![(message.channel_id, message.id)],
            targets: vec![author],
            count: 1,
            join: true,
        }),
        Claim::Short => None,
        Claim::Counted { count, entries } => {
            let messages = entries
                .iter()
                .flat_map(|entry| entry.messages.iter().copied())
                .collect();
            let args = Args::new()
                .with("what", measure_label(ctx, spam.measure))
                .with("count", count)
                .with("window", window_text(ctx, within));
            Some(Hit {
                detail: ctx.t_in(ctx.locale, "detail-spam", &args),
                messages,
                targets: vec![author],
                count,
                join: false,
            })
        }
    }
}

/// Members who joined within the window, counted together. Once a burst has fired, each member who
/// joins while it is handled is a target of its own; the burst is handled until the window passes.
pub fn evaluate_join(
    scan: &Scan<'_>,
    rule: &str,
    index: usize,
    spam: &JoinSpam,
    context: &Context,
) -> Option<Hit> {
    let (Source::Join, Some(who)) = (&context.source, context.user.as_ref()) else {
        return None;
    };
    let ctx = scan.ctx;
    let guild = ctx.guild_id;
    let kind = kind(rule, index);
    let now = ctx.now();
    let filter = RecentFilter::default();
    let burst = Burst {
        kind: &kind,
        subject: None,
        within: spam.within,
        filter: &filter,
        handled_key: &kind,
        handled_until: now + i64::try_from(spam.within.as_secs()).unwrap_or(i64::MAX),
    };
    let counts = |entries: &[&Recent]| {
        let count: u64 = entries.iter().map(|entry| u64::from(entry.count)).sum();
        (count >= u64::from(spam.amount)).then_some(count)
    };
    let claim = if scan.commit {
        scan.recent.claim_burst(guild, now, &burst, counts)
    } else {
        scan.recent.inspect_burst(guild, now, &burst, counts)
    };
    match claim {
        Claim::Handled => {
            // The burst counted the members who joined before it fired, and punished them with it.
            // Only a join recorded after that is punished on its own.
            let joined_after = !scan
                .recent
                .entries(guild, &kind, Some(who.id.get()), spam.within, now, &filter)
                .is_empty();
            joined_after.then(|| Hit {
                detail: ctx.t_in(ctx.locale, "detail-join-burst", &Args::new()),
                messages: Vec::new(),
                targets: vec![who.id],
                count: 1,
                join: false,
            })
        }
        Claim::Short => None,
        Claim::Counted { count, entries } => {
            let mut targets: Vec<UserId> = Vec::new();
            for entry in &entries {
                if let Some(user) = UserId::new_checked(entry.subject)
                    && !targets.contains(&user)
                {
                    targets.push(user);
                }
            }
            let args = Args::new()
                .with("count", count)
                .with("window", window_text(ctx, spam.within));
            Some(Hit {
                detail: ctx.t_in(ctx.locale, "detail-join-spam", &args),
                messages: Vec::new(),
                targets,
                count,
                join: false,
            })
        }
    }
}

/// What a message measures, for the measure of its kind.
pub fn measure(measure: Measure, message: &MessageInfo) -> u32 {
    let value = match measure {
        Measure::Messages | Measure::Duplicates => 1,
        Measure::Mentions => text::mention_count(message),
        Measure::Links => text::links(&message.content).len(),
        Measure::Attachments => message.attachments.len(),
        Measure::Emojis => text::emoji_count(&message.content),
        Measure::Lines => message.content.lines().count(),
        Measure::Characters => message.content.chars().count(),
        Measure::Stickers => usize::try_from(message.sticker_count).unwrap_or(usize::MAX),
    };
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The count of a window: the sum of its measures, after the options that drop or refuse some.
fn tally(entries: &[&Recent], spam: &Spam) -> u64 {
    // A message without text matches no other message, not even one without text.
    let bodyless = entries.iter().filter(|e| e.tag.is_none()).count();
    if spam.group_identical_messages {
        let texts: HashSet<&str> = entries.iter().filter_map(|e| e.tag.as_deref()).collect();
        if texts.len() + bodyless != 1 {
            return 0;
        }
    }
    if spam.measure == Measure::Duplicates {
        let mut copies: HashMap<&str, u64> = HashMap::new();
        for tag in entries.iter().filter_map(|e| e.tag.as_deref()) {
            *copies.entry(tag).or_default() += 1;
        }
        let most = copies.values().copied().max().unwrap_or(0);
        return most.max(u64::from(bodyless > 0));
    }
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter(|entry| {
            !spam.ignore_identical_messages
                || entry
                    .tag
                    .as_ref()
                    .is_none_or(|tag| seen.insert(tag.as_str()))
        })
        .map(|entry| u64::from(entry.count))
        .sum()
}

/// A fingerprint of a text as the window compares it: the words, single-spaced. Empty texts have
/// none, so they are never identical to each other.
fn fingerprint(content: &str) -> Option<String> {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    normalized.hash(&mut hasher);
    Some(format!("{:x}", hasher.finish()))
}

fn handled_key(kind: &str, message: &MessageInfo, per_channel: bool) -> String {
    if per_channel {
        format!(
            "{kind}:{}:{}",
            message.author.id.get(),
            message.channel_id.get()
        )
    } else {
        format!("{kind}:{}", message.author.id.get())
    }
}

fn measure_label(ctx: &closure_sdk::EventContext, measure: Measure) -> String {
    let key = match measure {
        Measure::Messages => "measure-messages",
        Measure::Mentions => "measure-mentions",
        Measure::Links => "measure-links",
        Measure::Attachments => "measure-attachments",
        Measure::Emojis => "measure-emojis",
        Measure::Lines => "measure-lines",
        Measure::Characters => "measure-characters",
        Measure::Stickers => "measure-stickers",
        Measure::Duplicates => "measure-duplicates",
    };
    ctx.t_in(ctx.locale, key, &Args::new())
}
