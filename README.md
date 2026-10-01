# Layerlock

Content-addressed Docker dependency images. Layerlock hashes dependency files, a base-image recipe, and build settings into an image tag, publishes that image only when needed, and updates explicitly marked application stages.

See [Layerlock Design.md](Layerlock%20Design.md) for the design and open questions.

## Install and use

Download a binary and its `.sha256` checksum from GitHub Releases for Linux x86_64 (GNU or musl), macOS (Intel or Apple Silicon), or Windows x86_64. On Linux/macOS, verify with `shasum -a 256 -c <asset>.sha256`, rename the binary to `layerlock`, and run `chmod +x layerlock`. On Windows, compare `Get-FileHash <asset> -Algorithm SHA256` with the checksum file and rename the binary to `layerlock.exe`. Place the binary on your PATH.

Alternatively, install from source with Rust using `cargo install --path . --locked`.

Publishing requires Docker with the buildx plugin and a configured builder. Registry checks and published-image reuse do not require a Docker daemon; local single-platform image inspection does.

```sh
cargo install --path . --locked

# Preview the full workflow, including registry checks and Dockerfile diffs.
layerlock check

# Publish dependency images, then atomically update marked Dockerfiles.
layerlock sync
docker build -t my-app .

# Publish only; never read or modify application Dockerfiles.
layerlock build --group python-base

# A force preview is still read-only.
layerlock check --force --output json
```

`--force` rebuilds and pushes even if the expected tag exists. It does **not** set `--no-cache` or `--pull`. Immutable registries may reject the push; Layerlock reports the failure and leaves Dockerfiles untouched.

## Configuration

Paths inside `.layerlock.toml` are relative to that file's directory, not the invocation directory.

```toml
dockerfiles = ["Dockerfile", "ci/Dockerfile"]

[groups.python-base]
dependencies = ["pyproject.toml", "uv.lock"]
dockerfile = "Dockerfile.python-base"
repository = "ghcr.io/your-org/python-base"
context = "."
build-args = { PYTHON_VERSION = "3.12" }
target = "dependencies"
platforms = ["linux/amd64", "linux/arm64"]
```

`context` defaults to `.`; build arguments and platforms default to empty, and target defaults to unset. Empty platforms use the buildx builder's default. Explicit platforms are recommended when sharing tags across runners with different architectures.

Paths are lexically normalized; dependency files and platform lists are sorted and deduplicated. Unknown config fields are errors. Config paths must be relative and cannot lexically escape the config directory. Repositories must be untagged. Managed application Dockerfiles must be regular files, not symlinks, and cannot also be dependency hash inputs or base-image recipes.

Annotate every managed stage separately:

```dockerfile
# layerlock: python-base
FROM --platform=$BUILDPLATFORM old/base:tag AS builder
```

The marker must immediately precede a single-line `FROM`. Continued managed `FROM` instructions and trailing comments are rejected rather than rewritten unsafely. Aliases, platform options, whitespace, line endings, and unmarked stages are preserved. Unselected groups are not rewritten, but all markers are validated during `check`/`sync`.

A small example lives in `examples/minimal/`. Change its placeholder repository to one you can publish to before running it:

```sh
layerlock check --config examples/minimal/.layerlock.toml
```

## Runtime options

Options are global and work before or after the subcommand. CLI arguments override environment values, which override defaults. CLI group selections replace the entire environment group list.

| Option | Environment | Default |
| --- | --- | --- |
| `-c, --config` | `LAYERLOCK_CONFIG` | `.layerlock.toml` |
| `-g, --group` (repeatable/comma-separated) | `LAYERLOCK_GROUPS` | all groups |
| `--registry-timeout` | `LAYERLOCK_REGISTRY_TIMEOUT` | 30 seconds/request |
| `--registry-concurrency` | `LAYERLOCK_REGISTRY_CONCURRENCY` | 8 lookups |
| `--output` | `LAYERLOCK_OUTPUT` | `human` (`human`, `json`) |
| `--color` | `LAYERLOCK_COLOR` | `auto` (`auto`, `always`, `never`) |
| `-v, --verbose` | — | off |

`--force` is CLI-only. Timeouts/concurrency must be positive integers. Invalid effective environment values are errors. Automatic color respects terminal detection and nonempty `NO_COLOR`; JSON is never colorized.

## Publication and authentication

