//! Values for the usual contexts of a module: a user, a guild, a message. The names follow
//! Zeppelin's where closure has the same data, and Discord ids are text, as they are there.

use std::collections::BTreeMap;

use closure_core::event::{GuildId, MessageInfo, UserInfo};

use super::Value;

/// A user: `id`, `username`, `mention`, `bot`.
#[must_use]
pub fn user(user: &UserInfo) -> Value {
    object([
        ("id", user.id.to_string().into()),
        ("username", user.name.as_str().into()),
        ("mention", format!("<@{}>", user.id).into()),
        ("bot", user.bot.into()),
    ])
}

/// A guild: `id`, `name`. The name is passed in, since an event does not carry it.
#[must_use]
pub fn guild(id: GuildId, name: &str) -> Value {
    object([("id", id.to_string().into()), ("name", name.into())])
}

/// A message: `id`, `content`, `author` (a [`user`]), and `channel` with `id` and `mention`.
#[must_use]
pub fn message(message: &MessageInfo) -> Value {
    let channel = message.channel_id;
    object([
        ("id", message.id.to_string().into()),
        ("content", message.content.as_str().into()),
        ("author", user(&message.author)),
        (
            "channel",
            object([
                ("id", channel.to_string().into()),
                ("mention", format!("<#{channel}>").into()),
            ]),
        ),
    ])
}

fn object<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Map(
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[cfg(test)]
mod tests {
    use closure_core::event::UserInfo;
    use twilight_model::id::Id;

    use super::super::{Template, Values};
    use super::*;

    fn render(text: &str, values: &Values) -> String {
        Template::parse(text).unwrap().render(values)
    }

    #[test]
    fn a_user_has_its_id_name_mention_and_bot_flag() {
        let info = UserInfo {
            id: Id::new(42),
            name: "Ann".into(),
            bot: false,
        };
        let values = Values::new().with("user", user(&info));
        assert_eq!(
            render(
                "{user.id} {user.username} {user.mention} [{user.bot}]",
                &values
            ),
            "42 Ann <@42> [false]"
        );
    }

    #[test]
    fn a_message_has_its_content_author_and_channel() {
        let info = MessageInfo {
            id: Id::new(1),
            channel_id: Id::new(2),
            guild_id: Id::new(3),
            author: UserInfo {
                id: Id::new(4),
                name: "u".into(),
                bot: true,
            },
            author_roles: Vec::new(),
            content: "hi".into(),
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: 0,
            edited_at: None,
        };
        let values = Values::new().with("message", message(&info));
        assert_eq!(
            render(
                "{message.content} {message.author.username} {message.author.bot} {message.channel.mention}",
                &values
            ),
            "hi u true <#2>"
        );
    }

    #[test]
    fn a_guild_has_its_id_and_name() {
        let values = Values::new().with("guild", guild(Id::new(9), "Zeppelin"));
        assert_eq!(render("{guild.name} ({guild.id})", &values), "Zeppelin (9)");
    }
}
