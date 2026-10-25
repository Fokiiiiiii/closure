//! Plain-text helpers for modules that read messages: normalising, matching words, finding
//! links, invites, mentions, caps, repeats and emoji. Pure functions without I/O. They
//! approximate what people see; they are not a full Unicode implementation.

use std::collections::HashSet;

use closure_core::event::MessageInfo;

/// Lower-cases `text` and folds it to the forms people type: fullwidth ASCII becomes ASCII,
/// ideographic space becomes a space, format characters (zero-width, soft hyphen, bidi
/// controls) and combining marks (U+0300-U+036F) are removed, and runs of whitespace become one
/// space, with the ends trimmed.
///
/// This is not full Unicode NFKC: other compatibility forms are left as they are.
///
/// ```
/// assert_eq!(closure_sdk::text::normalize("ＨＥＬＬＯ\u{200B} \u{3000} World"), "hello world");
/// ```
#[must_use]
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if is_ignorable(ch) {
            continue;
        }
        let ch = fold_fullwidth(ch);
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        // Only between two words: this drops the leading and trailing whitespace.
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.extend(ch.to_lowercase());
    }
    out
}

fn is_ignorable(ch: char) -> bool {
    is_combining(ch)
        || matches!(
            ch,
            '\u{00AD}' // soft hyphen
            | '\u{200B}'..='\u{200F}' // zero-width characters and direction marks
            | '\u{202A}'..='\u{202E}' // bidi embedding and override controls
            | '\u{2060}'..='\u{2064}' // word joiner and invisible operators
            | '\u{FEFF}' // byte order mark
        )
}

fn is_combining(ch: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&ch)
}

fn fold_fullwidth(ch: char) -> char {
    match ch {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(u32::from(ch) - 0xFEE0).unwrap_or(ch),
        _ => ch,
    }
}

/// How a listed word has to occur in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// The word occurs anywhere, also inside a longer word.
    Contains,
    /// The word occurs with no letter or digit right before or after it. Japanese has no
    /// spaces, so a Japanese word inside a longer run of Japanese does not match.
    Whole,
}

/// Words to look for in a text. Words and text are both [`normalize`]d, so case and fullwidth
/// forms do not matter; the word returned is the one as it was listed.
///
/// ```
/// use closure_sdk::text::{MatchMode, WordList};
///
/// let list = WordList::new(["ass"], MatchMode::Whole);
/// assert_eq!(list.find("what an ASS!"), Some("ass"));
/// assert_eq!(list.find("class"), None);
/// ```
#[derive(Debug, Clone)]
pub struct WordList {
    mode: MatchMode,
    words: Vec<Word>,
}

#[derive(Debug, Clone)]
struct Word {
    listed: String,
    normalized: String,
    /// The KMP failure table of `normalized`: overlapping occurrences are found in one pass.
    failure: Vec<usize>,
}

impl WordList {
    /// Builds the list. Words that normalise to nothing are ignored; a word that repeats an
    /// earlier one (after normalisation) keeps the first spelling only.
    #[must_use]
    pub fn new(words: impl IntoIterator<Item = impl AsRef<str>>, mode: MatchMode) -> Self {
        let mut seen = HashSet::new();
        let words = words
            .into_iter()
            .filter_map(|word| {
                let normalized = normalize(word.as_ref());
                if normalized.is_empty() || !seen.insert(normalized.clone()) {
                    return None;
                }
                Some(Word {
                    listed: word.as_ref().to_owned(),
                    failure: failure_table(normalized.as_bytes()),
                    normalized,
                })
            })
            .collect();
        Self { mode, words }
    }

    /// The first listed word, in list order, that the text contains.
    #[must_use]
    pub fn find(&self, text: &str) -> Option<&str> {
        let text = normalize(text);
        self.words
            .iter()
            .find(|word| self.occurs(word, &text))
            .map(|word| word.listed.as_str())
    }

    /// Every listed word, in list order, that the text contains.
    #[must_use]
    pub fn find_all(&self, text: &str) -> Vec<&str> {
        let text = normalize(text);
        self.words
            .iter()
            .filter(|word| self.occurs(word, &text))
            .map(|word| word.listed.as_str())
            .collect()
    }

