#!/usr/bin/env python3
"""Canonical validation for fleet candidates and promoted generations."""

import argparse
import hashlib
import json
import os
import plistlib
import re
import secrets
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from xml.parsers.expat import ExpatError

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
# Produced together by build-candidate.sh, optional when decoding a previous
# generation so health rollback remains valid (ADR 0047).
CANARY_PAYLOAD = {"fleet-canary.py", "fleet-canary-agent.sh", "crew-image-baseline.yaml"}
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")
DIGEST_PATTERN = re.compile(r"^[0-9a-f]{64}$")
GENERATION_PATTERN = re.compile(r"^(\d{8}T\d{6}Z-r\d+-f([0-9a-f]{12})-c([0-9a-f]{12}))$")
# Component metadata is embedded in the signed v2 pin list, so compatibility
# claims are authenticated together with the identity and archive digest.
COMPONENT_PATTERN = re.compile(r"^[a-z][a-z0-9-]*$")
FACT_PATTERN = re.compile(r"^([a-z][a-z0-9-]*(?::[a-z0-9][a-z0-9_.-]*)+)(?:=(0|[1-9][0-9]*))?$")
REQUIREMENT_PATTERN = re.compile(r"^([a-z][a-z0-9-]*(?::[a-z0-9][a-z0-9_.-]*)+)(?:(=|>=)(0|[1-9][0-9]*))?$")
INDEPENDENT_PLATFORM = "platform-independent"



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
    if path in REQUIRED_PAYLOAD or path in CANARY_PAYLOAD:
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


def validate_skill_source_paths(document, catalog_output=None):
    catalog = []
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
                    config = ("-c", "credential.helper=")
                    # Catalog production must authenticate private sources with an
                    # explicit one-shot read credential, never an ambient helper.
                    if catalog_output is not None and source.get("credential"):
                        if not os.environ.get("GH_TOKEN") and not os.environ.get("GITHUB_TOKEN_FILE"):
                            raise ValidationError(f"catalog source {name} needs injected read credentials")
                        helper = '!f() { [ "$1" = get ] || exit 0; token="$GH_TOKEN"; if [ -n "$GITHUB_TOKEN_FILE" ]; then IFS= read -r token <"$GITHUB_TOKEN_FILE" || :; fi; [ -n "$token" ] || exit 1; printf "username=x-access-token\\npassword=%s\\n" "$token"; }; f'
                        config += ("-c", f"credential.helper={helper}")
                    command = command[:1] + config + command[1:]
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
                if catalog_output is not None:
                    raise ValidationError(f"skill catalog requires credentialed verification of {name}")
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
                if catalog_output is None:
                    continue
                for skill_file in sorted(path.rglob("SKILL.md")):
                    if not skill_file.resolve().is_relative_to(checkout.resolve()):
                        raise ValidationError(f"skill escapes source checkout: {skill_file}")
                    lines = skill_file.read_text().splitlines()
                    if not lines or lines[0] != "---" or "---" not in lines[1:]:
                        raise ValidationError(f"skill lacks frontmatter: {skill_file}")
                    fields = lines[1:lines[1:].index("---") + 1]
                    names = [line.split(":", 1)[1].split("#", 1)[0].strip().strip("\"'") for line in fields if line.startswith("name:")]
                    if len(names) != 1 or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", names[0]):
                        raise ValidationError(f"invalid skill frontmatter name: {skill_file}")
                    from urllib.parse import urlparse
                    repo = urlparse(source["repository"]).path.strip("/").removesuffix(".git")
                    if "://" not in source["repository"] and ":" in source["repository"]:
                        repo = source["repository"].split(":", 1)[1].strip("/").removesuffix(".git")
                    if len(repo.split("/")) != 2:
                        raise ValidationError(f"skill repository must have owner/repo identity: {source['repository']}")
                    if any(character in str(skill_file.parent.relative_to(checkout)) for character in "\r\n\t"):
                        raise ValidationError(f"unsafe skill catalog path: {skill_file}")
                    entry = {"source": name, "repository": repo, "revision": revision, "name": names[0],
                             "path": str(skill_file.parent.relative_to(checkout))}
                    if entry in catalog:
                        continue
                    catalog.append(entry)
    if catalog_output is not None:
        Path(catalog_output).write_text(json.dumps(catalog, indent=2, sort_keys=True) + "\n")



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


