//! The JSON Schema of the module's configuration, and its defaults.
//!
//! The schema is what a server's edit is checked against when it is saved. It mirrors the ranges
//! `config` checks again when it reads the rules, which is the safety net for a rule that got in
//! some other way.

use serde_json::{Value, json};

const DURATION: &str = "^([0-9]{1,10}[smhdw] ?)+$";
const SNOWFLAKE: &str = "^[0-9]{1,20}$";
const RULE_ID: &str = "^[a-z0-9_-]{1,32}$";
/// Counter names and the names of their triggers are kept to plain characters, as rule ids are.
const COUNTER_NAME: &str = "^[a-z0-9_-]{1,32}$";
const CONDITION: &str = "^\\s*(>=|<=|!=|>|<|=)\\s*[0-9]{1,10}\\s*$";
const MAX_COUNTER_VALUE: i64 = 2_147_483_647;

/// The schema and the defaults of the module's section.
pub fn settings() -> (Value, Value) {
    (
        json!({
            "type": "object",
            "properties": {
                "dry_run": {
                    "type": "boolean",
                    "description": "Act on nothing: post what would have happened to the log channel.",
                    "x-ui": { "level": "basic" }
                },
                "warn_expire_days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 365,
                    "description": "After how many days a warning of a rule expires.",
                    "x-ui": { "level": "basic" }
                },
                "dm_on_action": {
                    "type": "boolean",
                    "description": "Whether the member gets a direct message about a punishment.",
                    "x-ui": { "level": "basic" }
                },
                "timeout_method": {
                    "type": "string",
                    "enum": ["discord", "role"],
                    "description": "How a timeout is given: Discord's timeout, or a role that is taken off when its time is up."
                },
                "timeout_role": {
                    "type": ["string", "null"],
                    "pattern": SNOWFLAKE,
                    "description": "The id of the role a timeout gives, when timeout_method is role."
                },
                "counters": {
                    "type": "object",
                    "maxProperties": 10,
                    "description": "The counters, keyed by name: the numbers the rules add to, set, and react to.",
                    "propertyNames": { "pattern": COUNTER_NAME },
                    "additionalProperties": counter()
                },
                "rules": {
                    "type": "object",
                    "maxProperties": 100,
                    "description": "The rules, keyed by id. Each one is checked on every message; the rules are checked in the order of their ids, and the first one that trips acts unless it says to continue.",
                    "propertyNames": { "pattern": RULE_ID },
                    "additionalProperties": rule()
                }
            },
            "additionalProperties": false
        }),
        json!({
            "dry_run": false,
            "warn_expire_days": 30,
            "dm_on_action": true,
            "timeout_method": "discord",
            "timeout_role": null,
            "counters": {},
            "rules": {}
        }),
    )
}

fn counter() -> Value {
    json!({
        "type": "object",
        "properties": {
            "per_channel": { "type": "boolean" },
            "per_user": { "type": "boolean" },
            "initial_value": { "type": "integer", "minimum": 0, "maximum": MAX_COUNTER_VALUE },
            "decay": {
                "type": "object",
                "properties": {
                    "amount": { "type": "integer", "minimum": -MAX_COUNTER_VALUE, "maximum": MAX_COUNTER_VALUE },
                    "every": { "type": "string", "pattern": DURATION }
                },
                "required": ["amount", "every"],
                "additionalProperties": false
            },
            "triggers": {
                "type": "object",
                "maxProperties": 10,
                "propertyNames": { "pattern": COUNTER_NAME },
                "additionalProperties": {
                    "oneOf": [
                        { "type": "string", "pattern": CONDITION },
                        {
                            "type": "object",
                            "properties": {
                                "condition": { "type": "string", "pattern": CONDITION },
                                "reverse_condition": { "type": "string", "pattern": CONDITION }
                            },
                            "required": ["condition"],
                            "additionalProperties": false
                        }
                    ]
                }
            }
        },
        "additionalProperties": false
    })
}

fn rule() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": "The name shown in logs. Defaults to the id."
            },
            "enabled": { "type": "boolean" },
            "triggers": {
                "type": "array",
                "minItems": 1,
                "maxItems": 20,
                "description": "Any one of them tripping trips the rule.",
                "items": trigger()
            },
            "actions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 12,
                "description": "Run in order.",
                "items": action()
            },
            "scope": scope(),
            "cooldown": {
                "type": "string",
                "pattern": DURATION,
                "description": "After the rule fired for a member, it is skipped for that member for this long."
            },
            "continue_after_match": {
                "type": "boolean",
                "description": "Whether the rules after this one still run when it trips."
            }
        },
        "required": ["triggers", "actions"],
        "additionalProperties": false
    })
}

