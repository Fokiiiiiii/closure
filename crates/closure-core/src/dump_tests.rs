use std::fs;
use std::path::PathBuf;

use crate::action::{Action, MentionPolicy};
use crate::dump::{dump_file, record_in};
use crate::event::ChannelId;

/// A directory of this test process that no other test uses, emptied before the test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("closure-dump-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn sample(text: &str) -> Action {
    Action::SendMessage {
        channel_id: ChannelId::new(7),
        content: Some(text.to_owned()),
        embeds: vec![],
        reply_to: None,
        mentions: MentionPolicy::None,
    }
}

#[test]
fn appends_one_json_line_per_action_to_the_process_file() {
    let dir = scratch("lines");

    record_in(Some(&dir), &sample("first")).unwrap();
    record_in(Some(&dir), &sample("second")).unwrap();

    let text = fs::read_to_string(dump_file(&dir)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let back: Action = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(back, sample("first"));
}

#[test]
fn records_nothing_without_a_directory() {
    let dir = scratch("none");

    record_in(None, &sample("hidden")).unwrap();

    assert!(!dir.exists());
    assert!(!dump_file(&dir).exists());
}