    fn occurs(&self, word: &Word, text: &str) -> bool {
        match self.mode {
            MatchMode::Contains => text.contains(word.normalized.as_str()),
            MatchMode::Whole => contains_whole(text, word),
        }
    }
}

/// Whether `word` occurs in `text` with no letter or digit next to it. Every occurrence is
/// checked, overlapping ones included, in one pass over the text.
fn contains_whole(text: &str, word: &Word) -> bool {
    let pattern = word.normalized.as_bytes();
    let mut matched = 0;
    for (index, &byte) in text.as_bytes().iter().enumerate() {
        while matched > 0 && pattern[matched] != byte {
            matched = word.failure[matched - 1];
        }
        if pattern[matched] == byte {
            matched += 1;
        }
        if matched == pattern.len() {
            // Both ends of a match are character boundaries: the pattern is valid UTF-8.
            let start = index + 1 - pattern.len();
            if is_boundary_before(text, start) && is_boundary_after(text, index + 1) {
                return true;
            }
            matched = word.failure[matched - 1];
        }
    }
    false
}

fn is_boundary_before(text: &str, start: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|ch| !ch.is_alphanumeric())
}

fn is_boundary_after(text: &str, end: usize) -> bool {
    text[end..]
        .chars()
        .next()
        .is_none_or(|ch| !ch.is_alphanumeric())
}

/// For each prefix of `pattern`, the length of its longest proper prefix that is also a suffix.
fn failure_table(pattern: &[u8]) -> Vec<usize> {
    let mut table = vec![0; pattern.len()];
    let mut k = 0;
    for (i, &byte) in pattern.iter().enumerate().skip(1) {
        while k > 0 && byte != pattern[k] {
            k = table[k - 1];
        }
        if byte == pattern[k] {
            k += 1;
        }
        table[i] = k;
    }
    table
}

/// A web link in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The link as written, without trailing punctuation or closing brackets.
    pub url: String,
    /// The host in lower case, without userinfo, port or trailing dot.
    pub host: String,
}

impl Link {
    /// Whether the host is `domain` or a subdomain of it: `example.com` matches
    /// `shop.example.com` but not `evil-example.com`. Case does not matter.
    #[must_use]
    pub fn is_domain(&self, domain: &str) -> bool {
        let domain = domain.trim_matches('.').to_lowercase();
        self.host == domain
            || self
                .host
                .strip_suffix(domain.as_str())
                .is_some_and(|rest| rest.ends_with('.'))
    }
}

const LINK_STARTS: [&str; 3] = ["https://", "http://", "www."];

/// The web links in `text`: `http://` and `https://` URLs and `www.` addresses, including the
/// `<...>` form that hides the preview. Each whitespace-delimited run gives at most one link.
///
/// ```
/// let found = closure_sdk::text::links("<https://Shop.Example.com/x>, www.example.org.");
/// assert_eq!(found[0].url, "https://Shop.Example.com/x");
/// assert!(found[0].is_domain("example.com"));
/// assert_eq!(found[1].host, "www.example.org");
/// ```
#[must_use]
pub fn links(text: &str) -> Vec<Link> {
    text.split(|ch: char| ch.is_whitespace() || ch == '<' || ch == '>')
        .filter_map(link_in)
        .collect()
}

fn link_in(token: &str) -> Option<Link> {
    let lower = token.to_ascii_lowercase();
    let start = link_start(&lower)?;
    let url = token[start..].trim_end_matches(is_trailing_punctuation);
    let host = host_of(url)?;
    Some(Link {
        url: url.to_owned(),
        host,
    })
}

/// The position of the first link start that is not in the middle of a word.
fn link_start(lower: &str) -> Option<usize> {
    LINK_STARTS
        .iter()
        .filter_map(|marker| {
            lower
                .match_indices(marker)
                .map(|(at, _)| at)
                .find(|&at| !lower[..at].ends_with(char::is_alphanumeric))
        })
        .min()
}

