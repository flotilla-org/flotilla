#!/usr/bin/env python3
"""Canonical validation for fleet candidates and promoted generations."""

import argparse
import hashlib
import json
import os
import re
from pathlib import Path, PurePosixPath

PLATFORMS = ("linux-x86_64-gnu2.36", "darwin-aarch64")
SOURCE_NAMES = ("flotilla", "cleat", "mattpocock-skills", "rjw-skills")
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
    return (library
            or (len(pure.parts) >= 4 and pure.parts[:3] == ("share", "flotilla", "skills")))


def validate_skill_bundle(document, sources):
    entries = document.get("sources") if isinstance(document, dict) else None
    if (not isinstance(document, dict) or set(document) != {"schema_version", "sources"}
            or document.get("schema_version") != 3 or not isinstance(entries, list) or not entries):
        raise ValidationError("invalid v3 skill bundle")
    names = set()
    for source in entries:
        if not isinstance(source, dict) or set(source) != {"name", "repository", "revision"}:
            raise ValidationError("invalid skill source")
        name = source.get("name")
        repository = source.get("repository")
        if (not isinstance(name, str) or not name or name in {".", ".."} or any(c in name for c in "/\\\r\n")
                or name in names or not isinstance(repository, str) or not repository
                or source.get("revision") != sources.get(name)):
            raise ValidationError("skill bundle source pins do not match the fleet generation")
        if name == "mattpocock-skills" and repository != "https://github.com/flotilla-org/mattpocock-skills.git":
            raise ValidationError("skill bundle points the credential-granted source at an unexpected repository")
        names.add(name)
    return entries


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
    args = parser.parse_args()
    try:
        if args.command == "fixture":
            validate_fixture(args.path)
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
