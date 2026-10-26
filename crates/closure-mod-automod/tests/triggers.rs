//! Each trigger of a rule, run against one message: a hit and a miss for each option.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]
#![allow(clippy::too_many_lines, reason = "a scenario reads best as one block")]
#![allow(
    clippy::needless_pass_by_value,
    reason = "helpers take the JSON they embed by value, which keeps the rules readable at the call site"
)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

// ---- words ----

#[tokio::test]
async fn words_contains_matches_japanese_inside_a_sentence() {
    let trigger = json!({ "type": "words", "words": ["バカ"], "whole_words": false });
    assert!(trips(trigger, "お前はバカだ").await);
}

#[tokio::test]
async fn words_whole_matches_only_a_word_on_its_own() {
    let spam = json!({ "type": "words", "words": ["spam"] });
    assert!(trips(spam.clone(), "this is spam!").await);
    assert!(!trips(spam, "spammy").await);
    assert!(!trips(json!({ "type": "words", "words": ["ass"] }), "class").await);
}

#[tokio::test]
async fn words_whole_does_not_match_a_japanese_word_inside_a_run() {
    let trigger = json!({ "type": "words", "words": ["バカ"] });
    assert!(!trips(trigger, "お前はバカだ").await);
}

#[tokio::test]
async fn words_ignore_case_unless_case_sensitive_is_set() {
    assert!(trips(json!({ "type": "words", "words": ["spam"] }), "SPAM").await);
    let strict = json!({ "type": "words", "words": ["spam"], "case_sensitive": true });
    assert!(!trips(strict, "SPAM").await);
}

#[tokio::test]
async fn words_fold_width_only_with_normalize() {
    assert!(!trips(json!({ "type": "words", "words": ["spam"] }), "ＳＰＡＭ").await);
    let folded = json!({ "type": "words", "words": ["spam"], "normalize": true });
    assert!(trips(folded, "ＳＰＡＭ").await);
}

#[tokio::test]
async fn words_strip_markdown_only_when_asked() {
    let stripped = json!({ "type": "words", "words": ["spam"], "strip_markdown": true });
    assert!(trips(stripped, "sp*am").await);
    assert!(!trips(json!({ "type": "words", "words": ["spam"] }), "sp*am").await);
}

#[tokio::test]
async fn words_contains_mode_matches_inside_a_longer_word() {
    let trigger = json!({ "type": "words", "words": ["spam"], "whole_words": false });
    assert!(trips(trigger, "spammy").await);
}

#[tokio::test]
async fn words_can_read_the_username_instead_of_the_message() {
    let trigger = json!({ "type": "words", "words": ["spam"], "targets": ["username"] });
    let mut named = custom(MEMBER, CHANNEL, "hello");
    named.author.name = "spam".into();
    assert!(trips_with(trigger.clone(), named).await);

    let mut plain = custom(MEMBER, CHANNEL, "spam");
    plain.author.name = "tester".into();
    assert!(
        !trips_with(trigger, plain).await,
        "a message is not read when only the username is a target"
    );
}

// ---- regex ----

#[tokio::test]
async fn regex_matches_case_insensitively_by_default() {
    let trigger = json!({ "type": "regex", "patterns": [r"free\s+nitro"] });
    assert!(trips(trigger.clone(), "Free  NITRO now").await);
    assert!(!trips(trigger, "nothing here").await);
    let strict = json!({ "type": "regex", "patterns": [r"free\s+nitro"], "case_sensitive": true });
    assert!(!trips(strict, "Free NITRO").await);
}

#[tokio::test]
async fn regex_with_catastrophic_backtracking_finishes_at_once() {
    let text = format!("{}b", "a".repeat(50_000));
    let started = Instant::now();
    assert!(!trips(json!({ "type": "regex", "patterns": ["(a+)+$c"] }), &text).await);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a regex is linear in the text"
    );
}

// ---- links ----

#[tokio::test]
async fn links_any_hits_a_link_and_misses_plain_text() {
    let any = json!({ "type": "links", "mode": "any" });
    assert!(trips(any.clone(), "see https://example.com/x").await);
    assert!(!trips(any, "no link here").await);
}

#[tokio::test]
async fn links_allowlist_hits_a_link_outside_the_list_and_counts_subdomains_as_inside() {
    let allow = json!({ "type": "links", "mode": "allowlist", "domains": ["example.com"] });
    assert!(!trips(allow.clone(), "https://example.com/x").await);
    assert!(!trips(allow.clone(), "https://shop.example.com").await);
    assert!(trips(allow, "https://evil-example.com").await);
}

#[tokio::test]
async fn links_blocklist_hits_only_listed_domains_and_their_subdomains() {
    let block = json!({ "type": "links", "mode": "blocklist", "domains": ["bad.net"] });
    assert!(trips(block.clone(), "https://bad.net").await);
    assert!(trips(block.clone(), "https://sub.bad.net/x").await);
    assert!(!trips(block, "https://good.org").await);
}

#[tokio::test]
async fn links_without_subdomains_match_only_the_domain_itself() {
    let block = json!({
        "type": "links", "mode": "blocklist", "domains": ["bad.net"], "include_subdomains": false
    });
    assert!(trips(block.clone(), "https://bad.net").await);
    assert!(!trips(block, "https://sub.bad.net").await);
}

