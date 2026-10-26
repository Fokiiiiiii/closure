//! Spam triggers: what a member sent within a window, measured and compared with an amount, and
//! acted on once per burst.
//!
//! Each spam trigger of a rule keeps its own kind in [`RecentActions`], so that one rule's burst
//! never takes another rule's messages. A message is recorded before any rule is checked, so the
//! message that trips a burst counts too.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use closure_sdk::recent::{Recent, RecentFilter};
use closure_sdk::text;
use closure_sdk::{Args, MessageInfo};

use crate::config::{Measure, Rule, Spam, Trigger};
use crate::triggers::{Hit, Scan, window_text};

/// The kind a spam trigger is recorded under.
pub fn kind(rule: &str, index: usize) -> String {
    format!("{rule}#{index}")
}

/// Records what each windowed spam trigger of an in-scope rule measures in `message`.
pub fn record(scan: &Scan<'_>, rule: &Rule, message: &MessageInfo, now: i64) {
    for (index, trigger) in rule.triggers.iter().enumerate() {
        let Trigger::Spam(spam) = trigger else {
            continue;
        };
        // A single message is measured where it is; nothing needs keeping.
        if spam.within.is_none() {
            continue;
        }
        let count = measure(spam.measure, message);
        if count == 0 {
            continue;
        }
        scan.recent.record(
            message.guild_id,
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
}

/// Decides whether a spam trigger trips on `message`. Only a new message counts.
///
/// A burst that tripped marks the member's key as handled for the window: until it passes, the
/// messages of that member join the burst (they are deleted, with no second punishment), as
/// Zeppelin's spam trigger does.
pub fn evaluate(
    scan: &Scan<'_>,
    rule: &str,
    index: usize,
    spam: &Spam,
    message: &MessageInfo,
) -> Option<Hit> {
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
            count,
            join: false,
        });
    };

    let kind = kind(rule, index);
    let author = message.author.id;
    let key = handled_key(&kind, message, spam.per_channel);
    let now = ctx.now();
    if scan.recent.already_handled(message.guild_id, &key, now) {
        return Some(Hit {
            detail: ctx.t_in(ctx.locale, "detail-burst", &Args::new()),
            messages: vec![(message.channel_id, message.id)],
            count: 1,
            join: true,
        });
    }

    let filter = RecentFilter {
        group: spam
            .per_channel
            .then(|| message.channel_id.get().to_string()),
        distinct_groups_at_least: spam
            .distinct_channels
            .map(|distinct| usize::try_from(distinct).unwrap_or(usize::MAX)),
        distinct_subjects_at_least: None,
    };
    let entries = scan.recent.entries(
        message.guild_id,
        &kind,
        Some(author.get()),
        within,
        now,
        &filter,
    );
    let count = tally(&entries, spam);
    if count < u64::from(spam.amount) {
        return None;
    }
    let messages: Vec<_> = entries
        .iter()
        .flat_map(|entry| entry.messages.iter().copied())
        .collect();
    if scan.commit {
        // Taken, so that the next burst starts from nothing; marked, so that the messages that
        // follow join this one.
        let _ = scan.recent.take(
            message.guild_id,
            &kind,
            Some(author.get()),
            within,
            now,
            &filter,
        );
        let until = now + i64::try_from(within.as_secs()).unwrap_or(i64::MAX);
        scan.recent.mark_handled(message.guild_id, &key, until);
    }
    let args = Args::new()
        .with("what", measure_label(ctx, spam.measure))
        .with("count", count)
        .with("window", window_text(ctx, within));
    Some(Hit {
        detail: ctx.t_in(ctx.locale, "detail-spam", &args),
        messages,
        count,
        join: false,
    })
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
fn tally(entries: &[Recent], spam: &Spam) -> u64 {
    if spam.group_identical_messages {
        let texts: HashSet<&str> = entries.iter().filter_map(|e| e.tag.as_deref()).collect();
        if texts.len() != 1 {
            return 0;
        }
    }
    if spam.measure == Measure::Duplicates {
        let mut copies: HashMap<&str, u64> = HashMap::new();
        for tag in entries.iter().filter_map(|e| e.tag.as_deref()) {
            *copies.entry(tag).or_default() += 1;
        }
        return copies.values().copied().max().unwrap_or(0);
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