def validate_component(document):
    """Read a component manifest; builders must measure provides from payload.

    Identity is (component, source_sha, platform, recipe_sha256). The recipe
    hashes canonical build inputs including toolchain pins and build features;
    archive_sha256 identifies the installed bytes, not a build cache.
    """
    fields = {"schema_version", "kind", "component", "source_sha", "platform",
              "recipe_sha256", "archive_sha256", "provides", "requires"}
    if (not isinstance(document, dict) or not fields.issubset(document)
            or set(document) - fields - {"version"}
            or type(document.get("schema_version")) is not int or document["schema_version"] != 1
            or document.get("kind") != "fleet-component"):
        raise ValidationError("invalid component manifest")
    name = document["component"]
    if not isinstance(name, str) or not COMPONENT_PATTERN.fullmatch(name):
        raise ValidationError("invalid component name")
    sha = document["source_sha"]
    if not isinstance(sha, str) or not SHA_PATTERN.fullmatch(sha):
        raise ValidationError(f"invalid {name} source pin")
    if document["platform"] not in (*PLATFORMS, INDEPENDENT_PLATFORM):
        raise ValidationError(f"invalid {name} platform")
    require_digest(document["recipe_sha256"], f"{name} recipe hash")
    require_digest(document["archive_sha256"], f"{name} archive digest")
    if "version" in document and (not isinstance(document["version"], str) or not document["version"].strip()):
        raise ValidationError(f"invalid {name} version label")
    for field, pattern in (("provides", FACT_PATTERN), ("requires", REQUIREMENT_PATTERN)):
        values = document[field]
        if (not isinstance(values, list) or any(not isinstance(value, str) or not pattern.fullmatch(value) for value in values)
                or len(values) != len(set(values))):
            raise ValidationError(f"invalid or duplicate {name} {field}")
    keys = set()
    for fact in document["provides"]:
        key = FACT_PATTERN.fullmatch(fact).group(1)
        if not key.startswith(name + ":") or key in keys:
            raise ValidationError(f"foreign or conflicting {name} provides: {fact}")
        keys.add(key)
    return document


def validate_composition(components, platform):
    """Refuse missing facts in one platform's validated effective pin set.

    Callers must run validate_component on every pin and refuse duplicate
    component names. Ownership checks then make fact-key collisions impossible: no other
    component can provide facts in a pin's namespace.
    """
    facts = {}
    for component in components:
        for fact in component["provides"]:
            match = FACT_PATTERN.fullmatch(fact)
            facts[match.group(1)] = match.group(2)
    for component in components:
        for requirement in component["requires"]:
            key, operator, number = REQUIREMENT_PATTERN.fullmatch(requirement).groups()
            met = key in facts
            if operator is not None:
                measured = facts.get(key)
                met = measured is not None and (int(measured) >= int(number) if operator == ">=" else int(measured) == int(number))
            if not met:
                raise ValidationError(f"{platform}: {component['component']} requires {requirement}, not provided by pinned components")