fn is_trailing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ','
            | ';'
            | ':'
            | '!'
            | '?'
            | '\''
            | '"'
            | '*'
            | '_'
            | '~'
            | ')'
            | ']'
            | '}'
            | '。'
            | '、'
            | '！'
            | '？'
            | '）'
            | '】'
            | '」'
            | '』'
    )
}

/// The host of a link: the part after the scheme, before any path, without userinfo or port.
fn host_of(url: &str) -> Option<String> {
    let rest = if url.to_ascii_lowercase().starts_with("http") {
        url.split_once("://")?.1
    } else {
        url
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host_and_port)| host_and_port);
    let host = if host_and_port.starts_with('[') {
        host_and_port.split_inclusive(']').next()?
    } else {
        host_and_port.split(':').next()?
    };
    let host = host.trim_end_matches('.').to_lowercase();
    (!host.is_empty()).then_some(host)
}

const INVITE_PREFIXES: [&str; 3] = [
    "discord.gg/",
    "discord.com/invite/",
    "discordapp.com/invite/",
];

/// The invite codes in `text`: `discord.gg/<code>`, `discord.com/invite/<code>` and
/// `discordapp.com/invite/<code>`, with or without a scheme. Codes keep their case, come in
/// order of appearance, and each is listed once.
///
/// ```
/// assert_eq!(closure_sdk::text::invites("join discord.gg/Abc1 now"), ["Abc1"]);
/// ```
#[must_use]
pub fn invites(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut found = Vec::new();
    for prefix in INVITE_PREFIXES {
        for (at, _) in lower.match_indices(prefix) {
            if lower[..at].ends_with(|ch: char| ch.is_alphanumeric() || ch == '-') {
                continue;
            }
            let code: String = text[at + prefix.len()..]
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
                .collect();
            if !code.is_empty() {
                found.push((at, code));
            }
        }
    }
    found.sort_by_key(|(at, _)| *at);
    let mut seen = HashSet::new();
    let mut codes = Vec::new();
    for (_, code) in found {
        if seen.insert(code.clone()) {
            codes.push(code);
        }
    }
    codes
}

/// How many people and roles a message mentions, plus one for `@everyone`. Each user and each
/// role counts once however often it is mentioned.
///
/// ```
/// # fn demo(message: &closure_sdk::MessageInfo) -> usize {
/// closure_sdk::text::mention_count(message)
/// # }
/// ```
#[must_use]
pub fn mention_count(message: &MessageInfo) -> usize {
    let users: HashSet<_> = message.mentioned_users.iter().collect();
    let roles: HashSet<_> = message.mentioned_roles.iter().collect();
    users.len() + roles.len() + usize::from(message.mentions_everyone)
}

/// The share of upper-case letters among the cased letters (those with an upper or lower case
/// form). `None` when there are fewer than `min_letters` of them, or none at all: Japanese,
/// digits and symbols have no case.
///
/// ```
/// assert_eq!(closure_sdk::text::caps_ratio("HELLO world", 5), Some(0.5));
/// assert_eq!(closure_sdk::text::caps_ratio("こんにちは", 1), None);
/// ```
#[must_use]
pub fn caps_ratio(text: &str, min_letters: usize) -> Option<f64> {
    let (upper, lower) = text.chars().fold((0_usize, 0_usize), |(upper, lower), ch| {
        if ch.is_uppercase() {
            (upper + 1, lower)
        } else if ch.is_lowercase() {
            (upper, lower + 1)
        } else {
            (upper, lower)
        }
    });
    let cased = upper + lower;
    (cased > 0 && cased >= min_letters).then(|| share(upper, cased))
}

#[allow(
    clippy::cast_precision_loss,
    reason = "letter counts stay far below 2^52, where f64 is exact"
)]
fn share(part: usize, whole: usize) -> f64 {
    part as f64 / whole as f64
}

/// The longest run of the same character in a row, counted in characters.
///
/// ```
/// assert_eq!(closure_sdk::text::longest_run("!!!!!!wow"), 6);
/// ```
#[must_use]
pub fn longest_run(text: &str) -> usize {
    let mut best = 0;
    let mut run = 0;
    let mut previous = None;
    for ch in text.chars() {
        run = if previous == Some(ch) { run + 1 } else { 1 };
        best = best.max(run);
        previous = Some(ch);
    }
    best
}

