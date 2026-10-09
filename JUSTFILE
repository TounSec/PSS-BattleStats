default:
    @just --list

# Run the GUI
run:
    cargo run -p pss-gui

# Format check + clippy + tests (same checks as the CI)
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked
    cargo test --workspace --locked

# Format the whole workspace
fmt:
    cargo fmt --all


# Usage: just release-prepare 0.1.1-beta.1      (nothing is pushed)
release-prepare version:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ ! "{{version}}" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
        echo "Version must look like X.Y.Z or X.Y.Z-suffix (got '{{version}}')"; exit 1
    fi
    [ "$(git branch --show-current)" = "dev" ] || { echo "Switch to the dev branch first"; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "Working tree is not clean: commit or stash first"; exit 1; }

    cargo fmt --all -- --check
    cargo test --workspace --locked

    sed -i '0,/^version = ".*"/s//version = "{{version}}"/' Cargo.toml
    cargo update --workspace
    git add Cargo.toml Cargo.lock
    git commit -m "chore: bump version to {{version}}"

    echo
    echo "Version bumped and committed locally. Next:"
    echo "  1. git push origin dev, wait for the CI to pass"
    echo "  2. merge dev into main (PR or local merge) and push main"
    echo "  3. just release-tag {{version}}      (from main)"

# Usage: just release-tag 0.1.1-beta.1
[confirm("Create the tag and push it to origin? This starts the release build.")]
release-tag version:
    #!/usr/bin/env bash
    set -euo pipefail
    [ "$(git branch --show-current)" = "main" ] || { echo "Switch to the main branch first"; exit 1; }
    [ -z "$(git status --porcelain)" ] || { echo "Working tree is not clean"; exit 1; }
    git fetch origin
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "main is not in sync with origin/main: push or pull first"; exit 1; }

    cargo_v=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "(.*)"/\1/')
    [ "$cargo_v" = "{{version}}" ] || { echo "Cargo.toml says '$cargo_v', not '{{version}}'"; exit 1; }
    ! git rev-parse "v{{version}}" >/dev/null 2>&1 || { echo "Tag v{{version}} already exists"; exit 1; }

    git tag -a "v{{version}}" -m "Release {{version}}"
    git push origin "v{{version}}"
    echo "Tag v{{version}} pushed: follow the build in the Actions tab."