def validate_generation_v2(document, generation, platform=None, require_installable=False):
    fields = {"schema_version", "kind", "generation", "peer_protocol_version", "signing", "platforms", "platform_independent"}
    if set(document) != fields:
        raise ValidationError("invalid v2 generation fields")
    if document["signing"] != {"scheme": "cms-detached", "signature": "generation.json.cms"}:
        raise ValidationError("invalid v2 generation signing contract")
    version = document["peer_protocol_version"]
    require_size(version, "peer_protocol_version")
    platforms = document["platforms"]
    if not isinstance(platforms, dict) or not platforms or not set(platforms).issubset(PLATFORMS):
        raise ValidationError("invalid v2 platform set")
    shared = document["platform_independent"]
    if not isinstance(shared, list):
        raise ValidationError("invalid platform-independent pin list")

    def pins(values, expected_platform):
        if not isinstance(values, list):
            raise ValidationError(f"invalid {expected_platform} pin list")
        names = set()
        for value in values:
            component = validate_component(value)
            if component["platform"] != expected_platform:
                raise ValidationError(f"component {component['component']} platform does not match {expected_platform}")
            if component["component"] in names:
                raise ValidationError(f"duplicate {expected_platform} component: {component['component']}")
            names.add(component["component"])
        return names

    shared_names = pins(shared, INDEPENDENT_PLATFORM)
    sources = {}
    for target, entry in platforms.items():
        if (not isinstance(entry, dict) or set(entry) != {"state", "components"}
                or not isinstance(entry["state"], str) or entry["state"] not in {"candidate", "installable-internal"}):
            raise ValidationError(f"invalid {target} composition")
        names = pins(entry["components"], target)
        if names & shared_names:
            raise ValidationError(f"platform-independent component repeated in {target}")
        effective = entry["components"] + shared
        if not {"flotilla", "cleat"}.issubset(names) or "skills" not in shared_names:
            raise ValidationError(f"{target} missing required fleet components")
        for component in effective:
            name, sha = component["component"], component["source_sha"]
            if name in sources and sources[name] != sha:
                raise ValidationError(f"component {name} source pins differ across platforms")
            sources[name] = sha
        validate_composition(effective, target)
        flotilla = next(component for component in effective if component["component"] == "flotilla")
        if f"flotilla:protocol={version}" not in flotilla["provides"]:
            raise ValidationError(f"{target} peer protocol differs from measured flotilla protocol")
        if require_installable and (platform is None or platform == target) and entry["state"] != "installable-internal":
            raise ValidationError(f"generation artifact for {target} is not installable")
    identity = GENERATION_PATTERN.fullmatch(generation)
    if sources["flotilla"][:12] != identity.group(2) or sources["cleat"][:12] != identity.group(3):
        raise ValidationError("generation identity does not match component pins")
    if platform is None:
        return sources, version
    if platform not in platforms:
        raise ValidationError(f"generation has no {platform} composition")
    return sources, version, platforms[platform]


def verify_generation_signature(manifest, signature, trusted_certificate):
    """Verify exact manifest bytes with a caller-pinned CMS signer certificate.

    Structural validation does not establish authenticity. Never trust a
    certificate shipped by the candidate. Ignore embedded signer certificates
    and use only the provisioning-owned exact leaf-certificate pin supplied by
    the caller, not a CA certificate or a chain trust store. -noverify disables
    chain, validity-period and purpose checks; revocation is not checked either.
    Expired certificates remain usable for rollback while explicitly provisioned.
    Withdrawing a signer requires removing its pin from provisioning.
    """
    try:
        result = subprocess.run(
            ["openssl", "cms", "-verify", "-binary", "-inform", "DER", "-in", str(signature),
             "-content", str(manifest), "-nointern", "-certfile", str(trusted_certificate),
             "-noverify", "-out", os.devnull], capture_output=True, text=True)
    except FileNotFoundError as error:
        raise ValidationError("OpenSSL CMS verifier is unavailable") from error
    if result.returncode != 0:
        raise ValidationError("generation signature does not verify against the trusted certificate")