/// The longest row of combining marks (U+0300-U+036F), the mark of "zalgo" text.
///
/// ```
/// assert_eq!(closure_sdk::text::combining_run("h\u{316}\u{317}i"), 2);
/// ```
#[must_use]
pub fn combining_run(text: &str) -> usize {
    let mut best = 0;
    let mut run = 0;
    for ch in text.chars() {
        run = if is_combining(ch) { run + 1 } else { 0 };
        best = best.max(run);
    }
    best
}

/// How many emoji `text` shows, approximately. Each custom emoji `<:name:id>` or `<a:name:id>`
/// counts once. Among Unicode emoji, a skin-tone modifier, a variation selector and a
/// zero-width-joiner sequence count as one, and a flag (a pair of regional indicators) as one.
/// The ranges are the usual pictographic blocks, not the Unicode emoji list, so a few symbols
/// that people do not see as emoji are counted too.
///
/// ```
/// assert_eq!(closure_sdk::text::emoji_count("hi 👍🏽 <:wave:42>"), 2);
/// ```
#[must_use]
pub fn emoji_count(text: &str) -> usize {
    let mut count = 0;
    // Set after a zero-width joiner: the pictograph that follows is part of the same emoji.
    let mut joined = false;
    let mut rest = text;
    while let Some(ch) = rest.chars().next() {
        if ch == '<'
            && let Some(len) = custom_emoji_len(rest)
        {
            count += 1;
            joined = false;
            rest = &rest[len..];
            continue;
        }
        let mut width = ch.len_utf8();
        match ch {
            '\u{200D}' => joined = true,
            '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}' => {}
            // The keycap mark ends `1️⃣`, `#️⃣` and the like.
            '\u{20E3}' => count += 1,
            '\u{1F1E6}'..='\u{1F1FF}' => {
                count += 1;
                joined = false;
                if let Some(next) = rest[width..]
                    .chars()
                    .next()
                    .filter(|next| is_regional(*next))
                {
                    width += next.len_utf8();
                }
            }
            _ if is_pictograph(ch) => {
                if !joined {
                    count += 1;
                }
                joined = false;
            }
            _ => joined = false,
        }
        rest = &rest[width..];
    }
    count
}

fn is_regional(ch: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch)
}

fn is_pictograph(ch: char) -> bool {
    matches!(
        ch,
        '\u{00A9}' | '\u{00AE}' | '\u{203C}' | '\u{2049}' | '\u{2122}' | '\u{2139}'
            | '\u{2300}'..='\u{23FF}' // watches, media controls
            | '\u{2600}'..='\u{27BF}' // weather, symbols, dingbats
            | '\u{2B05}'..='\u{2B07}' | '\u{2B1B}' | '\u{2B1C}' | '\u{2B50}' | '\u{2B55}'
            | '\u{3030}' | '\u{303D}' | '\u{3297}' | '\u{3299}'
            | '\u{1F000}'..='\u{1FAFF}' // mahjong, cards, enclosed, emoticons, transport, symbols
    )
}

/// The length of the custom emoji `<:name:id>` or `<a:name:id>` at the start of `text`.
fn custom_emoji_len(text: &str) -> Option<usize> {
    let body = text.strip_prefix('<')?;
    let body = body.strip_prefix('a').unwrap_or(body);
    let body = body.strip_prefix(':')?;
    let name_len = body
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        .count();
    if name_len == 0 {
        return None;
    }
    let body = body[name_len..].strip_prefix(':')?;
    let id_len = body.bytes().take_while(u8::is_ascii_digit).count();
    if id_len == 0 {
        return None;
    }
    let tail = body[id_len..].strip_prefix('>')?;
    Some(text.len() - tail.len())
}

#[cfg(test)]
mod tests {
    use closure_core::event::{MessageInfo, UserInfo};
    use twilight_model::id::Id;

    use super::*;

