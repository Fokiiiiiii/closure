use std::collections::BTreeMap;
use std::fmt::Write;

use closure_sdk::duration::{TimeStyle, timestamp};
use closure_sdk::{Args, CommandCx, DiscordError, UserError};

use crate::{PAGE, State, Target, count, listed_reason, mention, state};

pub(crate) async fn userinfo(cx: CommandCx, args: Target) -> Result<(), UserError> {
    cx.defer(true).await?;
    let user = args.user;
    let member = match cx.api().get_member(cx.guild_id(), user).await {
        Ok(member) => Some(member),
        Err(DiscordError::NotFound) => None,
        Err(error) => return Err(error.into()),
    };
    let created = i64::try_from((user.get() >> 22) / 1000).unwrap_or(i64::MAX) + 1_420_070_400;
    let mut text = cx.t(
        "userinfo-header",
        &Args::new()
            .with("user", mention(user))
            .with("created", timestamp(created, TimeStyle::ShortDateTime)),
    );
    if let Some(member) = member {
        member_info(&cx, &member, &mut text).await?;
    }
    let mut before = None;
    let mut cases = Vec::new();
    loop {
        let page = cx.cases().for_target(user, before, PAGE).await?;
        let len = page.len();
        before = page.last().map(|case| case.number);
        cases.extend(page);
        if len < PAGE {
            break;
        }
    }
    let mut counts = BTreeMap::new();
    for case in &cases {
        *counts.entry(case.kind.as_str()).or_insert(0_usize) += 1;
    }
    text.push('\n');
    text.push_str(&cx.t(
        "userinfo-cases",
        &Args::new().with("count", count(cases.len())),
    ));
    for (kind, n) in counts {
        let _ = write!(text, "\n{kind}: {n}");
    }
    for case in cases.iter().take(3) {
        let _ = write!(
            text,
            "\n{}",
            cx.t(
                "case-list-line",
                &Args::new()
                    .with("number", case.number)
                    .with("kind", case.kind.as_str().to_owned())
                    .with("state", cx.t(state(case, cx.now()).key(), &Args::new()))
                    .with("reason", listed_reason(&cx, case.reason.as_deref()))
            )
        );
    }
    if cases
        .iter()
        .any(|case| case.kind.as_str() == "ban" && state(case, cx.now()) == State::Active)
    {
        text.push('\n');
        text.push_str(&cx.t("userinfo-ban", &Args::new()));
    }
    cx.reply_private(closure_sdk::embed::truncate(&text, 3900))
        .await
}

async fn member_info(
    cx: &CommandCx,
    member: &twilight_model::guild::Member,
    text: &mut String,
) -> Result<(), UserError> {
    let unknown = cx.t("userinfo-unknown", &Args::new());
    let joined = member.joined_at.map_or_else(
        || unknown.clone(),
        |at| timestamp(at.as_secs(), TimeStyle::ShortDateTime),
    );
    let roles = member
        .roles
        .iter()
        .map(|role| format!("<@&{}>", role.get()))
        .collect::<Vec<_>>()
        .join(", ");
    text.push('\n');
    text.push_str(
        &cx.t(
            "userinfo-member",
            &Args::new()
                .with("joined", joined)
                .with(
                    "nick",
                    member
                        .nick
                        .as_deref()
                        .map_or_else(|| unknown.clone(), crate::shorten),
                )
                .with("roles", if roles.is_empty() { unknown } else { roles }),
        ),
    );
    if let Some(until) = member
        .communication_disabled_until
        .filter(|at| at.as_secs() > cx.now())
    {
        text.push('\n');
        text.push_str(&cx.t(
            "userinfo-timeout",
            &Args::new().with(
                "until",
                timestamp(until.as_secs(), TimeStyle::ShortDateTime),
            ),
        ));
    }
    let config: crate::Config = cx.config()?;
    let auto_role = cx
        .ctx()
        .store()
        .get::<u64>("auto-role:timeout")
        .await?
        .and_then(closure_sdk::RoleId::new_checked);
    if [
        crate::restrict::role_of(config.timeout_role.as_deref()),
        auto_role,
    ]
    .into_iter()
    .flatten()
    .any(|role| member.roles.contains(&role))
    {
        text.push('\n');
        text.push_str(&cx.t("userinfo-role-mute", &Args::new()));
    }
    Ok(())
}