def validate_generation(document, generation, platform=None, trusted_team="973L4GV58R", require_installable=False):
    identity = GENERATION_PATTERN.fullmatch(generation) if isinstance(generation, str) else None
    if identity is None:
        raise ValidationError("invalid generation id")
    if (not isinstance(document, dict) or type(document.get("schema_version")) is not int
            or document.get("schema_version") not in {1, 2}
            or document.get("kind") != "internal-promoted-fleet-generation"):
        raise ValidationError("unsupported generation manifest")
    if document.get("generation") != generation:
        raise ValidationError("generation manifest identity mismatch")
    if document["schema_version"] == 2:
        return validate_generation_v2(document, generation, platform, require_installable)
    # Remove v1 decoding one fleet roll after the dual-published transition.
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
    if outer.get("schema_version") == 2:
        raise ValidationError("v2 component installation requires the component installer; cannot verify as a v1 bundle")
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
    if document["schema_version"] == 2:
        # V2 fixtures exercise the pin list, not a monolithic bundle payload.
        # Component archive verification belongs to the component installer.
        return
    payload = fixture.get("payload", sorted(REQUIRED_PAYLOAD))
    if not REQUIRED_PAYLOAD.issubset(payload) or any(not allowed_payload(item) for item in payload):
        raise ValidationError("unexpected or missing payload")
    validate_skill_bundle(fixture["skill_bundle"], sources)


def collect_package_page(response, package_name, output_path):
    packages = json.loads(Path(response).read_text())
    if not isinstance(packages, list):
        raise ValidationError("package listing is not an array")
    with open(output_path, "a") as output:
        for package in packages:
            if package.get("type") == "generic" and package.get("name") == package_name:
                print(json.dumps([package.get("created_at", ""), package.get("version", "")]), file=output)
    print(len(packages))


