# closure

あらゆる規模のサーバーに対応する、Rust 製の Discord Bot です。 · [English](README.md)

closure は、小さく厳格な「核」を中心に作られたモデレーション Bot です。機能に必要なもの
（イベント、Discord 操作、サーバーごとの設定、翻訳）はすべて核が提供するので、新しい機能は
核を弱めることなくモジュールとして追加できます。

**現状:** 基盤（コア、設定、Discord 層、HTTP API）の上に、4 つのモジュール（moderation、automod、logs、diagnostics）が実装されています。
モデレーション機能そのものは次のステップです。

## 基盤が提供するもの

- **入口の狭いモジュール。** モジュールは必要なもの（イベント、Intent、権限、コマンド、設定
  スキーマ、翻訳カタログ）を宣言します。Discord を直接操作することはなく、権限の確認と入力検証を
  行うクライアント越しに依頼します。詳しくは [docs/writing-modules.md](docs/writing-modules.md)。
- **設定は 2 通り、文書は 1 つ。** プリセットと少数のスイッチで手軽に、または完全な YAML で
  詳細に設定できます。どちらも同じ「差分だけの文書」を編集するので、既定値の改善は全サーバーに
  届きます。詳しくは [docs/configuration.md](docs/configuration.md)。
- **大規模にも小規模にも。** イベントはサーバーごとのレーンで処理され、順序が必要な範囲は順序を
  保ち、それ以外は並列に動きます。キューの上限、タイムアウト、panic の隔離、モジュールごとの
  サーキットブレーカーを備え、プリセットがこれらの上限を決めます。
- **安全な Discord 操作。** すべての操作は、監査ログの理由が必須のデータとして表され、送信前に
  ロール階層で確認されます。明示しない限り、メッセージは誰にも通知しません。
- **日本語と英語。** 言語の追加はカタログの追加だけです。サーバーごとに言語を選べ（Discord の
  設定に従うことも可）、コマンドの返信は実行した人の言語で返ります。
- **運用。** ヘルス/レディネス、Prometheus メトリクス、構造化ログ、グレースフルシャットダウン。
  データベースが唯一の正本です。

全体の構成は [docs/architecture.md](docs/architecture.md) を参照してください。ビルド、リリース、
コンテナイメージは [docs/deploy.md](docs/deploy.md)（ビルドは GitHub 上で行うので、手元では
コンパイルせずにダウンロードして使います）。

## 動かし方

最近の stable な Rust と PostgreSQL 14 以降が必要です。

1. [Discord Developer Portal](https://discord.com/developers/applications) でアプリケーションと Bot を
   作り、`bot` と `applications.commands` のスコープで招待します。Bot の設定で **Server Members Intent**
   （特権 Intent。モデレーションモジュールが再参加したメンバーに使います）を有効にします。
2. 空のデータベースを作ります（例: `createdb closure`）。
3. 設定を環境変数で渡します（作業ディレクトリの `.env` も読まれます。バージョン管理には入れないで
   ください）。

   ```text
   DISCORD_TOKEN=...            # Bot トークン
   DISCORD_APPLICATION_ID=...
   DATABASE_URL=postgres://user:password@localhost/closure
   API_ADMIN_TOKEN=...          # 任意。24 文字以上。未設定なら HTTP API は起動しません
   COMMAND_GUILD=...            # 任意。1 つのサーバーにだけコマンドを公開（即時反映）
   ```

4. `cargo run --release -p closure`

マイグレーションは起動時に適用されます。`closure --help` で全オプションが分かります。停止は
Ctrl-C か SIGTERM で、処理中のキューを終えてから止まります。

HTTP API は既定で `127.0.0.1:8080` で待ち受けます。外部に公開する場合は TLS を前段に置いてください。

## 開発

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
DATABASE_URL=postgres://localhost/closure_test cargo test --workspace
cargo deny check
```

データベースのテストは専用のデータベースを作って消すので、`DATABASE_URL` はそれを許可された
ユーザーのサーバーを指す必要があります。本物の Discord API に対するテストは既定では無視され、
用意したテスト用アプリとサーバーを使います（`crates/closure-discord/tests/live.rs` の冒頭を参照）。

## 制限事項

- モジュールはコンパイルして組み込む Rust crate です。将来 Lua や WebAssembly のモジュールを
  追加できるように契約（マニフェスト、直列化できるイベントと操作、権限）は設計してありますが、
  実装はまだです。
- 1 Bot につき 1 プロセスです。シャードを複数プロセスに分ける構成は未対応です。
- HTTP API は単一の管理トークンで保護されています。サーバー管理者のログイン（Discord OAuth2）は
  今後のダッシュボードの役割です。
- 設定の問題は安定したコードと英語のメッセージで報告されます。その翻訳は未対応です。

## ライセンス

[GNU Affero General Public License v3.0](LICENSE)（`AGPL-3.0-only`）。改変版をネットワーク越しに
他人へ提供する場合は、そのソースコードを提供する義務があります。`/about` コマンドがこの
リポジトリを案内します。
