# Messages shared by every module. Module specific text lives in each module's own catalog.
error-invalid = That request is not valid: { $detail }
error-capability-denied = This feature is not permitted for that module.
error-forbidden = I don't have permission to do that.
error-hierarchy = I can't do that because of the role hierarchy. My highest role must be above the target's highest role.
error-not-found = I couldn't find that. It may have been deleted.
error-dm-closed = That user does not accept direct messages from me.
error-not-in-voice = That member is not in a voice channel.
error-rate-limited = Discord is rate limiting me. Please try again shortly.
error-transient = Discord is having trouble right now. Please try again shortly.
error-fatal = Something went wrong while talking to Discord.
error-internal = Something went wrong on my side. The incident has been logged.
error-module-disabled = That feature is turned off on this server.
error-command-failed = Something went wrong while running that command. Please try again later.
error-busy = The bot is busy with an earlier request. Please try again in a moment.

# Replies of the module SDK (closure-sdk).
sdk-done = Done.
sdk-not-yours = That is not for you.
sdk-gone = That is no longer available.
sdk-permission-level = You are not allowed to do that here. It needs level { $required } and you are at level { $actual }.
sdk-permission-denied = You are not allowed to do that here.
sdk-cooldown = Please try again { $retry }.
sdk-bad-argument = The value of `{ $option }` is missing or not valid.
sdk-unit-day = { $n ->
    [one] { $n } day
   *[other] { $n } days
}
sdk-unit-hour = { $n ->
    [one] { $n } hour
   *[other] { $n } hours
}
sdk-unit-minute = { $n ->
    [one] { $n } minute
   *[other] { $n } minutes
}
sdk-unit-second = { $n ->
    [one] { $n } second
   *[other] { $n } seconds
}
sdk-duration-none = less than a second
sdk-target-self = You cannot do that to yourself.
sdk-target-bot = You cannot do that to a bot.
sdk-target-owner = You cannot do that to the owner of the server.
sdk-target-higher = You cannot do that to someone whose highest role is not below yours.

# The moderation log, posted to the log channel of a server (closure-sdk `modlog`).
modlog-title = Case #{ $number } · { $kind }
modlog-kind-warn = Warning
modlog-kind-timeout = Timeout
modlog-kind-untimeout = Timeout removed
modlog-kind-vcmute = Voice mute
modlog-kind-vcunmute = Voice unmute
modlog-kind-kick = Kick
modlog-kind-ban = Ban
modlog-kind-unban = Unban
modlog-kind-note = Note
modlog-target = Target
modlog-moderator = Moderator
modlog-reason = Reason
modlog-no-reason = No reason given
modlog-expires = Expires
modlog-revoked = Revoked
modlog-revoked-by = By { $who } { $when }

# Punishments the SDK gives (closure-sdk `punish`): the direct messages, the notes and the errors.
punish-dm-warned = You were warned in a server. Reason: { $reason } (Active warnings: { $active })
punish-dm-kicked = You were kicked from a server. Reason: { $reason }
punish-dm-timed-out = You were timed out in a server until { $until }. Reason: { $reason }
punish-dm-banned = You were banned from a server. Reason: { $reason }
punish-dm-banned-for = You were banned from a server until { $until }. Reason: { $reason }
punish-dm-voice-muted = You were muted in voice channels in a server. Reason: { $reason }
punish-dm-voice-muted-for = You were muted in voice channels in a server until { $until }. Reason: { $reason }
punish-dm-not-delivered = (The direct message could not be delivered.)
punish-reason-none = no reason given
punish-timeout-no-timer = The timeout does not end by itself: its timer could not be set.
punish-ban-no-timer = The ban does not end by itself: its timer could not be set.
punish-vc-no-timer = The voice mute does not end by itself: its timer could not be set.
punish-timeout-too-long = Discord allows a timeout of 28 days at most.
punish-timeout-role-missing = Timeouts by role need a role: set timeout_role in the moderation settings.
punish-role-forbidden = The bot may not give the role for this punishment. Check that it has Manage Roles and that its top role is above the dedicated role.
punish-role-unavailable = The role for this punishment could not be set up. Check that the bot has Manage Roles and Manage Channels, then try again.
punish-roles-not-stripped = { $count } of the member's roles could not be taken off, so the member keeps them.
role-created-reason = Closure created the role that restrictions give.
restriction-audit = Closure gave back or took off the roles of a { $role } restriction ({ $what }).
restriction-sync-reason = Closure set the channel permissions of the { $role } role.
restriction-sync-done = Channel setup of a restriction role is done: { $skipped } channels could not be set up, { $allowed } channels let another role or a member speak, and { $unchecked } channels were not checked.
punish-voice-role-missing = Voice mutes by role need a role: set voice_mute_role in the moderation settings.
punish-ban-delete-days-range = The days of messages to delete are 0 to 7.
sdk-expired = This has expired, use the command again.
reason-removed-by-case = removed by case #{ $case }
reason-unbanned-by-case = unbanned by case #{ $case }
reason-ban-ran-out = the ban ran out
reason-ban-of-case-ran-out = the ban of case #{ $case } ran out
reason-timeout-of-case-ran-out = the timeout of case #{ $case } ran out
span-not-understood = Unknown time (e.g. 1h, tomorrow 18:00).
span-past = That time has passed ({ $zone }).
span-too-far = More than ten years away ({ $zone }).
text-command-usage = Usage: { $usage }
text-command-forms = Forms are not available for text commands. Use the slash command instead.
config-timezone = The zone of a date typed without one: UTC, Z, an offset such as UTC+9, UTC+09:00, UTC-5, +09:00 or -0500, or a fixed zone such as Asia/Tokyo. Zones with daylight saving time are refused; use a fixed offset instead.
config-prefix = The prefix of text commands, 1 to 10 characters without spaces. Unset, text commands are off.
config-delete-after = Seconds after which the message is deleted, 1 to 86400. Unset, it is kept.
config-emoji = A Unicode emoji, or a custom one as <:name:id> or <a:name:id>. An empty string means no emoji.
config-language = Language of the bot's messages; `auto` follows the server's community locale.
config-modules = Per-module settings, keyed by module id.
config-replies = Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose.
config-texts = Replaces the module's texts by message id; {"{"}name{"}"} is filled in with the argument name. An empty or non-text value keeps the default.
replay-direct-message = DM to { $user }
role-name-timeout = Mute
role-name-voice = VC mute
role-name-quarantine = Quarantine