    fn message(mentioned_users: &[u64], mentioned_roles: &[u64], everyone: bool) -> MessageInfo {
        MessageInfo {
            id: Id::new(1),
            channel_id: Id::new(2),
            guild_id: Id::new(3),
            author: UserInfo {
                id: Id::new(4),
                name: "u".into(),
                bot: false,
            },
            author_roles: Vec::new(),
            content: String::new(),
            mentioned_users: mentioned_users.iter().map(|id| Id::new(*id)).collect(),
            mentioned_roles: mentioned_roles.iter().map(|id| Id::new(*id)).collect(),
            mentions_everyone: everyone,
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: 0,
            edited_at: None,
        }
    }

    fn urls(found: &[Link]) -> Vec<&str> {
        found.iter().map(|link| link.url.as_str()).collect()
    }

    #[test]
    fn normalize_handles_empty_and_plain_text() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
        assert_eq!(normalize("Hello World"), "hello world");
    }

    #[test]
    fn normalize_folds_fullwidth_ascii_and_ideographic_space() {
        assert_eq!(normalize("ＡＢＣ１２３！"), "abc123!");
        assert_eq!(normalize("\u{3000}ｈｅｌｌｏ\u{3000}"), "hello");
    }

    #[test]
    fn normalize_keeps_japanese_and_drops_format_characters() {
        assert_eq!(normalize("こんにちは 世界"), "こんにちは 世界");
        assert_eq!(normalize("ba\u{200B}ss"), "bass");
        assert_eq!(normalize("so\u{AD}ft\u{FEFF}"), "soft");
        assert_eq!(normalize("a\u{202E}b\u{2060}c"), "abc");
    }

    #[test]
    fn normalize_removes_combining_marks() {
        assert_eq!(normalize("e\u{301}"), "e");
        assert_eq!(normalize("h\u{316}\u{317}\u{318}i"), "hi");
    }

    #[test]
    fn normalize_collapses_and_trims_whitespace() {
        assert_eq!(normalize("  a \t\n b   "), "a b");
        assert_eq!(normalize("a \u{200B} b"), "a b");
    }

    #[test]
    fn normalize_copes_with_long_input() {
        let text = "A".repeat(10_000);
        assert_eq!(normalize(&text), "a".repeat(10_000));
    }

    #[test]
    fn a_word_list_matches_normalised_text_and_returns_the_word_as_listed() {
        let list = WordList::new(["Spam"], MatchMode::Contains);
        assert_eq!(list.find("buy SPAM now"), Some("Spam"));
        assert_eq!(list.find("eggs"), None);
        let fullwidth = WordList::new(["ｓｐａｍ"], MatchMode::Whole);
        assert_eq!(fullwidth.find("SPAM!"), Some("ｓｐａｍ"));
    }

    #[test]
    fn contains_matches_inside_longer_words_and_whole_does_not() {
        let contains = WordList::new(["ass"], MatchMode::Contains);
        let whole = WordList::new(["ass"], MatchMode::Whole);
        assert_eq!(contains.find("class"), Some("ass"));
        assert_eq!(whole.find("class"), None);
        assert_eq!(whole.find("ass"), Some("ass"));
        assert_eq!(whole.find("ass."), Some("ass"));
        assert_eq!(whole.find("(ass)"), Some("ass"));
    }

    #[test]
    fn whole_with_punctuation_in_the_word_needs_a_non_letter_on_both_sides() {
        let list = WordList::new(["a.b"], MatchMode::Whole);
        assert_eq!(list.find("say a.b!"), Some("a.b"));
        assert_eq!(list.find("xa.b"), None);
        assert_eq!(list.find("a.bx"), None);
    }

    #[test]
    fn whole_finds_a_match_that_overlaps_an_earlier_one() {
        // The match at 0 is followed by a letter; the one at 2 is not.
        let list = WordList::new(["a!a!"], MatchMode::Whole);
        assert_eq!(list.find("a!a!a!"), Some("a!a!"));
    }

    #[test]
    fn japanese_has_no_word_breaks_so_whole_needs_a_separator() {
        let whole = WordList::new(["猫"], MatchMode::Whole);
        assert_eq!(whole.find("猫"), Some("猫"));
        assert_eq!(whole.find("猫好き"), None);
        assert_eq!(
            WordList::new(["猫"], MatchMode::Contains).find("猫好き"),
            Some("猫")
        );
    }

    #[test]
    fn empty_words_are_ignored_and_duplicates_keep_the_first_spelling() {
        let list = WordList::new(["", "  ", "\u{200B}", "Spam", "spam"], MatchMode::Contains);
        assert_eq!(list.find("nothing"), None);
        assert_eq!(list.find_all("SPAM"), ["Spam"]);
        assert!(
            WordList::new(["", " "], MatchMode::Whole)
                .find_all("x")
                .is_empty()
        );
    }

    #[test]
    fn find_returns_the_first_listed_match_and_find_all_every_one_in_list_order() {
        let list = WordList::new(["scam", "spam", "eggs"], MatchMode::Contains);
        assert_eq!(list.find("spam and scam"), Some("scam"));
        assert_eq!(list.find_all("spam and scam"), ["scam", "spam"]);
    }

    #[test]
    fn long_adversarial_input_is_handled_without_quadratic_rescans() {
        let text = "a!".repeat(5_000);
        assert_eq!(
            WordList::new(["a!a!"], MatchMode::Whole).find(&text),
            Some("a!a!")
        );
        assert_eq!(WordList::new(["a!b"], MatchMode::Whole).find(&text), None);
        let words = ["a".repeat(50)];
        assert_eq!(
            WordList::new(words, MatchMode::Contains).find(&"a".repeat(10_000)),
            Some("a".repeat(50).as_str())
        );
    }

    #[test]
    fn links_are_found_with_their_host_and_trailing_punctuation_trimmed() {
        let found = links("see https://example.com/path, and www.Example.org.");
        assert_eq!(
            urls(&found),
            ["https://example.com/path", "www.Example.org"]
        );
        assert_eq!(found[1].host, "www.example.org");
    }

    #[test]
    fn brackets_and_angle_brackets_around_links_are_not_part_of_them() {
        assert_eq!(
            urls(&links("(https://a.example.com/x).")),
            ["https://a.example.com/x"]
        );
        assert_eq!(
            urls(&links("<https://example.net/a>")),
            ["https://example.net/a"]
        );
    }

    #[test]
    fn the_host_drops_userinfo_and_port_and_is_lower_case() {
        let found = links("https://user:pw@Host.Example.com:8443/x");
        assert_eq!(found[0].host, "host.example.com");
        assert_eq!(found[0].url, "https://user:pw@Host.Example.com:8443/x");
    }

    #[test]
    fn is_domain_matches_the_domain_and_its_subdomains_only() {
        let first = |text: &str| links(text).remove(0);
        assert!(first("https://example.com").is_domain("example.com"));
        assert!(first("https://sub.example.com/x").is_domain("EXAMPLE.com"));
        assert!(!first("https://evil-example.com").is_domain("example.com"));
        assert!(!first("https://example.com.evil.net").is_domain("example.com"));
        assert!(!first("https://notexample.com").is_domain("example.com"));
    }

    #[test]
    fn text_that_only_looks_like_a_link_is_not_one() {
        assert!(links("example.com and no scheme").is_empty());
        assert!(links("xhttps://a.example.com").is_empty());
        assert!(links("http://").is_empty());
        assert!(links("").is_empty());
    }

    #[test]
    fn a_long_run_of_schemes_is_one_link_in_linear_time() {
        let text = "https://".repeat(1_250);
        assert_eq!(links(&text).len(), 1);
    }

    #[test]
    fn invites_come_from_all_three_hosts_with_or_without_a_scheme_in_order() {
        let found = invites(
            "join discord.gg/abc-Def1 or https://discord.com/invite/XYZ9, discordapp.com/invite/Qq",
        );
        assert_eq!(found, ["abc-Def1", "XYZ9", "Qq"]);
        assert_eq!(invites("DISCORD.GG/ABC"), ["ABC"]);
    }

    #[test]
    fn invites_are_deduplicated_and_stop_where_the_code_does() {
        assert_eq!(invites("discord.gg/a discord.gg/a"), ["a"]);
        assert_eq!(invites("<discord.gg/zz> (http://discord.gg/zz)"), ["zz"]);
        assert_eq!(invites("discord.gg/abc.def"), ["abc"]);
    }

    #[test]
    fn look_alike_hosts_and_empty_codes_are_not_invites() {
        assert!(invites("evil-discord.gg/x").is_empty());
        assert!(invites("xdiscord.gg/x").is_empty());
        assert!(invites("discord.gg/ alone").is_empty());
        assert!(invites("discord.gg/").is_empty());
    }

    #[test]
    fn mention_count_counts_distinct_users_and_roles_and_everyone_once() {
        assert_eq!(mention_count(&message(&[1, 1, 2], &[10, 10], true)), 4);
        assert_eq!(mention_count(&message(&[], &[], false)), 0);
    }

    #[test]
    fn caps_ratio_is_the_share_of_upper_case_among_cased_letters() {
        assert_eq!(caps_ratio("HELLO world", 5), Some(0.5));
        assert_eq!(caps_ratio("ABC", 3), Some(1.0));
        assert_eq!(caps_ratio("abc", 3), Some(0.0));
        assert_eq!(caps_ratio("A1 2! 3?", 1), Some(1.0));
    }

    #[test]
    fn caps_ratio_needs_enough_cased_letters_and_japanese_has_none() {
        assert_eq!(caps_ratio("Hello", 6), None);
        assert_eq!(caps_ratio("", 0), None);
        assert_eq!(caps_ratio("12345!", 0), None);
        assert_eq!(caps_ratio("こんにちは世界", 1), None);
        assert_eq!(caps_ratio(&"A".repeat(10_000), 1), Some(1.0));
    }

    #[test]
    fn longest_run_counts_characters_not_bytes() {
        assert_eq!(longest_run(""), 0);
        assert_eq!(longest_run("abccccd"), 4);
        assert_eq!(longest_run("ああああいう"), 4);
        assert_eq!(longest_run("aAaA"), 1);
        assert_eq!(longest_run(&"x".repeat(10_000)), 10_000);
    }

    #[test]
    fn emoji_count_treats_modifiers_and_joined_sequences_as_one() {
        assert_eq!(emoji_count(""), 0);
        assert_eq!(emoji_count("hello こんにちは 123"), 0);
        assert_eq!(emoji_count("😀😀"), 2);
        assert_eq!(emoji_count("👍🏽"), 1);
        assert_eq!(emoji_count("👨\u{200D}👩\u{200D}👧"), 1);
        assert_eq!(emoji_count("🏳\u{FE0F}\u{200D}🌈"), 1);
        assert_eq!(emoji_count("🇯🇵🇺🇸"), 2);
        assert_eq!(emoji_count("1\u{FE0F}\u{20E3}"), 1);
    }

    #[test]
    fn emoji_count_includes_custom_emoji_only_when_well_formed() {
        assert_eq!(emoji_count("<:smile:123456789>"), 1);
        assert_eq!(emoji_count("<a:dance:42>x<:y:1>"), 2);
        assert_eq!(emoji_count("<:bad name:1>"), 0);
        assert_eq!(emoji_count("<:ab:>"), 0);
        assert_eq!(emoji_count("<:ab:12"), 0);
    }

    #[test]
    fn emoji_count_is_linear_on_long_input() {
        assert_eq!(emoji_count(&"😀".repeat(5_000)), 5_000);
        assert_eq!(emoji_count(&"<:".repeat(5_000)), 0);
    }

    #[test]
    fn combining_run_is_the_longest_row_of_combining_marks() {
        assert_eq!(combining_run(""), 0);
        assert_eq!(combining_run("abc"), 0);
        assert_eq!(combining_run("a\u{301}\u{302}\u{303}b\u{301}"), 3);
        assert_eq!(
            combining_run(&format!("a{}", "\u{301}".repeat(10_000))),
            10_000
        );
    }
}
