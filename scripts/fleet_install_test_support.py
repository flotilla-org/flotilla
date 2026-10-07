"""Data fixtures and output assertions for the fleet bootstrap shell contract.

These helpers construct package metadata; production policy lives in
ci/fleet-candidates/generation_validation.py and its ordinary unit tests.
"""
import hashlib
import json
import os
import plistlib
import re
import sys
from pathlib import Path


def linux_release(args, environ):
    bundle = Path(environ["TEST_BUNDLE"])
    identity = re.fullmatch(r".+-f([0-9a-f]{12})-c([0-9a-f]{12})", bundle.parent.name.removeprefix("bundle-"))
    sources = {
        "flotilla": identity.group(1) + "1" * 28,
        "cleat": identity.group(2) + "a" * 28,
        "mattpocock-skills": "2" * 40,
        "rjw-skills": "b" * 40,
    }
    files = []
    for path in sorted(bundle.rglob("*")):
        if path.is_file():
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            if environ["TEST_CORRUPT_INNER"] == "yes" and path.name == "cleat":
                digest = "0" * 64
            files.append({"path": str(path.relative_to(bundle)), "sha256": digest, "size_bytes": path.stat().st_size})
    (bundle / "manifest.json").write_text(json.dumps({
        "schema_version": 1,
        "kind": "unsigned-fleet-candidate",
        "platform": environ["TEST_PLATFORM"],
        "sources": sources,
        "peer_protocol_version": int(environ["TEST_PROTOCOL"]),
        "signed": False,
        "files": files,
    }, sort_keys=True) + "\n")


def linux_generation(args, environ):
    identity = re.fullmatch(r".+-f([0-9a-f]{12})-c([0-9a-f]{12})", environ["TEST_GENERATION"])
    sources = {
        "flotilla": identity.group(1) + "1" * 28,
        "cleat": identity.group(2) + "a" * 28,
        "mattpocock-skills": "2" * 40,
        "rjw-skills": "b" * 40,
    }
    manifest = {
        "schema_version": 1,
        "kind": "internal-promoted-fleet-generation",
        "generation": environ["TEST_GENERATION"],
        "sources": sources,
        "peer_protocol_version": int(environ["TEST_PROTOCOL"]),
        "platforms": {
            environ["TEST_PLATFORM"]: {
                "artifact": environ["TEST_ARTIFACT"],
                "sha256": environ["TEST_DIGEST"],
                "size_bytes": int(environ["TEST_SIZE"]),
                "signed": False,
                "state": "installable-internal",
            }
        },
    }
    (Path(environ["TEST_DIRECTORY"]) / "generation.json").write_text(json.dumps(manifest, sort_keys=True) + "\n")


def darwin_release(args, environ):
    bundle = Path(environ["TEST_BUNDLE"])
    sources = {
        "flotilla": environ["TEST_GENERATION"].split("-f", 1)[1].split("-c", 1)[0] + "1" * 28,
        "cleat": environ["TEST_GENERATION"].rsplit("-c", 1)[1] + "a" * 28,
        "mattpocock-skills": "2" * 40,
        "rjw-skills": "b" * 40,
    }
    files = [
        {
            "path": str(path.relative_to(bundle)),
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "size_bytes": path.stat().st_size,
        }
        for path in sorted(bundle.rglob("*"))
        if path.is_file()
    ]
    signing = {
        "identity": "Apple Development: Robert Wittams (DYYMCPD885)",
        "team_id": "973L4GV58R",
        "certificate_sha256": "d" * 64,
        "entitlements_sha256": "e" * 64,
        "options": ["runtime", "timestamp=none"],
    }
    (bundle / "manifest.json").write_text(json.dumps({
        "schema_version": 1,
        "kind": "signed-fleet-derivative",
        "platform": "darwin-aarch64",
        "sources": sources,
        "build_profile": "release",
        "peer_protocol_version": int(environ["TEST_PROTOCOL"]),
        "signed": True,
        "source_generation": environ["TEST_SOURCE_GENERATION"],
        "source_artifact_sha256": "c" * 64,
        "signing": signing,
        "files": files,
    }, sort_keys=True) + "\n")