def package_versions(path):
    rows = [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
    rows = [row for row in rows if row[0] and row[1]]
    for row in sorted(rows, reverse=True):
        print(row[1])


def manifest_value(manifest, expression):
    value = json.loads(Path(manifest).read_text())
    for part in expression.split("."):
        value = value[part]
    print(value)


def platform_value(manifest, platform, field):
    print(json.loads(Path(manifest).read_text())["platforms"][platform][field])


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    print(digest.hexdigest())


def file_size(path):
    print(os.path.getsize(path))


def extract_archive(archive, destination, platform):
    archive = Path(archive)
    destination = Path(destination)
    expected_root = "fleet-signed-darwin-aarch64" if platform == "darwin-aarch64" else f"fleet-candidate-{platform}"
    with tarfile.open(archive, "r:gz") as bundle:
        members = bundle.getmembers()
        names = set()
        roots = set()
        for member in members:
            path = PurePosixPath(member.name)
            normalized = str(path)
            if path.is_absolute() or ".." in path.parts or not path.parts or normalized in names:
                raise ValidationError(f"unsafe or duplicate archive path: {member.name}")
            if not (member.isfile() or member.isdir()):
                raise ValidationError(f"unsupported archive entry: {member.name}")
            names.add(normalized)
            roots.add(path.parts[0])
        if roots != {expected_root}:
            raise ValidationError(f"archive has an unexpected bundle directory: expected {expected_root}, got {sorted(roots)}")
        for member in members:
            target = destination.joinpath(*PurePosixPath(member.name).parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                target.chmod(member.mode & 0o777)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            source = bundle.extractfile(member)
            if source is None:
                raise ValidationError(f"archive entry cannot be read: {member.name}")
            with source, target.open("wb") as output:
                shutil.copyfileobj(source, output)
            target.chmod(member.mode & 0o777)
    shutil.move(str(destination / expected_root), str(destination / "release"))


def validate_entitlements(path, relative):
    content = Path(path).read_bytes()
    try:
        entitlements = plistlib.loads(content) if content.strip() else {}
    except (plistlib.InvalidFileException, ExpatError) as error:
        raise ValidationError(f"signed Darwin payload has unreadable entitlements: {relative}: {error}") from error
    if entitlements != {}:
        raise ValidationError(f"signed Darwin payload has unexpected entitlements: {relative}")


def protocol_version(path):
    try:
        print(json.loads(Path(path).read_text()).get("peer_protocol_version", ""))
    except (OSError, ValueError):
        pass


# Bootstrap/recovery glue intentionally stays outside the generation CLI:
# first install has no selected CLI, rollback targets can predate new commands,
# and recovery must not require the candidate's health gate to have passed.
# Keep these commands in the existing synced validator file so neither local
# bootstrap nor the SSH canary needs another payload/deployment boundary.
def release_install_lock(path, owner):
    """Release only the bootstrap lock owned by this invocation (or stale owner)."""
    try:
        if os.path.islink(path) and os.readlink(path) == owner:
            os.unlink(path)
    except FileNotFoundError:
        pass


def bootstrap_nonce():
    print(secrets.token_hex(8))


def file_mode(path):
    print(format(stat.S_IMODE(os.stat(path).st_mode), "o"))


def real_path(path):
    print(os.path.realpath(path))


def replace_link(source, destination):
    """Atomically replace the selection link without following its directory target."""
    os.replace(source, destination)


def systemd_path(path, home):
    if "\n" in path or "\r" in path:
        raise ValidationError("systemd path contains a line break")
    if path == home:
        print("%h")
    elif path.startswith(home + os.sep):
        relative = path[len(home) + 1 :].replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")
        print(f"%h/{relative}")
    else:
        print(path.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%"))


def write_launchd_agent(destination, label, daemon, path, skills, codex_home, config_dir, state_dir, socket, stderr_path, stdout_path):
    content = {
        "Label": label,
        "ProgramArguments": [
            daemon,
            "--timeout",
            "0",
            "--config-dir",
            config_dir,
            "--state-dir",
            state_dir,
            "--socket",
            socket,
        ],
        "EnvironmentVariables": {"PATH": path, "FLOTILLA_SKILLS_DIR": skills, "FLOTILLA_CODEX_HOME_TEMPLATE": codex_home},
        "StandardErrorPath": stderr_path,
        "StandardOutPath": stdout_path,
        "RunAtLoad": True,
        "KeepAlive": True,
    }
    with open(destination, "wb") as output:
        plistlib.dump(content, output, sort_keys=False)


def refresh_darwin_payload(source, destination):
    source, destination = Path(source), Path(destination)
    files = [Path("bin") / name for name in ("flotilla", "flotillad", "cleat")]
    files += [path.relative_to(source) for path in (source / "lib").rglob("*") if path.is_file()]
    expected = set(files)
    for stale in (destination / "lib").rglob("*"):
        if (stale.is_file() or stale.is_symlink()) and stale.relative_to(destination) not in expected:
            stale.unlink()
    for relative in files:
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".fleet-install-", dir=target.parent)
        os.close(fd)
        try:
            shutil.copy2(source / relative, temporary)
            os.replace(temporary, target)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def disarm_confirmation(path, token):
    try:
        with open(path) as source:
            armed = any(line.rstrip("\n") == f"token={token}" for line in source)
        if armed:
            os.unlink(path)
    except FileNotFoundError:
        pass


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    generation = sub.add_parser("generation")
    generation.add_argument("manifest")
    generation.add_argument("generation")
    generation.add_argument("platform", nargs="?")
    generation.add_argument("--installable", action="store_true")
    component = sub.add_parser("component")
    component.add_argument("manifest")
    signature = sub.add_parser("verify-signature")
    signature.add_argument("manifest")
    signature.add_argument("signature")
    signature.add_argument("trusted_certificate")
    release = sub.add_parser("release")
    release.add_argument("root")
    release.add_argument("manifest")
    release.add_argument("platform")
    fixture = sub.add_parser("fixture")
    fixture.add_argument("path")
    skill_sources = sub.add_parser("skill-sources")
    skill_sources.add_argument("manifest")
    skill_sources.add_argument("--catalog-output")
    codex_home = sub.add_parser("codex-home")
    codex_home.add_argument("root")
    helper = sub.add_parser("package-page")
    helper.add_argument("response")
    helper.add_argument("package_name")
    helper.add_argument("output_path")
    helper = sub.add_parser("package-versions")
    helper.add_argument("path")
    helper = sub.add_parser("value")
    helper.add_argument("manifest")
    helper.add_argument("expression")
    helper = sub.add_parser("platform-value")
    helper.add_argument("manifest")
    helper.add_argument("platform")
    helper.add_argument("field")
    helper = sub.add_parser("sha256")
    helper.add_argument("path")
    helper = sub.add_parser("size")
    helper.add_argument("path")
    helper = sub.add_parser("extract")
    helper.add_argument("archive")
    helper.add_argument("destination")
    helper.add_argument("platform")
    helper = sub.add_parser("entitlements")
    helper.add_argument("path")
    helper.add_argument("relative")
    helper = sub.add_parser("protocol")
    helper.add_argument("path")
    helper = sub.add_parser("release-lock")
    helper.add_argument("path")
    helper.add_argument("owner")
    sub.add_parser("nonce")
    for command in ("mode", "realpath"):
        helper = sub.add_parser(command)
        helper.add_argument("path")
    helper = sub.add_parser("replace-link")
    helper.add_argument("source")
    helper.add_argument("destination")
    helper = sub.add_parser("systemd-path")
    helper.add_argument("path")
    helper.add_argument("home")
    helper = sub.add_parser("launchd-agent")
    helper.add_argument("destination")
    helper.add_argument("label")
    helper.add_argument("daemon")
    helper.add_argument("path")
    helper.add_argument("skills")
    helper.add_argument("codex_home")
    helper.add_argument("config_dir")
    helper.add_argument("state_dir")
    helper.add_argument("socket")
    helper.add_argument("stderr_path")
    helper.add_argument("stdout_path")
    helper = sub.add_parser("darwin-payload")
    helper.add_argument("source")
    helper.add_argument("destination")
    helper = sub.add_parser("disarm-confirmation")
    helper.add_argument("path")
    helper.add_argument("token")
    args = parser.parse_args()
    try:
        if args.command == "release-lock":
            release_install_lock(args.path, args.owner)
        elif args.command == "nonce":
            bootstrap_nonce()
        elif args.command == "mode":
            file_mode(args.path)
        elif args.command == "realpath":
            real_path(args.path)
        elif args.command == "replace-link":
            replace_link(args.source, args.destination)
        elif args.command == "systemd-path":
            systemd_path(args.path, args.home)
        elif args.command == "launchd-agent":
            write_launchd_agent(args.destination, args.label, args.daemon, args.path, args.skills, args.codex_home, args.config_dir, args.state_dir, args.socket, args.stderr_path, args.stdout_path)
        elif args.command == "darwin-payload":
            refresh_darwin_payload(args.source, args.destination)
        elif args.command == "disarm-confirmation":
            disarm_confirmation(args.path, args.token)
        elif args.command == "fixture":
            validate_fixture(args.path)
        elif args.command == "skill-sources":
            validate_skill_source_paths(json.loads(Path(args.manifest).read_text()), args.catalog_output)
        elif args.command == "codex-home":
            validate_codex_home_template(args.root)
        elif args.command == "generation":
            validate_generation(json.loads(Path(args.manifest).read_text()), args.generation, args.platform, require_installable=args.installable)
        elif args.command == "component":
            validate_component(json.loads(Path(args.manifest).read_text()))
        elif args.command == "verify-signature":
            verify_generation_signature(args.manifest, args.signature, args.trusted_certificate)
        elif args.command == "release":
            validate_release(args.root, json.loads(Path(args.manifest).read_text()), args.platform)
        elif args.command == "package-page":
            collect_package_page(args.response, args.package_name, args.output_path)
        elif args.command == "package-versions":
            package_versions(args.path)
        elif args.command == "value":
            manifest_value(args.manifest, args.expression)
        elif args.command == "platform-value":
            platform_value(args.manifest, args.platform, args.field)
        elif args.command == "sha256":
            sha256_file(args.path)
        elif args.command == "size":
            file_size(args.path)
        elif args.command == "extract":
            extract_archive(args.archive, args.destination, args.platform)
        elif args.command == "entitlements":
            validate_entitlements(args.path, args.relative)
        elif args.command == "protocol":
            protocol_version(args.path)
    except (OSError, json.JSONDecodeError, tarfile.TarError, KeyError, ValidationError) as error:
        parser.exit(1, f"generation validation: {error}\n")


if __name__ == "__main__":
    main()
