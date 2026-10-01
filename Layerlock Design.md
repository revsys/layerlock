# Layerlock Design

Sep 30, 2026 · @Frank Wiles

## Overview

Layerlock is a CLI that hashes a project's dependency files, base-image Dockerfile, and build settings into a Docker image tag, so CI rebuilds a base image only when those inputs change.

In GitHub Actions, pulling images from a remote registry has been slow because of the transfer time, and rebuilding every layer on every run is slow too. Layerlock computes a tag from those inputs, reuses the image when that tag already exists, and rewrites the application's Dockerfile `FROM` lines to point at it. The goal is to make all of this as automatic as possible.

## Config file

`.layerlock.toml` defines named base-image groups. Each group specifies:

- Dependency files: a list, since a project can need several files together.
- The Dockerfile used to build the base image.
- The build context and destination image repository.
- Build settings such as build arguments, target stage, and target platforms.

The config also identifies the application Dockerfiles whose marked `FROM` lines should be updated. These are separate from the base-image build recipes.

| Group | Dependency files |
| --- | --- |
| python-base | `pyproject.toml`, `uv.lock` (or `requirements.txt`) |
| frontend-base | `package.json`, `package-lock.json` |

Each group can appear on multiple marked `FROM` lines, including across application Dockerfiles.

## Hashing

Each group's tag is the SHA-256 of its dependency files, base-image Dockerfile, and effective build settings. Changing the recipe or build settings must invalidate the tag even if dependency files are unchanged.

1. Read the configured dependency files in sorted, normalized relative-path order, so the result never depends on filesystem quirks or checkout location.
2. Include the base-image Dockerfile's contents and a canonical representation of effective build settings, including build context, build arguments, target stage, and target platforms. Sort map keys and normalize defaults.
3. Encode input types, paths, and raw file contents using a versioned, unambiguous format with explicit lengths. Do not simply concatenate file contents, since different file boundaries can otherwise produce identical inputs.
4. Hash the encoded inputs with SHA-256.
5. Use the digest in the image tag, for example `your-registry/python-base:sha256-abc123` (digest abbreviated here).

SHA-256 replaces the MD5 or SHA-1 first considered. It is fast enough that hashing a few small files is trivial. Do not include generated application Dockerfile references in the hash.

## Dockerfile marker

The marker is a comment on its own line above the `FROM` line, not a trailing comment on it.

```dockerfile
# layerlock: python-base
FROM your-registry/python-base:sha256-abc123
```

Every managed `FROM` line must have its own marker. The tool scans for markers, computes each group's hash once, and updates all marked occurrences. Unmarked `FROM` lines are left alone; group ownership is never inferred. Registry checks and builds are deduplicated by expected image reference.

Above-the-line was chosen for two reasons:

- Trailing comments get fragile on multi-stage lines such as `FROM python:3.12 AS builder`, where the comment has to survive after the alias.
- A separate line is easier to parse with a line scanner and reads naturally as an annotation for the line below.

## Implementation

Build the CLI in Rust using Clap's derive API for argument parsing, help, and version output. Enable Clap's `derive` and `env` features. Declare environment-backed arguments with `#[arg(env = "LAYERLOCK_...")]`, use typed value parsers and enums, and show supported environment variable names in help.

## Proposed CLI

```text
layerlock [OPTIONS] <COMMAND>

layerlock build [OPTIONS] [--force]
layerlock check [OPTIONS] [--force]
layerlock sync  [OPTIONS] [--force]
```

### Shared options and environment variables

Shared runtime options are global Clap arguments, accepted before or after the subcommand.

