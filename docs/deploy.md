# Building, releasing and running

Nothing has to be compiled on a personal machine. GitHub Actions builds, tests and packages the
bot; a computer only downloads the result and runs it. A local build of the whole workspace fills
`target/` with gigabytes, so the rule is: **do not build locally unless you are changing code**.

## What runs on GitHub

| Workflow | When | What |
| --- | --- | --- |
| `CI` (`.github/workflows/ci.yml`) | every push to `main`, every pull request, weekly, by hand | `fmt`, `clippy -D warnings`, the tests against a real PostgreSQL, the generated references (`closure-xtask docs --check`) and `cargo deny` as parallel jobs. A newer push cancels the running check of the same branch. The cache is written by `main` only. The weekly run exists because new security advisories appear without any change here. |
| `Release` (`.github/workflows/release.yml`) | a tag `v*`, or by hand | Release binaries for Linux x86_64, Linux aarch64 and macOS arm64 (`tar.gz` and a `sha256` each). A tag also makes a GitHub release with them and publishes the container image to `ghcr.io/fokiiiiiii/closure` (amd64 and arm64). A manual run only builds the binaries and keeps them for 14 days as a preview, even when it is started on a tag; it never makes a release or publishes the image. |
| Dependabot (`.github/dependabot.yml`) | weekly | One pull request for minor and patch updates of the Cargo tree, one for the actions. |

## Getting the bot

A released version:

```bash
gh release download --repo Fokiiiiiii/closure --pattern '*aarch64-apple-darwin.tar.gz*'
```

The newest `main`, without a release (a preview build):

```bash
gh workflow run release.yml --repo Fokiiiiiii/closure --ref main
gh run watch --repo Fokiiiiiii/closure   # pick the run you just started
gh run download <run-id> --repo Fokiiiiiii/closure -n closure-aarch64-apple-darwin
```

Check the file with its `.sha256` (`shasum -a 256 -c closure-*.tar.gz.sha256`), unpack it, and run
the `closure` binary inside. The Linux binaries are built on Ubuntu 22.04, so they run on systems
with glibc 2.35 or newer.

## Running it

The bot reads its settings from the environment or from flags (`closure --help`):

| Variable | Meaning |
| --- | --- |
| `DISCORD_TOKEN`, `DISCORD_APPLICATION_ID` | The bot's identity (secret: never commit it). |
| `DATABASE_URL` | The PostgreSQL database (secret). Migrations run at start. |
| `API_BIND` | Where the admin API listens, `127.0.0.1:8080` by default. |
| `API_ADMIN_TOKEN` | Token of the admin API (secret, at least 32 characters). |
| `BOT_CONFIG` | A bot-wide configuration file (see [configuration](configuration.md)). |
| `COMMAND_GUILD` | Publish the slash commands to this one server only (development). |
| `MESSAGE_HISTORY_DAYS` | How long the message history is kept, 7 by default, 0 = never record. |
| `LOG_FORMAT` | `pretty` or `json`. |

Privileged gateway intents that some modules need (Server Members for rejoin handling and
antiraid, Message Content for automod and text commands) must be switched on for the bot in the
Discord Developer Portal, or the bot cannot connect.

### Container

```bash
docker run --rm --env-file .env -p 127.0.0.1:8080:8080 -e API_BIND=0.0.0.0:8080 \
  ghcr.io/fokiiiiiii/closure:latest
```

Only publish the admin API port to a trusted network or put it behind a reverse proxy.

## Making a release

1. Make sure `main` is green and `Cargo.toml`'s `version` is the one to release.
2. `git tag v0.2.0 && git push origin v0.2.0`.
3. The `Release` workflow builds the three binaries, creates the release with generated notes and
   publishes the image. Check the run; delete the release and the tag if it must be redone.

## Keeping the local disk small

- Work with `cargo check -p <crate>` and `cargo test -p <crate>` instead of the whole workspace;
  CI runs everything.
- The dev profile already drops incremental caches and debug information of dependencies. If
  `target/` still grows, `cargo clean` is always safe: everything in it is rebuilt on demand.
- Release builds are never needed locally; use the downloads above.
