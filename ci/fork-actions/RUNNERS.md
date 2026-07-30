# Forgejo fork runner setup

This is an operator checklist, not automation. Do not create forks, request
registration tokens, register runners, or install workflows from an agent run.
Complete those HITL steps only after the lab fork organization/naming decision
has been made.

The workflow files intentionally use generic labels. Concrete instance URLs,
registration tokens, Docker image coordinates, signing identities, and SDK
paths belong only in Forgejo and runner-side configuration.

Current upstream references:

- [Forgejo Runner installation](https://forgejo.org/docs/latest/admin/actions/installation/)
- [runner registration](https://forgejo.org/docs/latest/admin/actions/registration/)
- [runner labels and execution types](https://forgejo.org/docs/latest/admin/actions/configuration/)
- [Forgejo release API overview](https://forgejo.org/docs/latest/user/releases/)

## 1. Create and prepare the forks

For each selected source:

1. Create the lab fork using the operator-selected organization and naming.
2. Enable the Actions unit in the fork.
3. Keep GitHub as the issue/PR-gate surface during the transition; do not copy
   these release workflows into GitHub's `.github/workflows/`.
4. Obtain repository- or organization-scoped registration values from
   **Settings → Actions → Runners**. Do not store them in this repository.
5. Verify the instance's `repository.release.ALLOWED_TYPES` setting is empty,
   `*/*`, or otherwise accepts extensionless binaries, `.wasm`, `.json`, and
   `.tar.gz` assets. The publisher uploads each asset in its own API request.

## 2. Linux runner on `udder`

`udder` is the always-on silo guest and is the Linux release runner host.
`feta` is not a release runner because it sleeps.

### Docker-in-LXC prerequisite

Before installing the runner, the hypervisor operator must:

1. enable LXC nesting (and the companion key-management capability required by
   Docker) for the `udder` guest;
2. allocate enough CPU, memory, and disk for Rust and Zig release builds;
3. install and start Docker inside the guest;
4. grant the dedicated runner service account access to the Docker socket;
5. prove nested execution as that account with a disposable container.

The hypervisor change is HITL. Do not try to work around missing nesting by
switching the workflow to host execution.

Install a Forgejo Runner version compatible with the lab Forgejo instance,
generate its default configuration, and register it using the scoped value from
the fork or owning organization. Follow the current Forgejo binary installation
and registration documentation rather than copying a token into a shell
history.

Configure the runner-side label mapping as:

```yaml
runner:
  labels:
    - fleet-release-linux-x64:docker://<operator-selected-build-image>
```

The private or changeable image coordinate stays in this runner configuration;
the workflow sees only `fleet-release-linux-x64`. The selected job image must
provide:

- Bash, Git, curl, jq, coreutils, a C/C++ toolchain, and Node.js for the
  fully-qualified checkout Action;
- Rust stable with `rustup`, including permission to add `wasm32-wasip1`;
- Zig 0.15.2 for Cleat;
- enough writable space for Cargo, Ghostty, Zellij, and `target/` caches.

Start the runner as a supervised service and confirm the fork's runner page
shows `fleet-release-linux-x64` online.

## 3. Darwin signing runner on `comte`

Install and register a second compatible Forgejo Runner on `comte`. Configure
host execution because codesigning needs the local keychain:

```yaml
runner:
  labels:
    - fleet-release-darwin-arm64-signing:host
```

The dedicated service account must provide Bash, Git, curl, jq, Node.js, Rust
stable, Zig 0.15.2, Xcode command-line tools, and `codesign`. Its service
environment must set:

- `FLEET_CODESIGN_IDENTITY` to the standard CLI signing identity;
- `FLEET_MACOS_SDK` to the compatible SDK directory used by the Cleat build.

Do not put either value into a workflow or repository secret. Verify from the
runner's non-interactive service context that the identity is visible, a copied
system binary can be signed and strictly verified, and the SDK directory is
readable.

## 4. Activate and prove

1. Copy `runtime/` and the repository's template workflows as described in the
   README.
2. Review the activation commit for concrete instance URLs, tokens, private
   image coordinates, or signing values; none should be present.
3. Bring up the Linux runner first and prove one controlled `main` push creates
   a complete Forgejo draft/release for a single-cohort repository.
4. Bring up the Darwin runner and prove Flotilla/Cleat Linux and Darwin cohorts
   converge on one `fleet-<full-sha>` release.
5. Download each asset, verify its metadata SHA-256, verify Darwin signatures,
   and check the Flotilla binary wire generation.
6. Only then allow slice 2 to pin the Forgejo release assets.

Runner cache pruning and service monitoring are ongoing operator duties. The
workflow deliberately does not build or push a crew image.
