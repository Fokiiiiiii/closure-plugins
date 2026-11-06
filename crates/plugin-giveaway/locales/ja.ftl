module-description = プレゼント企画（ギブアウェー）。ボタンで参加し、時間になったら当選者を抽選します。
command-giveaway = giveaway
    .description = ギブアウェーを開催する
subcommand-giveaway-start = start
    .description = 参加ボタン付きのギブアウェーを始める
subcommand-giveaway-end = end
    .description = ギブアウェーを今すぐ終了して当選者を抽選する
subcommand-giveaway-reroll = reroll
    .description = 終了したギブアウェーの当選者を引き直す
option-giveaway-start-prize = prize
    .description = 景品
option-giveaway-start-duration = duration
    .description = 開催時間: 30m、2h、1d など
option-giveaway-start-winners = winners
    .description = 当選者数（省略すると1人）
option-giveaway-start-channel = channel
    .description = 投稿するチャンネル（省略するとこのチャンネル）
option-giveaway-end-message = message
    .description = ギブアウェーのメッセージID
option-giveaway-reroll-message = message
    .description = ギブアウェーのメッセージID
permission-giveaway-manage = ギブアウェーの開始・終了・再抽選
giveaway-enter = 参加する
giveaway-open =
    ボタンを押して参加します。
    終了: { $ends }
    当選者数: { $winners }
    主催: { $host }
giveaway-ended-winners =
    終了しました。
    当選者: { $winners }
    主催: { $host }
giveaway-ended-nobody =
    終了しました。参加者はいませんでした。
    主催: { $host }
giveaway-won = おめでとうございます { $winners }！ **{ $prize }** が当選しました。
giveaway-redrawn = **{ $prize }** の新しい当選者: { $winners }
giveaway-joined = 参加しました。現在 { $count } 人が参加しています。
giveaway-left = 参加を取り消しました。
giveaway-over = このギブアウェーは終了しています。
giveaway-started = ギブアウェーを開始しました: { $link }
giveaway-bad-duration = 30m、2h、1d のように、1分から30日の間で時間を指定してください。
giveaway-bad-winners = 当選者数は1〜20です。
giveaway-not-found = そのメッセージのギブアウェーはありません。
giveaway-not-over = そのギブアウェーはまだ開催中です。先に終了させてください。
giveaway-ended = ギブアウェーを終了しました。
giveaway-no-entries = 参加者がいないので、抽選できません。
giveaway-failed = うまくいきませんでした。少し待ってからもう一度お試しください。
