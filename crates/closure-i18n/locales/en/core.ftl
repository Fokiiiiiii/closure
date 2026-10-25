# Messages shared by every module. Module specific text lives in each module's own catalog.
error-invalid = That request is not valid: { $detail }
error-capability-denied = This feature is not permitted for that module.
error-forbidden = I don't have permission to do that.
error-hierarchy = I can't do that because of the role hierarchy. My highest role must be above the target's highest role.
error-not-found = I couldn't find that. It may have been deleted.
error-dm-closed = That user does not accept direct messages from me.
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