def darwin_generation(args, environ):
    path = Path(environ["TEST_DIRECTORY"]) / "generation.json"
    manifest = json.loads(path.read_text())
    signing = {
        "identity": "Apple Development: Robert Wittams (DYYMCPD885)",
        "team_id": "973L4GV58R",
        "certificate_sha256": "d" * 64,
        "entitlements_sha256": "e" * 64,
        "options": ["runtime", "timestamp=none"],
    }
    manifest["source_generation"] = environ["TEST_SOURCE_GENERATION"]
    manifest["central_signing"] = {
        "derivative_package": "lab-signing/flotilla-fleet-darwin-signed",
        "derivative_version": environ["TEST_SOURCE_GENERATION"],
        "attestation": "darwin-signing-attestation.json",
        "attestation_sha256": "f" * 64,
        "cms": "darwin-signing-attestation.cms",
        "cms_sha256": "1" * 64,
        "certificate": "darwin-signing-certificate.pem",
        "certificate_sha256": signing["certificate_sha256"],
        "signing": signing,
    }
    manifest["platforms"]["darwin-aarch64"] = {
        "artifact": environ["TEST_ARTIFACT"],
        "sha256": environ["TEST_DIGEST"],
        "size_bytes": int(environ["TEST_SIZE"]),
        "signed": True,
        "state": "installable-internal",
        "source_artifact": "fleet-candidate-darwin-aarch64.tar.gz",
        "source_artifact_sha256": "c" * 64,
        "signing": signing,
    }
    path.write_text(json.dumps(manifest, sort_keys=True) + "\n")


def assert_launchd(args, environ):
    with open(args[0], "rb") as source:
        agent = plistlib.load(source)
    home = args[1]
    assert agent["Label"] == "work.flotilla.flotillad"
    assert agent["ProgramArguments"] == [
        f"{home}/.local/opt/flotilla-fleet/tcc/bin/flotillad",
        "--timeout",
        "0",
        "--config-dir",
        f"{home}/.config/flotilla",
        "--state-dir",
        f"{home}/.local/state/flotilla",
        "--socket",
        f"{home}/.config/flotilla/run/flotilla.sock",
    ]
    assert agent["EnvironmentVariables"]["PATH"].split(":")[0] == f"{home}/.local/bin"
    assert agent["EnvironmentVariables"]["FLOTILLA_SKILLS_DIR"] == f"{home}/.local/opt/flotilla-fleet/current/share/flotilla/skills"
    assert agent["EnvironmentVariables"]["FLOTILLA_CODEX_HOME_TEMPLATE"] == f"{home}/.local/opt/flotilla-fleet/current/share/flotilla/codex-home"
    assert "/usr/sbin" in agent["EnvironmentVariables"]["PATH"].split(":")
    assert "/sbin" in agent["EnvironmentVariables"]["PATH"].split(":")
    assert agent["StandardErrorPath"] == f"{home}/Library/Logs/flotilla/flotillad.stderr.log"
    assert agent["StandardOutPath"] == f"{home}/Library/Logs/flotilla/flotillad.stdout.log"
    assert agent["RunAtLoad"] is True
    assert agent["KeepAlive"] is True


def corrupt_outer_source(args, environ):
    path = Path(args[0])
    manifest = json.loads(path.read_text())
    manifest["central_signing"]["signing"]["team_id"] = "ATTACKERTEAM"
    path.write_text(json.dumps(manifest) + "\n")


def corrupt_outer_digest(args, environ):
    path = Path(args[0])
    manifest = json.loads(path.read_text())
    manifest["platforms"]["linux-x86_64-gnu2.36"]["sha256"] = "0" * 64
    path.write_text(json.dumps(manifest) + "\n")


if __name__ == "__main__":
    handlers = {
        "linux-release": linux_release,
        "linux-generation": linux_generation,
        "darwin-release": darwin_release,
        "darwin-generation": darwin_generation,
        "assert-launchd": assert_launchd,
        "corrupt-outer-source": corrupt_outer_source,
        "corrupt-outer-digest": corrupt_outer_digest,
    }
    handlers[sys.argv[1]](sys.argv[2:], os.environ)
