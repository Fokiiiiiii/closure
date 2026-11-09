permission-dehoist-sweep = Clean the names of the members already in the server
command-dehoist = dehoist
    .description = Take leading symbols off the names that sort to the top of the member list
dehoist-fallback = member
dehoist-reason = Dehoisted: the name started with a symbol
dehoist-done = Cleaned { $count } name(s).
dehoist-partial = Cleaned { $count } name(s) so far. This is not the whole server yet: run it again to continue from there.
dehoist-refused = Cleaned { $count } name(s), then stopped: the bot was refused several times, probably because its role is below some members. Check its position, then run it again.
