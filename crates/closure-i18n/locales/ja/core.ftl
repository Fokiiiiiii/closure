# すべてのモジュールで共通のメッセージ。モジュール固有の文言は各モジュールのカタログに置く。
error-invalid = リクエストが正しくありません: { $detail }
error-capability-denied = このモジュールにはこの操作が許可されていません。
error-forbidden = その操作を行う権限がありません。
error-hierarchy = ロールの階層が原因で実行できません。私の最上位ロールが、対象の最上位ロールより上にある必要があります。
error-not-found = 対象が見つかりません。すでに削除された可能性があります。
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