fn scope() -> Value {
    let list = json!({
        "type": "array",
        "maxItems": 100,
        "items": { "type": "string", "pattern": SNOWFLAKE }
    });
    json!({
        "type": "object",
        "properties": {
            "channels": list.clone(),
            "exclude_channels": list.clone(),
            "roles": list.clone(),
            "exclude_roles": list,
            "include_bots": { "type": "boolean" },
            "exempt_moderators": { "type": "boolean" },
            "member_age": {
                "type": "object",
                "properties": {
                    "min_days": { "type": "integer", "minimum": 0, "maximum": 36500 },
                    "max_days": { "type": "integer", "minimum": 0, "maximum": 36500 }
                },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    })
}

fn trigger() -> Value {
    json!({
        "oneOf": [
            tagged("any_message", json!({}), &[]),
            tagged("words", json!({
                "words": { "type": "array", "minItems": 1, "maxItems": 500,
                           "items": { "type": "string", "minLength": 1, "maxLength": 100 } },
                "case_sensitive": { "type": "boolean" },
                "whole_words": { "type": "boolean" },
                "normalize": { "type": "boolean" },
                "strip_markdown": { "type": "boolean" },
                "targets": targets()
            }), &["words"]),
            tagged("regex", json!({
                "patterns": { "type": "array", "minItems": 1, "maxItems": 20,
                              "items": { "type": "string", "minLength": 1, "maxLength": 200 } },
                "case_sensitive": { "type": "boolean" },
                "normalize": { "type": "boolean" },
                "strip_markdown": { "type": "boolean" },
                "targets": targets()
            }), &["patterns"]),
            tagged("links", json!({
                "mode": { "type": "string", "enum": ["any", "allowlist", "blocklist"] },
                "domains": { "type": "array", "maxItems": 200,
                             "items": { "type": "string", "minLength": 1, "maxLength": 253 } },
                "include_subdomains": { "type": "boolean" },
                "only_real_links": { "type": "boolean" },
                "include_regex": { "type": "array", "maxItems": 50,
                                   "items": { "type": "string", "minLength": 1, "maxLength": 200 } },
                "exclude_regex": { "type": "array", "maxItems": 50,
                                   "items": { "type": "string", "minLength": 1, "maxLength": 200 } }
            }), &[]),
            tagged("invites", json!({
                "allow_codes": { "type": "array", "maxItems": 100,
                                 "items": { "type": "string", "minLength": 1, "maxLength": 64 } },
                "block_codes": { "type": "array", "maxItems": 100,
                                 "items": { "type": "string", "minLength": 1, "maxLength": 64 } }
            }), &[]),
            tagged("attachments", json!({
                "min_count": { "type": "integer", "minimum": 1, "maximum": 10 },
                "max_count": { "type": "integer", "minimum": 0, "maximum": 10 },
                "blocked_extensions": list_of_text(16),
                "allowed_extensions": list_of_text(16),
                "blocked_mime_types": list_of_text(64),
                "allowed_mime_types": list_of_text(64)
            }), &[]),
            tagged("caps", json!({
                "max_ratio": { "type": "number", "minimum": 0, "maximum": 1 },
                "min_letters": { "type": "integer", "minimum": 1, "maximum": 2000 }
            }), &["max_ratio"]),
            tagged("repeat", json!({
                "max_run": { "type": "integer", "minimum": 2, "maximum": 2000 }
            }), &["max_run"]),
            tagged("stickers", json!({
                "max": { "type": "integer", "minimum": 0, "maximum": 10 }
            }), &["max"]),
            tagged("member_join", json!({
                "only_new": { "type": "boolean" },
                "new_threshold": { "type": "string", "pattern": DURATION }
            }), &[]),
            tagged("member_join_spam", json!({
                "amount": { "type": "integer", "minimum": 1, "maximum": 10000 },
                "within": { "type": "string", "pattern": DURATION }
            }), &["amount", "within"]),
            tagged("member_leave", json!({}), &[]),
            tagged("role_added", json!({ "roles": snowflakes(25) }), &["roles"]),
            tagged("role_removed", json!({ "roles": snowflakes(25) }), &["roles"]),
            tagged("case", json!({
                "kinds": {
                    "type": "array",
                    "maxItems": 9,
                    "items": { "type": "string", "enum": [
                        "warn", "note", "timeout", "untimeout", "kick", "ban", "unban", "vcmute", "vcunmute"
                    ] }
                },
                "include_own": { "type": "boolean" }
            }), &[]),
            tagged("thread_created", json!({}), &[]),
            tagged("thread_archived", json!({}), &[]),
            tagged("thread_unarchived", json!({}), &[]),
            tagged("thread_deleted", json!({}), &[]),
            tagged("counter", json!({
                "counter": { "type": "string", "pattern": COUNTER_NAME },
                "trigger": { "type": "string", "pattern": COUNTER_NAME },
                "reverse": { "type": "boolean" }
            }), &["counter", "trigger"]),
            tagged("spam", json!({
                "measure": { "type": "string", "enum": [
                    "messages", "mentions", "links", "attachments", "emojis",
                    "lines", "characters", "stickers", "duplicates"
                ] },
                "amount": { "type": "integer", "minimum": 1, "maximum": 10000 },
                "within": { "type": "string", "pattern": DURATION },
                "per_channel": { "type": "boolean" },
                "distinct_channels": { "type": "integer", "minimum": 2, "maximum": 50 },
                "ignore_identical_messages": { "type": "boolean" },
                "group_identical_messages": { "type": "boolean" }
            }), &["measure", "amount"]),
        ]
    })
}

/// A variant of the trigger union: its `type`, its own options, and the options it needs.
fn tagged(kind: &str, options: Value, required: &[&str]) -> Value {
    variant("type", kind, options, required)
}

fn targets() -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "items": { "type": "string", "enum": ["message", "username"] }
    })
}

