module-description = Giveaways: a button to enter, and winners drawn when the time is up.
command-giveaway = giveaway
    .description = Run a giveaway
subcommand-giveaway-start = start
    .description = Start a giveaway with a button to enter
subcommand-giveaway-end = end
    .description = End a giveaway now and draw its winners
subcommand-giveaway-reroll = reroll
    .description = Draw new winners of a giveaway that is over
option-giveaway-start-prize = prize
    .description = What is given away
option-giveaway-start-duration = duration
    .description = How long it runs: 30m, 2h or 1d
option-giveaway-start-winners = winners
    .description = How many winners; 1 when left out
option-giveaway-start-channel = channel
    .description = Where it is posted; this channel when left out
option-giveaway-end-message = message
    .description = The id of the message of the giveaway
option-giveaway-reroll-message = message
    .description = The id of the message of the giveaway
permission-giveaway-manage = Start, end and redraw giveaways
giveaway-enter = Enter
giveaway-open =
    Press the button to enter.
    Ends { $ends }
    Winners: { $winners }
    Hosted by { $host }
giveaway-ended-winners =
    Ended.
    Winners: { $winners }
    Hosted by { $host }
giveaway-ended-nobody =
    Ended. Nobody entered.
    Hosted by { $host }
giveaway-won = Congratulations { $winners }! You won **{ $prize }**.
giveaway-redrawn = New winners for **{ $prize }**: { $winners }
giveaway-joined = You are in. { $count } entered so far.
giveaway-left = You are out of the giveaway.
giveaway-over = That giveaway is over.
giveaway-started = The giveaway is up: { $link }
giveaway-bad-duration = Give a time such as 30m, 2h or 1d, from a minute to 30 days.
giveaway-bad-winners = Winners are from 1 to 20.
giveaway-not-found = There is no giveaway on that message.
giveaway-not-over = That giveaway is still running; end it first.
giveaway-ended = The giveaway is over.
giveaway-no-entries = Nobody has entered, so there is nobody to draw.
giveaway-failed = That did not work just now. Try again in a moment.
