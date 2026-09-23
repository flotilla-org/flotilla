#!/usr/bin/env python3
"""Canonical validation for fleet candidates and promoted generations."""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path, PurePosixPath

PLATFORMS = ("linux-x86_64-gnu2.36", "darwin-aarch64")
SOURCE_NAMES = ("flotilla", "cleat", "mattpocock-skills", "rjw-skills")
SKILLS_PREFIX = ("share", "flotilla", "skills")
# Credential-free `CODEX_HOME` template seeding each crew's scratch
# (flotilla-org/flotilla#1913). `scripts/fleet-install` points
# `FLOTILLA_CODEX_HOME_TEMPLATE` at this directory inside the generation.
#
# Adding a payload path is a one-time crossing: by ADR 0037 the *installed*
# generation's validator verifies the incoming one, so the first generation
# carrying this directory is rejected as an unexpected path by every host still
# on a validator that predates this line. The refusal is safe — it happens
# before the flip, leaving the host on its working generation — but the
# crossing has to be made deliberately (stage the incoming generation and run
# its own `install.sh`, or point `FLEET_GENERATION_VALIDATOR` at the incoming
# validator), and the rehearsal's upgrade-from-previous leg will say so first.
CODEX_HOME_PREFIX = ("share", "flotilla", "codex-home")
CODEX_HOME_CONFIG_FILE = "config.toml"
# Host-owned credential material, delivered to crews separately as a read-only
# `0400` copy of the central login. A generation must never carry one.
CODEX_CREDENTIAL_FILE = "auth.json"
# The CODEX_HOME template is deliberately absent here: `build-candidate.sh`
# hard-requires it, so no generation can be *produced* without one, and by
# ADR 0037 this same validator also verifies the generation a failed health
# check rolls *back* to. Requiring the path would retroactively invalidate
# every generation built before it existed, so a host whose first template-
# carrying generation failed health confirmation could not roll back off it.
# Enforce at production, tolerate at consumption.
REQUIRED_PAYLOAD = {
    "bin/flotilla", "bin/flotillad", "bin/cleat", "install.sh", "generation_validation.py",
    "share/flotilla/skills/.flotilla-sources.json",
}
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")
DIGEST_PATTERN = re.compile(r"^[0-9a-f]{64}$")
GENERATION_PATTERN = re.compile(r"^(\d{8}T\d{6}Z-r\d+-f([0-9a-f]{12})-c([0-9a-f]{12}))$")


class ValidationError(ValueError):
    pass


def require_sources(value):
    if not isinstance(value, dict) or set(value) != set(SOURCE_NAMES):
        raise ValidationError("invalid source set")
    if any(not isinstance(pin, str) or not SHA_PATTERN.fullmatch(pin) for pin in value.values()):
        raise ValidationError("invalid source pin")
    return value


def require_digest(value, description="digest"):
    if not isinstance(value, str) or not DIGEST_PATTERN.fullmatch(value):
        raise ValidationError(f"invalid {description}")
    return value


def require_size(value, description="size", *, allow_zero=False):
    minimum = 0 if allow_zero else 1
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise ValidationError(f"invalid {description}")
    return value


def allowed_payload(path, platform=None):
    if path in REQUIRED_PAYLOAD:
        return True
    pure = PurePosixPath(path)
    library = (len(pure.parts) == 2 and pure.parts[0] == "lib"
               and (pure.suffix == ".dylib" or pure.name.endswith(".so") or ".so." in pure.name))
    if platform == "darwin-aarch64" and library:
        library = pure.suffix == ".dylib"
    codex_home = len(pure.parts) >= 4 and pure.parts[:3] == CODEX_HOME_PREFIX
    # The single chokepoint for the credential-free contract: every consumer of
    # a generation payload (candidate build, promotion, Darwin signing, release
    # verification, install) rejects a `CODEX_HOME` template carrying a
    # credential, not just the builder that assembled it. Every component is
    # checked, not just the leaf: `seed_scratch` copies the template wholesale,
    # so a *directory* named `auth.json` lands in the crew's home as one too.
    if codex_home and CODEX_CREDENTIAL_FILE in pure.parts[len(CODEX_HOME_PREFIX):]:
        return False
    return (library
            or codex_home
            or (len(pure.parts) >= 4 and pure.parts[:3] == SKILLS_PREFIX))