fn list_of_text(max_length: u32) -> Value {
    json!({
        "type": "array",
        "maxItems": 50,
        "items": { "type": "string", "minLength": 1, "maxLength": max_length }
    })
}

fn action() -> Value {
    let reason = json!({ "type": "string", "minLength": 1, "maxLength": 2000 });
    json!({
        "oneOf": [
            action_tagged("delete", json!({}), &[]),
            action_tagged("purge", json!({
                "within": { "type": "string", "pattern": DURATION }
            }), &["within"]),
            action_tagged("warn", json!({ "reason": reason.clone() }), &[]),
            action_tagged("timeout", json!({
                "duration": { "type": "string", "pattern": DURATION },
                "reason": reason.clone()
            }), &["duration"]),
            action_tagged("kick", json!({ "reason": reason.clone() }), &[]),
            action_tagged("ban", json!({
                "duration": { "type": "string", "pattern": DURATION },
                "delete_days": { "type": "integer", "minimum": 0, "maximum": 7 },
                "reason": reason.clone()
            }), &[]),
            action_tagged("reply", json!({
                "text": reason.clone(),
                "auto_delete": { "type": "string", "pattern": DURATION }
            }), &["text"]),
            action_tagged("alert", json!({
                "channel": { "type": "string", "pattern": SNOWFLAKE },
                "text": reason.clone()
            }), &["channel"]),
            action_tagged("add_roles", json!({ "roles": snowflakes(25) }), &["roles"]),
            action_tagged("remove_roles", json!({ "roles": snowflakes(25) }), &["roles"]),
            action_tagged("set_nickname", json!({
                "name": { "type": "string", "minLength": 1, "maxLength": 100 }
            }), &["name"]),
            action_tagged("slowmode", json!({
                "duration": { "type": "string", "pattern": DURATION },
                "channels": { "type": "array", "maxItems": 10, "items": { "type": "string", "pattern": SNOWFLAKE } }
            }), &["duration"]),
            action_tagged("add_to_counter", json!({
                "counter": { "type": "string", "pattern": COUNTER_NAME },
                "amount": { "type": "integer", "minimum": -MAX_COUNTER_VALUE, "maximum": MAX_COUNTER_VALUE }
            }), &["counter", "amount"]),
            action_tagged("set_counter", json!({
                "counter": { "type": "string", "pattern": COUNTER_NAME },
                "value": { "type": "integer", "minimum": 0, "maximum": MAX_COUNTER_VALUE }
            }), &["counter", "value"]),
        ]
    })
}

/// A list of Discord ids, at most `max` of them.
fn snowflakes(max: u32) -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "maxItems": max,
        "items": { "type": "string", "pattern": SNOWFLAKE }
    })
}

/// A variant of the action union: its `do`, its own options, and the options it needs.
fn action_tagged(kind: &str, options: Value, required: &[&str]) -> Value {
    variant("do", kind, options, required)
}

/// One variant of a tagged union: the tag fixed to `kind`, then the options of the variant.
fn variant(tag: &str, kind: &str, options: Value, required: &[&str]) -> Value {
    let mut properties = match options {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    properties.insert(tag.into(), json!({ "const": kind }));
    let mut required: Vec<&str> = [&[tag][..], required].concat();
    required.sort_unstable();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}