| Option | Environment variable | Default | Behavior |
| --- | --- | --- | --- |
| `-c, --config <PATH>` | `LAYERLOCK_CONFIG` | `.layerlock.toml` | Select the config file. |
| `-g, --group <NAME>` | `LAYERLOCK_GROUPS` | All configured groups | Select groups; repeat the option or use a comma-separated list. |
| `--registry-timeout <SECONDS>` | `LAYERLOCK_REGISTRY_TIMEOUT` | `30` | Positive integer; deadline for each registry HTTP request, including token requests, not for builds. |
| `--registry-concurrency <N>` | `LAYERLOCK_REGISTRY_CONCURRENCY` | `8` | Positive integer; maximum concurrent image availability lookups, not parallel builds. |
| `--output <FORMAT>` | `LAYERLOCK_OUTPUT` | `human` | `human` or `json`. |
| `--color <WHEN>` | `LAYERLOCK_COLOR` | `auto` | `auto`, `always`, or `never`; JSON output is never colorized. |
| `-v, --verbose` | None | Off | Include diagnostic details on stderr, never credentials or tokens. |

`-h, --help` and `-V, --version` provide standard Clap help and version output.

Resolution rules:

- Explicit CLI arguments override environment variables, which override built-in defaults.
- CLI group selections replace the entire environment-provided group list, rather than merging with it. `LAYERLOCK_GROUPS` is comma-separated; deduplicate selected names. Empty or unknown group names are errors.
- Relative config paths are resolved from the invocation directory. Paths inside the config are relative to the config file's directory. A missing or unreadable config file is an error, not a reason to fall back to another file.
- Invalid effective environment values fail just like invalid CLI values; do not silently fall back to defaults. Empty path, numeric, or enum values are invalid.
- With color mode `auto`, respect a nonempty `NO_COLOR`, terminal detection, and redirected output. An explicit color mode from CLI or `LAYERLOCK_COLOR` takes precedence over automatic detection.
- Reporting and registry-tuning options do not affect image hashes.

Keep `--force` CLI-only: inherited environment settings must not silently opt into rebuilding and overwriting existing tags. `check --force` remains read-only.

### Output contract

Human output reports each group's expected image reference and planned or completed action; `check` also prints unified Dockerfile diffs. Diagnostics and build subprocess logs go to stderr.

`--output json` emits one versioned JSON report on stdout for scripting. Include the command, selected groups, expected references, planned/completed image actions, Dockerfile changes with diffs, whether work is needed, and any operational errors. Keep progress messages, credentials, and build logs out of the JSON stream. A partial failure must not be presented as a complete successful result. Clap help, version output, and argument errors retain their normal format.

Output format and verbosity do not change exit codes or command behavior.

### `layerlock build`

Ensure the selected base images exist in the remote registry. Compute the expected tags, reuse published images, and build and push missing ones. Do not modify application Dockerfiles.

`--force` rebuilds and pushes every selected image even if its expected tag already exists. This does not implicitly mean `--no-cache` or refreshing upstream base images. It can replace an existing tag, so it is incompatible with registries that enforce tag immutability; failures must be reported rather than silently skipped.

Report each group's expected image reference and whether it was reused, pushed from local storage, or built and pushed.

### `layerlock check` — dry run of `sync`

Use the same validation and planning path as `sync`, but only report what would happen:

1. Validate configuration and marked Dockerfiles, read all hash inputs, and compute expected references.
2. Query the registry using existing Docker credentials, and inspect local image metadata where needed to make the same reuse/build/push decisions as `sync`.
3. Report each selected group's expected reference and planned action: reuse a published image, push a verified matching local image, or build and push a missing image.
4. Print unified diffs of every proposed `FROM` change. Preserve stage aliases, `--platform` options, and unrelated Dockerfile content.

Registry checks are always included; there is no separate `--registry` option. Report missing images even when the application Dockerfile references are already correct, and report stale references even when the expected images are already published.

Never pull layers, build images, push images, or write files. Mutation steps are described, not executed: this checks the current plan, not whether a future build or push will succeed. Authentication failures and other operational errors must be reported rather than presented as a complete successful plan.

`--force` previews `sync --force`: report the rebuilds and pushes that would be forced, without performing them.

Exit codes:

- `0`: the check completed and no builds, pushes, or Dockerfile edits are needed.
- `1`: the check completed and work is needed, including rebuilds requested by `--force`, even if there are no `FROM` diffs.
- `2`: an operational or argument error prevented a complete check. Errors take precedence over pending work.