def validate_skill_bundle(document, sources):
    entries = document.get("sources") if isinstance(document, dict) else None
    if (not isinstance(document, dict) or set(document) != {"schema_version", "sources"}
            or document.get("schema_version") != 5 or not isinstance(entries, list) or not entries):
        raise ValidationError("invalid v5 skill bundle")
    names = set()
    for source in entries:
        if not isinstance(source, dict) or not {"name", "repository", "revision"}.issubset(source) or set(source) - {"name", "repository", "revision", "paths", "credential"}:
            raise ValidationError("invalid skill source")
        name = source.get("name")
        repository = source.get("repository")
        if (not isinstance(name, str) or not name or name in {".", ".."} or any(c in name for c in "/\\\r\n")
                or name in names or not isinstance(repository, str) or not repository
                or source.get("revision") != sources.get(name)):
            raise ValidationError("skill bundle source pins do not match the fleet generation")
        credential = source.get("credential")
        if credential is not None and (not isinstance(credential, str) or not credential or any(c in credential for c in "/\\\r\n")):
            raise ValidationError(f"skill source {name} has invalid credential")
        paths = source.get("paths", ["skills"])
        if (not isinstance(paths, list) or not paths or not all(isinstance(path, str) for path in paths)
                or len(paths) != len(set(paths))):
            raise ValidationError(f"skill source {name} has invalid or duplicate paths")
        for path in paths:
            pure = PurePosixPath(path) if isinstance(path, str) else None
            if (pure is None or not path or pure.is_absolute() or path.endswith("/") or any(c in path for c in "\\\r\n*?[]")
                    or any(part in {"", ".", ".."} for part in path.split("/"))):
                raise ValidationError(f"skill source {name} has invalid path: {path}")
        names.add(name)
    return entries


