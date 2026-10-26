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

# Replies of the module SDK (closure-sdk).
sdk-done = Done.
sdk-not-yours = That is not for you.
sdk-gone = That is no longer available.
sdk-permission-level = You are not allowed to do that here. It needs level { $required } and you are at level { $actual }.
sdk-permission-denied = You are not allowed to do that here.
sdk-cooldown = Please wait { $seconds } more second(s) before using that again.
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
punish-dm-timed-out = You were timed out in a server for { $duration }. Reason: { $reason }
punish-dm-banned = You were banned from a server. Reason: { $reason }
punish-dm-banned-for = You were banned from a server for { $duration }. Reason: { $reason }
punish-dm-voice-muted = You were muted in voice channels in a server. Reason: { $reason }
punish-dm-voice-muted-for = You were muted in voice channels in a server for { $duration }. Reason: { $reason }
punish-dm-not-delivered = (The direct message could not be delivered.)
punish-reason-none = no reason given
punish-timeout-no-timer = The timeout does not end by itself: its timer could not be set.
punish-ban-no-timer = The ban does not end by itself: its timer could not be set.
punish-vc-no-timer = The voice mute does not end by itself: its timer could not be set.
punish-timeout-too-long = Discord allows a timeout of 28 days at most.
punish-timeout-role-missing = Timeouts by role need a role: set timeout_role in the moderation settings.
punish-voice-role-missing = Voice mutes by role need a role: set voice_mute_role in the moderation settings.
punish-ban-delete-days-range = The days of messages to delete are 0 to 7.
sdk-expired = This has expired, use the command again.
span-not-understood = Unknown time (e.g. 1h, tomorrow 18:00).
span-past = That time has passed ({ $zone }).
span-too-far = More than ten years away ({ $zone }).