#[tokio::test]
async fn links_bare_domains_count_only_when_real_links_is_off() {
    let real = json!({ "type": "links", "mode": "blocklist", "domains": ["example.org"] });
    assert!(!trips(real, "visit evil.example.org now").await);
    let bare = json!({
        "type": "links", "mode": "blocklist", "domains": ["example.org"], "only_real_links": false
    });
    assert!(trips(bare, "visit evil.example.org now").await);
}

#[tokio::test]
async fn links_include_and_exclude_regex_narrow_the_urls() {
    let downloads = json!({ "type": "links", "mode": "any", "include_regex": ["/download"] });
    assert!(!trips(downloads.clone(), "https://a.com/home").await);
    assert!(trips(downloads, "https://a.com/download/x").await);

    let not_github = json!({ "type": "links", "mode": "any", "exclude_regex": ["github\\.com"] });
    assert!(!trips(not_github.clone(), "https://github.com/x").await);
    assert!(trips(not_github, "https://other.com").await);
}

// ---- invites ----

#[tokio::test]
async fn invites_hit_unless_the_code_is_allowed_and_blocked_codes_always_hit() {
    assert!(trips(json!({ "type": "invites" }), "join discord.gg/abc123").await);
    let allowed = json!({ "type": "invites", "allow_codes": ["abc123"] });
    assert!(!trips(allowed, "join discord.gg/abc123").await);
    let blocked = json!({
        "type": "invites", "allow_codes": ["abc123"], "block_codes": ["abc123"]
    });
    assert!(trips(blocked, "join discord.gg/abc123").await);
    assert!(!trips(json!({ "type": "invites" }), "no invite here").await);
}

// ---- attachments ----

fn with_files(files: Vec<closure_sdk::AttachmentInfo>) -> closure_sdk::MessageInfo {
    let mut info = custom(MEMBER, CHANNEL, "");
    info.attachments = files;
    info
}

#[tokio::test]
async fn attachments_by_count_at_least_and_more_than() {
    let at_least_one = json!({ "type": "attachments", "min_count": 1 });
    assert!(trips_with(at_least_one.clone(), with_files(vec![file("a.png", None)])).await);
    assert!(!trips_with(at_least_one, with_files(vec![])).await);

    let three = || with_files(vec![file("a", None), file("b", None), file("c", None)]);
    assert!(trips_with(json!({ "type": "attachments", "max_count": 2 }), three()).await);
    assert!(!trips_with(json!({ "type": "attachments", "max_count": 3 }), three()).await);
}

#[tokio::test]
async fn attachments_by_extension_ignore_case_and_a_leading_dot() {
    let blocked = json!({ "type": "attachments", "blocked_extensions": [".EXE"] });
    assert!(trips_with(blocked.clone(), with_files(vec![file("setup.exe", None)])).await);
    assert!(!trips_with(blocked, with_files(vec![file("notes.txt", None)])).await);

    let images = json!({ "type": "attachments", "allowed_extensions": ["png"] });
    assert!(trips_with(images.clone(), with_files(vec![file("a.gif", None)])).await);
    assert!(!trips_with(images, with_files(vec![file("a.png", None)])).await);
}

#[tokio::test]
async fn attachments_by_mime_type_with_wildcards() {
    let blocked = json!({ "type": "attachments", "blocked_mime_types": ["image/*"] });
    assert!(
        trips_with(
            blocked.clone(),
            with_files(vec![file("a.png", Some("image/png"))])
        )
        .await
    );
    assert!(!trips_with(blocked, with_files(vec![file("a.mp4", Some("video/mp4"))])).await);

    let allowed = json!({ "type": "attachments", "allowed_mime_types": ["image/png"] });
    assert!(
        trips_with(
            allowed.clone(),
            with_files(vec![file("a.mp4", Some("video/mp4"))])
        )
        .await
    );
    assert!(
        trips_with(allowed.clone(), with_files(vec![file("a", None)])).await,
        "a file of an unknown type is not in the list"
    );
    assert!(!trips_with(allowed, with_files(vec![file("a.png", Some("image/png"))])).await);
}

// ---- caps, repeats, stickers, any message ----

#[tokio::test]
async fn caps_need_enough_letters_before_they_count() {
    let caps = json!({ "type": "caps", "max_ratio": 0.8, "min_letters": 10 });
    assert!(trips(caps.clone(), "HELLO WORLD THIS IS LOUD").await);
    assert!(!trips(caps.clone(), "HELLO").await);
    assert!(!trips(caps, "hello world").await);
}

#[tokio::test]
async fn repeat_hits_a_run_longer_than_allowed() {
    let repeat = json!({ "type": "repeat", "max_run": 5 });
    assert!(trips(repeat.clone(), "aaaaaaa").await);
    assert!(!trips(repeat, "aaaaa").await);
}

#[tokio::test]
async fn stickers_count_against_the_max() {
    let trigger = json!({ "type": "stickers", "max": 1 });
    let harness = deletes_when(trigger.clone());
    let mut two = custom(MEMBER, CHANNEL, "");
    two.sticker_count = 2;
    post(&harness, two).await;
    assert_eq!(deletions(&harness), 1);

    let harness = deletes_when(trigger);
    let mut one = custom(MEMBER, CHANNEL, "");
    one.sticker_count = 1;
    post(&harness, one).await;
    assert_eq!(deletions(&harness), 0);
}

#[tokio::test]
async fn any_message_trips_on_every_message_even_an_empty_one() {
    let trigger = json!({ "type": "any_message" });
    assert!(trips(trigger.clone(), "hello").await);
    assert!(trips(trigger, "").await);
}