def validate_skill_source_paths(document):
    entries = document.get("sources") if isinstance(document, dict) else None
    pins = {source.get("name"): source.get("revision") for source in entries or [] if isinstance(source, dict)}
    if set(pins) != set(SOURCE_NAMES):
        raise ValidationError("invalid source set")
    entries = validate_skill_bundle(document, pins)
    with tempfile.TemporaryDirectory(prefix="fleet-skill-sources-") as temporary:
        for source in entries:
            name = source["name"]
            revision = source["revision"]
            paths = source.get("paths", ["skills"])
            checkout = Path(temporary) / name
            checkout.mkdir()
            commands = (
                (("git", "-C", str(checkout), "init", "--quiet"), None),
                (("git", "-C", str(checkout), "remote", "add", "origin", source["repository"]), None),
                (("git", "-C", str(checkout), "sparse-checkout", "set", "--no-cone", "--stdin"),
                 "".join(f"/{path}/\n" for path in paths)),
                (("git", "-C", str(checkout), "fetch", "--quiet", "--depth=1", "--filter=blob:none", "--no-tags", "origin", revision), None),
                (("git", "-C", str(checkout), "checkout", "--quiet", "--detach", "FETCH_HEAD"), None),
            )
            skipped = False
            # Non-interactive git everywhere: without this, an unauthenticated
            # fetch of a private source FAILS FAST on Linux but HANGS on macOS
            # (osxkeychain helper / terminal prompt), which stalled the Darwin
            # candidate lane for 70 minutes on 2026-08-26. The skip path below
            # only works if the failure is allowed to surface.
            git_env = dict(os.environ, GIT_TERMINAL_PROMPT="0", GIT_ASKPASS="/usr/bin/false", GCM_INTERACTIVE="never")
            for command, stdin in commands:
                if command[0] == "git":
                    command = command[:1] + ("-c", "credential.helper=") + command[1:]
                try:
                    subprocess.run(command, input=stdin, text=True, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=git_env)
                except subprocess.CalledProcessError as error:
                    detail = error.stderr.strip().splitlines()[-1] if error.stderr.strip() else "git command failed"
                    # Ruled 2026-08-26: the candidates job is credential-free, so
                    # sources requiring authentication cannot be verified here.
                    # Skip them LOUDLY by name; credentialed verification belongs
                    # to the rehearsal gate (#1804/#1805). Vessel staging remains
                    # the enforcement point for these sources.
                    if "could not read Username" in detail or "Authentication failed" in detail or "terminal prompts disabled" in detail:
                        print(
                            f"generation validation: SKIPPED unverifiable skill source {name} at {revision}: "
                            f"requires authentication and this job is credential-free ({detail})",
                            file=sys.stderr,
                        )
                        skipped = True
                        break
                    raise ValidationError(f"skill source {name} at pinned revision {revision} could not be fetched: {detail}") from error
            if skipped:
                continue
            resolved = subprocess.run(("git", "-C", str(checkout), "rev-parse", "FETCH_HEAD"), text=True,
                                      check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.strip()
            if resolved != revision:
                raise ValidationError(f"skill source {name} fetch did not resolve pinned revision {revision}")
            for declared_path in paths:
                path = checkout / declared_path
                if not path.is_dir():
                    raise ValidationError(f"skill source {name} declared path {declared_path} is missing at pinned revision {revision}")
                if not any(path.rglob("SKILL.md")):
                    raise ValidationError(f"skill source {name} declared path {declared_path} has no SKILL.md at pinned revision {revision}")


def validate_codex_home_template(root):
    """Gate the assembled `CODEX_HOME` template before it becomes payload.

    `CodexMaterialAdapter::seed_scratch` copies this directory into every
    crew's writable `CODEX_HOME`, so a credential at any depth (file *or*
    directory named `auth.json`), a symlink escaping the generation, or a
    special file here would reach every crew on the fleet.
    """
    root = Path(root)
    if not root.is_dir() or root.is_symlink():
        raise ValidationError("codex home template is missing or is not a directory")
    if not (root / CODEX_HOME_CONFIG_FILE).is_file():
        raise ValidationError(f"codex home template has no {CODEX_HOME_CONFIG_FILE}")
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if path.is_symlink():
            raise ValidationError(f"codex home template entry is a symlink: {relative}")
        if not path.is_file() and not path.is_dir():
            raise ValidationError(f"codex home template entry is not a regular file or directory: {relative}")
        # Directories are checked too, so an empty one named `auth.json` — which
        # contributes no payload file of its own — cannot slip past the walk.
        if not allowed_payload(str(PurePosixPath(*CODEX_HOME_PREFIX, *relative.parts))):
            raise ValidationError(f"codex home template must be credential-free, but carries {relative}")


def validate_generation(document, generation, platform=None, trusted_team="973L4GV58R", require_installable=False):
    identity = GENERATION_PATTERN.fullmatch(generation)
    if identity is None:
        raise ValidationError("invalid generation id")
    if not isinstance(document, dict) or document.get("schema_version") != 1 or document.get("kind") != "internal-promoted-fleet-generation":
        raise ValidationError("unsupported generation manifest")
    if document.get("generation") != generation:
        raise ValidationError("generation manifest identity mismatch")
    sources = require_sources(document.get("sources"))
    if sources["flotilla"][:12] != identity.group(2) or sources["cleat"][:12] != identity.group(3):
        raise ValidationError("generation identity does not match source pins")
    version = document.get("peer_protocol_version")
    if not isinstance(version, int) or isinstance(version, bool) or version < 1:
        raise ValidationError("invalid peer_protocol_version")
    platforms = document.get("platforms")
    if not isinstance(platforms, dict) or not set(platforms).issubset(PLATFORMS):
        raise ValidationError("invalid platform set")
    if platform is None:
        return sources, version
    entry = platforms.get(platform)
    if not isinstance(entry, dict):
        raise ValidationError(f"generation has no {platform} artifact")
    if require_installable and entry.get("state") != "installable-internal":
        raise ValidationError(f"generation artifact for {platform} is not installable")
    expected_artifact = "fleet-signed-darwin-aarch64.tar.gz" if platform == "darwin-aarch64" else "fleet-candidate-linux-x86_64-gnu2.36.tar.gz"
    if entry.get("artifact") != expected_artifact:
        raise ValidationError("invalid artifact name")
    require_digest(entry.get("sha256"), "artifact digest")
    require_size(entry.get("size_bytes"), "artifact size")
    if platform == "linux-x86_64-gnu2.36" and entry.get("signed") is not False:
        raise ValidationError("Linux artifact has an unexpected signing state")
    if platform == "darwin-aarch64" and require_installable:
        if entry.get("signed") is not True:
            raise ValidationError("Darwin artifact is not centrally signed")
        source_generation = document.get("source_generation", "")
        source_identity = GENERATION_PATTERN.fullmatch(source_generation)
        if (source_identity is None or sources["flotilla"][:12] != source_identity.group(2)
                or sources["cleat"][:12] != source_identity.group(3)):
            raise ValidationError("Darwin source generation does not match source pins")
        if entry.get("source_artifact") != "fleet-candidate-darwin-aarch64.tar.gz":
            raise ValidationError("invalid Darwin source artifact")
        require_digest(entry.get("source_artifact_sha256"), "Darwin source artifact digest")
        central = document.get("central_signing")
        expected = {
            "derivative_package": "lab-signing/flotilla-fleet-darwin-signed",
            "derivative_version": source_generation,
            "attestation": "darwin-signing-attestation.json",
            "cms": "darwin-signing-attestation.cms",
            "certificate": "darwin-signing-certificate.pem",
        }
        if not isinstance(central, dict) or any(central.get(key) != value for key, value in expected.items()):
            raise ValidationError("invalid central-signing linkage")
        for field in ("attestation_sha256", "cms_sha256", "certificate_sha256"):
            require_digest(central.get(field), field)
        signing = central.get("signing")
        if not isinstance(signing, dict) or signing.get("team_id") != trusted_team:
            raise ValidationError(f"Darwin generation is not signed by trusted Apple team {trusted_team}")
        if (not isinstance(signing.get("identity"), str) or not signing["identity"]
                or signing.get("options") != ["runtime", "timestamp=none"]):
            raise ValidationError("invalid Darwin signing identity")
        require_digest(signing.get("certificate_sha256"), "signing certificate digest")
        require_digest(signing.get("entitlements_sha256"), "signing entitlements digest")
        if central["certificate_sha256"] != signing["certificate_sha256"] or entry.get("signing") != signing:
            raise ValidationError("Darwin signing metadata is invalid or inconsistent")
    return sources, version, entry


def validate_release(root, outer, platform):
    root = Path(root)
    manifest_path = root / "manifest.json"
    manifest = json.loads(manifest_path.read_text())
    sources, version, entry = validate_generation(outer, outer.get("generation", ""), platform, require_installable=True)
    kind = "signed-fleet-derivative" if platform == "darwin-aarch64" else "unsigned-fleet-candidate"
    if manifest.get("schema_version") != 1 or manifest.get("kind") != kind or manifest.get("platform") != platform:
        raise ValidationError("release manifest platform or schema mismatch")
    if manifest.get("sources") != sources or manifest.get("peer_protocol_version") != version:
        raise ValidationError("release and generation metadata differ")
    if platform == "darwin-aarch64":
        if (manifest.get("signed") is not True
                or manifest.get("source_generation") != outer.get("source_generation")
                or manifest.get("source_artifact_sha256") != entry.get("source_artifact_sha256")
                or manifest.get("signing") != outer.get("central_signing", {}).get("signing")):
            raise ValidationError("signed Darwin derivative does not match its generation")
    elif manifest.get("signed") is not False:
        raise ValidationError("Linux candidate has an unexpected signing state")
    entries = manifest.get("files")
    if not isinstance(entries, list) or not entries:
        raise ValidationError("release manifest has no files")
    expected = set()
    for item in entries:
        rel = item.get("path") if isinstance(item, dict) else None
        pure = PurePosixPath(rel) if isinstance(rel, str) else None
        if pure is None or pure.is_absolute() or ".." in pure.parts or not pure.parts or rel in expected or not allowed_payload(rel, platform):
            raise ValidationError("release manifest contains an unsafe, duplicate, or unexpected path")
        require_digest(item.get("sha256"), f"digest for {rel}")
        require_size(item.get("size_bytes"), f"size for {rel}", allow_zero=True)
        path = root / rel
        if not path.is_file() or path.stat().st_size != item["size_bytes"] or hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]:
            raise ValidationError(f"release file mismatch: {rel}")
        expected.add(rel)
    actual = {str(path.relative_to(root)) for path in root.rglob("*") if path.is_file() and path not in {manifest_path, root / ".generation.json"}}
    if actual != expected or not REQUIRED_PAYLOAD.issubset(expected):
        raise ValidationError("release files do not match the manifest or required payload")
    validate_skill_bundle(json.loads((root / "share/flotilla/skills/.flotilla-sources.json").read_text()), sources)
    # Present only from the generation that introduced it onward; a rollback
    # target predating it is still a valid release. See REQUIRED_PAYLOAD.
    codex_home = root.joinpath(*CODEX_HOME_PREFIX)
    if codex_home.exists():
        validate_codex_home_template(codex_home)
    for rel in ("bin/flotilla", "bin/flotillad", "bin/cleat"):
        if not os.access(root / rel, os.X_OK):
            raise ValidationError(f"release binary is not executable: {rel}")
    return manifest, entry


