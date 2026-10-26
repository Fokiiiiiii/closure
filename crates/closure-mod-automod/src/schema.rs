//! The JSON Schema of the module's configuration, and its defaults.
//!
//! The schema is what a server's edit is checked against when it is saved. It mirrors the ranges
//! `config` checks again when it reads the rules, which is the safety net for a rule that got in
//! some other way.

use serde_json::{Value, json};

const DURATION: &str = "^([0-9]{1,10}[smhdw] ?)+$";
const SNOWFLAKE: &str = "^[0-9]{1,20}$";

/// The schema and the defaults of the module's section.
pub fn settings() -> (Value, Value) {
    (
        json!({
            "type": "object",
            "properties": {
                "dry_run": {
                    "type": "boolean",
                    "description": "Act on nothing: post what would have happened to the log channel."
                },
                "warn_expire_days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 365,
                    "description": "After how many days a warning of a rule expires."
                },
                "dm_on_action": {
                    "type": "boolean",
                    "description": "Whether the member gets a direct message about a punishment."
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
                "rules": {
                    "type": "array",
                    "maxItems": 50,
                    "description": "The rules, each checked on every message; the first one that hits acts.",
                    "items": rule()
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
            "rules": []
        }),
    )
}

fn rule() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "pattern": "^[a-z0-9_-]{1,32}$",
                "description": "A name for the rule, unique in the list."
            },
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": "The name shown in logs. Defaults to the id."
            },
            "enabled": { "type": "boolean" },
            "check": check(),
            "scope": scope(),
            "actions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 8,
                "items": action()
            }
        },
        "required": ["id", "check", "actions"],
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
            "exempt_moderators": { "type": "boolean" }
        },
        "additionalProperties": false
    })
}

fn check() -> Value {
    let mut variants = vec![words(), links(), invites()];
    variants.extend(counts());
    json!({ "oneOf": variants })
}

fn words() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "enum": ["words"] },
            "words": {
                "type": "array",
                "minItems": 1,
                "maxItems": 500,
                "items": { "type": "string", "minLength": 1, "maxLength": 100 }
            },
            "match": { "type": "string", "enum": ["contains", "whole"] }
        },
        "required": ["type", "words"],
        "additionalProperties": false
    })
}

fn links() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "enum": ["links"] },
            "mode": { "type": "string", "enum": ["any", "allowlist", "blocklist"] },
            "domains": {
                "type": "array",
                "maxItems": 200,
                "items": { "type": "string", "minLength": 1, "maxLength": 253 }
            }
        },
        "required": ["type", "mode"],
        "additionalProperties": false
    })
}

fn invites() -> Value {
    json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "enum": ["invites"] },
            "allow": {
                "type": "array",
                "maxItems": 100,
                "items": { "type": "string", "minLength": 1, "maxLength": 64 }
            }
        },
        "required": ["type"],
        "additionalProperties": false
    })
}

/// The checks that count something against a limit.
fn counts() -> Vec<Value> {
    vec![
        json!({
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["mentions"] },
                "max": { "type": "integer", "minimum": 1, "maximum": 50 }
            },
            "required": ["type", "max"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["caps"] },
                "max_ratio": { "type": "number", "minimum": 0, "maximum": 1 },
                "min_letters": { "type": "integer", "minimum": 1, "maximum": 2000 }
            },
            "required": ["type", "max_ratio"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["repeat"] },
                "max_run": { "type": "integer", "minimum": 2, "maximum": 2000 }
            },
            "required": ["type", "max_run"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["emoji"] },
                "max": { "type": "integer", "minimum": 1, "maximum": 200 }
            },
            "required": ["type", "max"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["attachments"] },
                "max": { "type": "integer", "minimum": 0, "maximum": 10 },
                "blocked_extensions": {
                    "type": "array",
                    "maxItems": 50,
                    "items": { "type": "string", "minLength": 1, "maxLength": 16 }
                }
            },
            "required": ["type"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["flood"] },
                "messages": { "type": "integer", "minimum": 2, "maximum": 100 },
                "per_minutes": { "type": "integer", "minimum": 1, "maximum": 60 }
            },
            "required": ["type", "messages", "per_minutes"],
            "additionalProperties": false
        }),
    ]
}

fn action() -> Value {
    json!({
        "oneOf": [
            { "type": "object", "properties": { "do": { "enum": ["delete"] } }, "required": ["do"], "additionalProperties": false },
            { "type": "object", "properties": { "do": { "enum": ["warn"] } }, "required": ["do"], "additionalProperties": false },
            {
                "type": "object",
                "properties": {
                    "do": { "enum": ["timeout"] },
                    "duration": { "type": "string", "pattern": DURATION }
                },
                "required": ["do", "duration"],
                "additionalProperties": false
            },
            { "type": "object", "properties": { "do": { "enum": ["kick"] } }, "required": ["do"], "additionalProperties": false },
            {
                "type": "object",
                "properties": {
                    "do": { "enum": ["ban"] },
                    "duration": { "type": "string", "pattern": DURATION },
                    "delete_days": { "type": "integer", "minimum": 0, "maximum": 7 }
                },
                "required": ["do"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "do": { "enum": ["notify"] },
                    "channel": { "type": "string", "pattern": SNOWFLAKE }
                },
                "required": ["do", "channel"],
                "additionalProperties": false
            }
        ]
    })
}