Layerlock uses Registry HTTP API V2 `HEAD` manifest requests, accepting OCI and Docker manifests/indexes. HTTP `405`/`501` triggers manifest `GET` fallback. No layer downloads or Docker pulls are performed. Identical references are deduplicated, registry lookups have bounded concurrency, and each HTTP request (including token requests) has its own deadline. Builds execute sequentially and are not subject to the registry timeout.

Docker credentials come from `$DOCKER_CONFIG/config.json`, or `~/.docker/config.json`. Per-registry `credHelpers` override the default `credsStore`, which overrides inline `auths`. Basic authentication, Bearer token exchange, and identity-token refresh are supported; credentials and tokens are cached only in memory. Authentication failures, rate limits, failed helpers, invalid credentials, and network errors are operational errors, not missing images. `DOCKER_AUTH_CONFIG` is not supported.

Registry endpoints use HTTPS. Loopback registries (`localhost`, loopback IPs) use HTTP to support local Registry V2 development. Non-loopback insecure registries and HTTP redirects are not supported. Token endpoints must use HTTPS except for loopback endpoints challenged by a loopback registry. Credential helper subprocesses follow Docker's protocol; helper output is never logged.

For each image:

1. Reuse the published tag, unless forced.
2. If remote is missing, push a verified matching local image when possible.
3. Otherwise run `docker buildx build --push`, then verify publication by a fresh manifest lookup.

Local reuse requires exactly one explicit platform, matching inspected OS/architecture/variant, and all of these image labels:

- `io.layerlock.input-digest`: the complete input digest
- `io.layerlock.hash-version`: `1`
- `io.layerlock.platforms`: JSON encoding of the normalized configured platform list

Layerlock adds these labels to builds. A previously built image pulled into local storage can therefore be reused. Unlabelled images, unknown default platforms, and local multi-platform completeness that cannot be established cause a rebuild. Labels are provenance conventions, **not cryptographic attestations**; do not trust images from hostile local sources. Local metadata inspection is read-only. Provenance is rechecked before a local push.

`sync` validates the complete plan before publishing, and starts Dockerfile writes only after all required publication succeeds. Every edit is staged before any replacement, permissions are preserved, and each replacement is atomic. This is **not a cross-file transaction**: a later write failure reports which prior edits completed. Concurrent edits to Dockerfiles are detected before replacement; configured hash inputs are rechecked before/after image actions. Avoid changing build inputs during a run; this does not snapshot the build context or provide a distributed lock.

## Output and exit codes

Human output reports expected references and image actions. `check` prints unified diffs. Diagnostics and build subprocess logs go to stderr; Layerlock does not log credential/helper responses, tokens, or command-line build argument values.

`--output json` emits one report on stdout, with `schema_version: 1`, command, selected groups, expected references/digests, image actions and completion flags, Dockerfile diffs and completion flags, `work_needed`, `complete`, and `errors`. Image actions are `reuse`, `push_local`, `build_and_push`, `force_build_and_push`, or `availability_unknown` when planning fails. `work_needed` describes the initial plan, not remaining work; `null` means it could not be determined. `complete: false` and errors distinguish partial failures from success. A `check` report contains planned actions, so its completion flags remain false even when the plan is complete.

| Command | `0` | `1` | `2` |
| --- | --- | --- | --- |
| `check` | no work needed | builds, pushes, or edits needed | operational/argument error |
| `build`, `sync` | success (including no-op) | — | operational/argument error |

Errors take precedence over pending work. Subprocess exit codes are not forwarded. Clap help, version output, and argument errors keep their standard format.

## Hash boundaries and open questions

Hash format v1 uses a domain prefix and explicit big-endian byte lengths for each input type, normalized path, and raw contents, followed by canonical JSON build settings. Tags use the full SHA-256 digest.

Dependency bytes, input paths, recipe bytes, context **path**, arguments, target, and platforms affect the hash. The publication repository, application Dockerfiles, reporting options, registry tuning, and checkout's absolute location do not.

Context **contents** are not automatically hashed: list every other recipe input in `dependencies`. Applying `.dockerignore` during automatic context hashing and refreshing mutable upstream base tags remain design questions. Concurrent runners may race and build the same tag; availability checks are not a lock.

## Development

GitHub Actions runs tests, formatting, and Clippy checks on pushes and pull requests to `main`. Pushing a `v*` tag (for example, `v0.1.0`) runs tests and publishes binaries and SHA256 checksums for all five platform targets to GitHub Releases.

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use loopback mock registries, isolated Docker credential files/helpers, fake Docker subprocesses, and injected filesystem/backend failures. They do not publish real images or require a running Docker daemon. Live registry/buildx interoperability should also be validated against your CI builder and registry before deployment.