def validate_fixture(path):
    fixture = json.loads(Path(path).read_text())
    document = fixture["manifest"]
    sources, _ = validate_generation(document, fixture["generation"])
    payload = fixture.get("payload", sorted(REQUIRED_PAYLOAD))
    if not REQUIRED_PAYLOAD.issubset(payload) or any(not allowed_payload(item) for item in payload):
        raise ValidationError("unexpected or missing payload")
    validate_skill_bundle(fixture["skill_bundle"], sources)


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    generation = sub.add_parser("generation")
    generation.add_argument("manifest")
    generation.add_argument("generation")
    generation.add_argument("platform", nargs="?")
    generation.add_argument("--installable", action="store_true")
    release = sub.add_parser("release")
    release.add_argument("root")
    release.add_argument("manifest")
    release.add_argument("platform")
    fixture = sub.add_parser("fixture")
    fixture.add_argument("path")
    skill_sources = sub.add_parser("skill-sources")
    skill_sources.add_argument("manifest")
    codex_home = sub.add_parser("codex-home")
    codex_home.add_argument("root")
    args = parser.parse_args()
    try:
        if args.command == "fixture":
            validate_fixture(args.path)
        elif args.command == "skill-sources":
            validate_skill_source_paths(json.loads(Path(args.manifest).read_text()))
        elif args.command == "codex-home":
            validate_codex_home_template(args.root)
        else:
            outer = json.loads(Path(args.manifest).read_text())
        if args.command == "generation":
            validate_generation(outer, args.generation, args.platform, require_installable=args.installable)
        elif args.command == "release":
            validate_release(args.root, outer, args.platform)
    except (OSError, json.JSONDecodeError, ValidationError) as error:
        parser.exit(1, f"generation validation: {error}\n")


if __name__ == "__main__":
    main()