For `build` and `sync`, return `0` on success (including no-op runs) and `2` on operational or argument errors; do not forward subprocess exit codes that would blur this distinction.

### `layerlock sync` — main CI workflow

Run the complete workflow in one command:

1. Use the same read-only planner as `check` to validate configuration and marked Dockerfiles, compute expected references, check image availability, and plan image actions and edits before doing build work.
2. Ensure every selected image is published, using the same behavior as `build`.
3. Only after all required builds and pushes succeed, update all selected marked `FROM` lines in working-tree Dockerfiles in place and report the changes. Do not generate separate Dockerfiles or automatically commit changes.

`--force` has the same meaning as for `build`. Validate and prepare all edits before writing; replace each changed Dockerfile atomically. This is not a cross-file transaction, so report any partial write failure clearly. Build or push failures leave Dockerfiles untouched.

Repeated successful runs with unchanged inputs should skip builds and leave Dockerfiles unchanged. An alternative name is `prepare`, but `sync` communicates bringing both published images and Dockerfile references into agreement.

```sh
# Typical CI: prepare dependency images and references, then build the app.
layerlock sync
docker build -t my-app .

# Preview the complete sync workflow, including registry checks and FROM diffs.
layerlock check

# Preview a forced rebuild and sync without changing anything.
layerlock check --force

# Publish only; leave application Dockerfiles alone.
layerlock build --group python-base

# Use CI environment defaults, including a custom config location.
export LAYERLOCK_CONFIG=ci/layerlock.toml
export LAYERLOCK_GROUPS=python-base,frontend-base
export LAYERLOCK_REGISTRY_TIMEOUT=60
layerlock sync

# CLI selection overrides the environment; emit a report for automation.
# Exit 1 still means work is needed, not that reporting failed.
layerlock check --group python-base --output json
```

## Rebuild decision and fast registry checks

The registry is the source of truth for publication. Check the expected remote tag first using the Registry HTTP API V2: `HEAD /v2/<repository>/manifests/<tag>`, accepting OCI and Docker manifest/index media types. This retrieves no image layers and requires no Docker daemon. If a registry does not support `HEAD`, fall back to a manifest `GET`, still without downloading layers.

Reuse standard Docker credential configuration: honor `DOCKER_CONFIG`, otherwise use `~/.docker/config.json`, including per-registry `credHelpers`, the default `credsStore`, and inline `auths`. Invoke credential helpers as needed and support registry authentication challenges/token exchange. Existing Docker login credentials should normally be sufficient, including in CI. Do not log credentials or tokens. Other environment-based credential formats, such as `DOCKER_AUTH_CONFIG`, would need explicit support rather than being assumed to work automatically.

Deduplicate identical reference checks, bound concurrency with `--registry-concurrency`, apply `--registry-timeout` to registry HTTP requests, and reuse tokens in memory. Treat an authenticated manifest-not-found response as missing; authentication failures, authorization failures, rate limits, and network errors are errors, not reasons to rebuild.

- Remote image exists: reuse it, unless `--force` was requested.
- Remote image is missing, but a local image is known to match the expected build inputs and platforms: push it and verify publication.
- Neither exists: build, push, and verify publication.

Local presence alone must never skip publication: a later CI runner cannot use an image that exists only on this machine. If local provenance or platform completeness cannot be established, build rather than trusting a matching local tag.

The tag comes from the dependency, recipe, and build-settings hash, so any branch or runner with identical inputs gets the same tag. Concurrent runners can still race and perform duplicate builds; the existence check is not a distributed lock.

## Open questions

- **Upstream refresh:** Mutable upstream base tags need an explicit refresh policy; `--force` alone does not guarantee fresh upstream layers.
- **Additional build inputs:** Files consumed by a base-image recipe beyond the dependency files and Dockerfile also affect its output. Should users explicitly list all such files, or should Layerlock hash the build context with `.dockerignore` rules applied? Hashing only the context path does not detect changes to its contents.

