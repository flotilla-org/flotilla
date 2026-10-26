//! The image harness environment an agentless session takes (#3655), read from the stdlib image
//! layers' fragments. The layers themselves are built-in definitions from the code
//! (`flotilla_resources::stdlib`, #3824 layer 2a).
use flotilla_resources::stdlib::{EmbeddedLayerDirectory, STDLIB_IMAGE_LAYERS};

/// #3655: a bare host's sessions take the crew image's harness environment
/// (auto-updater and installation checks off, no update nags, the image
/// locale) from these crew-runtime layers' fragments, so a bare host and a
/// vessel read one definition.
const HARNESS_ENVIRONMENT_LAYERS: &[&str] = &["base-ubuntu-24.04", "crew-utilities", "harness-claude-code", "harness-codex"];
/// The host's own facilities stay the host's: the image's search path and
/// login shell describe the image, not the host.
const HOST_OWNED_ENVIRONMENT: &[&str] = &["PATH", "SHELL"];

static HARNESS_SESSION_ENVIRONMENT: std::sync::LazyLock<Vec<(String, String)>> =
    std::sync::LazyLock::new(|| harness_environment_from(STDLIB_IMAGE_LAYERS));

/// The image harness environment an agentless session receives.
pub(crate) fn harness_session_environment() -> &'static [(String, String)] {
    &HARNESS_SESSION_ENVIRONMENT
}

/// Literal `ENV NAME=value` assignments of the harness layers. Values that
/// expand variables or name image-only paths (`/opt/flotilla`) cannot hold on
/// a bare host and are left out with the host-owned names. Only the
/// single-assignment form is read: an unquoted value holding whitespace (a
/// multi-pair `ENV A=1 B=2` line, or the legacy `ENV A 1` form) is skipped
/// rather than mis-split, and the pinned test below then reports the loss.
fn harness_environment_from(directories: &[EmbeddedLayerDirectory]) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = Vec::new();
    for layer in HARNESS_ENVIRONMENT_LAYERS {
        let Some(directory) = directories.iter().find(|directory| directory.directory == *layer) else { continue };
        let Some((_, fragment, _)) = directory.files.iter().find(|(path, _, _)| *path == flotilla_resources::LAYER_FRAGMENT_FILE) else {
            continue;
        };
        for line in String::from_utf8_lossy(fragment).lines() {
            let Some((name, value)) = line.trim().strip_prefix("ENV ").and_then(|assignment| assignment.trim().split_once('=')) else {
                continue;
            };
            let value = value.trim();
            let value = match value.strip_prefix('"').and_then(|value| value.strip_suffix('"')) {
                Some(quoted) => quoted,
                None if value.contains(char::is_whitespace) => continue,
                None => value,
            };
            let literal_name = !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if !literal_name || HOST_OWNED_ENVIRONMENT.contains(&name) || value.contains('$') || value.contains("/opt/flotilla") {
                continue;
            }
            environment.retain(|(existing, _)| existing != name);
            environment.push((name.to_string(), value.to_string()));
        }
    }
    environment
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3655 contract 3: agentless sessions get exactly the image's harness
    // environment, read from the shipped layer fragments. A new literal ENV in
    // a harness layer fails here, so reaching bare hosts is a decision.
    #[test]
    fn shipped_harness_layers_define_the_agentless_session_environment() {
        let mut environment = harness_session_environment().to_vec();
        environment.sort();
        assert_eq!(
            environment,
            [("DISABLE_AUTOUPDATER", "1"), ("DISABLE_INSTALLATION_CHECKS", "1"), ("GH_NO_UPDATE_NOTIFIER", "1"), ("LANG", "en_US.UTF-8"),]
                .map(|(name, value)| (name.to_string(), value.to_string()))
        );
    }

    // Image-only values (expansions, `/opt/flotilla` paths) and host-owned
    // names never reach a bare host; a later layer's value wins; layers outside
    // the harness set and malformed or multi-pair lines contribute nothing.
    #[test]
    fn harness_environment_keeps_only_literal_host_applicable_assignments() {
        static LAYERS: &[EmbeddedLayerDirectory] = &[
            EmbeddedLayerDirectory {
                directory: "base-ubuntu-24.04",
                files: &[(
                    "Dockerfile.fragment",
                    b"FROM ${BASE}\nENV SHELL=/bin/bash\nENV LANG=C.UTF-8\nENV TERMINFO_DIRS=/opt/flotilla/share/terminfo\nENV PATH=\"/usr/bin\"\nENV broken\nENV A=1 B=2\n",
                    false,
                )],
            },
            EmbeddedLayerDirectory {
                directory: "harness-claude-code",
                files: &[("Dockerfile.fragment", b"ENV PIN=${PIN_CLAUDE_CODE_VERSION}\nENV LANG=\"en_GB.UTF-8\"\n", false)],
            },
            EmbeddedLayerDirectory { directory: "display-x11", files: &[("Dockerfile.fragment", b"ENV LIBGL_ALWAYS_SOFTWARE=1\n", false)] },
        ];
        assert_eq!(harness_environment_from(LAYERS), vec![("LANG".to_string(), "en_GB.UTF-8".to_string())]);
    }
}
