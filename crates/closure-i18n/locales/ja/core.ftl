# すべてのモジュールで共通のメッセージ。モジュール固有の文言は各モジュールのカタログに置く。
error-invalid = リクエストが正しくありません: { $detail }
error-capability-denied = このモジュールにはこの操作が許可されていません。
error-forbidden = その操作を行う権限がありません。
error-hierarchy = ロールの階層が原因で実行できません。私の最上位ロールが、対象の最上位ロールより上にある必要があります。
error-not-found = 対象が見つかりません。すでに削除された可能性があります。
error-dm-closed = そのユーザーはこのボットからのダイレクトメッセージを受け付けていません。
error-not-in-voice = そのメンバーはボイスチャンネルにいません。
error-rate-limited = Discord のレート制限中です。しばらくしてからもう一度お試しください。
error-transient = Discord 側で一時的な問題が発生しています。しばらくしてからもう一度お試しください。
error-fatal = Discord との通信中に問題が発生しました。
error-internal = 内部でエラーが発生しました。内容は記録されています。
error-module-disabled = この機能はこのサーバーでは無効になっています。
error-command-failed = コマンドの実行中に問題が発生しました。しばらくしてからもう一度お試しください。

# モジュールSDK (closure-sdk) の返信。
sdk-done = 完了しました。
sdk-not-yours = これはあなた向けではありません。
sdk-gone = これはもう利用できません。
sdk-permission-level = ここでそれを行う権限がありません。レベル { $required } が必要で、あなたはレベル { $actual } です。
sdk-permission-denied = ここでそれを行う権限がありません。
sdk-cooldown = あと { $seconds } 秒待ってから、もう一度お試しください。
sdk-bad-argument = `{ $option }` の値がないか、正しくありません。
sdk-unit-day = { $n }日
sdk-unit-hour = { $n }時間
sdk-unit-minute = { $n }分
sdk-unit-second = { $n }秒
sdk-duration-none = 1秒未満
sdk-target-self = 自分自身には実行できません。
sdk-target-bot = ボットには実行できません。
sdk-target-owner = サーバーの所有者には実行できません。
sdk-target-higher = 最も上位のロールが自分以上のメンバーには実行できません。

# モデレーションログ。サーバーのログチャンネルに投稿される (closure-sdk の `modlog`)。
modlog-title = ケース #{ $number } · { $kind }
modlog-kind-warn = 警告
modlog-kind-timeout = タイムアウト
modlog-kind-untimeout = タイムアウト解除
modlog-kind-kick = キック
modlog-kind-ban = BAN
modlog-kind-unban = BAN 解除
modlog-kind-note = メモ
modlog-target = 対象
modlog-moderator = 実行者
modlog-reason = 理由
modlog-no-reason = 理由なし
modlog-expires = 期限
modlog-revoked = 取り消し
modlog-revoked-by = { $who }（{ $when }）

# Punishments the SDK gives (closure-sdk `punish`): the direct messages, the notes and the errors.
punish-dm-warned = サーバーで警告を受けました。理由: { $reason } (有効な警告: { $active } 件)
punish-dm-kicked = サーバーからキックされました。理由: { $reason }
punish-dm-timed-out = サーバーで { $duration } タイムアウトされました。理由: { $reason }
punish-dm-banned = サーバーから BAN されました。理由: { $reason }
punish-dm-banned-for = サーバーから { $duration } の間 BAN されました。理由: { $reason }
punish-dm-voice-muted = サーバーのボイスチャンネルでミュートされました。理由: { $reason }
punish-dm-voice-muted-for = サーバーのボイスチャンネルで { $duration } の間ミュートされました。理由: { $reason }
punish-dm-not-delivered = (ダイレクトメッセージは届きませんでした。)
punish-reason-none = 理由なし
punish-timeout-no-timer = タイムアウトは期間が過ぎても解除されません。タイマーを設定できませんでした。
punish-ban-no-timer = 期間が過ぎても解除されません。タイマーを設定できませんでした。
punish-vc-no-timer = ボイスミュートは期間が過ぎても解除されません。タイマーを設定できませんでした。
punish-timeout-too-long = Discord のタイムアウトは最長 28 日です。
punish-timeout-role-missing = ロールによるタイムアウトには、設定の timeout_role にロールが必要です。
punish-voice-role-missing = ロールによるボイスミュートには、設定の voice_mute_role にロールが必要です。
punish-ban-delete-days-range = 削除するメッセージの日数は 0 から 7 までです。
sdk-expired = 期限が切れました。コマンドをもう一度使ってください。
span-not-understood = 理解できない期間です（例: 1h, 明日 18:00）。
span-past = その日時は過去です（{ $zone }）。
span-too-far = 10年より先の日時です（{ $zone }）。
