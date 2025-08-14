# Contributing

## Development environment

This repo contains a full [Nix] devleopment flake. It provides a reproducible rust toolchain,
utilities like the sea-orm CLI, and more. This should be used if possible, but it isn't necessarily
required.

Because the backend requires a PostgreSQL database and s3 bucket (even for development),
[`./scripts/start-dev-env.sh`](scripts/start-dev-env.sh) is provided to start a temporary PostgreSQL
server and S3-compatible API locally. The data for these services is stored in `./.local`, so to
reset the stored datam this directory can just be deleted. The S3 API is hosted at `127.0.0.1:8081`
(with a bucket `local`), and postgres is run on the default port `5432` with a database named
`local`.

The script also populates the database and S3 bucket with some example data, to make developing
simple.

If the Nix devshell is used, the script should work out of the box. If not, then make sure you have
the following dependencies installed:

- curl
- jq
- rclone
- postgresql

## Stack

### Codebase

This backend is written in rust, and uses [axum] (with [aide] for OpenAPI documentation) to make an
HTTP server. [bpaf] is used as a light CLI parser for configuration. For logging [tracing] is used,
with [tracing-subscriber] as the logging implementatation. The subscriber will read a `RUST_LOG`
variable from the environment, to allow easy log filtering.

To interact with the database, [sea-orm] is used with migrations in
[`./database/migrations`](database/migrations) and generated models in
[`./database/entities`](database/entities). The entities directory has its own README with
information about how to keep it up to date.

All Tebex-related code such as webhook parsing/validating, and API clients is located in
[`./crates/tebex`](crates/tebex), as it will likely be published to https://crates.io as an
independent library at some point, so make sure any code in that directory is high-quality enough to
be published, and doesn't depend on or contain any Poly+ specific code.

### Database

The backend requires a PostgreSQL database to store data in. A database schema is automatically
initialized, so all that needs created is a new, empty database for the backend to use.

[aide]: https://lib.rs/aide
[axum]: https://lib.rs/axum
[bpaf]: https://lib.rs/bpaf
[nix]: https://nixos.org
[sea-orm]: https://lib.rs/sea-orm
[tracing]: https://lib.rs/tracing
[tracing-subscriber]: https://lib.rs/tracing-subscriber
