"""Component and generation contracts from #2762 / the #1848 ruling."""
import copy
import itertools
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import generation_validation as validation


GENERATION = "20261006T120000Z-r545-faaaaaaaaaaaa-cbbbbbbbbbbbb"


def component(name, platform, provides=(), requires=()):
    return {"schema_version": 1, "kind": "fleet-component", "component": name,
            "source_sha": {"flotilla": "a", "cleat": "b", "skills": "c"}.get(name, "d") * 40,
            "platform": platform, "recipe_sha256": "d" * 64, "archive_sha256": "e" * 64,
            "provides": list(provides), "requires": list(requires)}


def generation():
    return {"schema_version": 2, "kind": "internal-promoted-fleet-generation", "generation": GENERATION,
            "peer_protocol_version": 20,
            "signing": {"scheme": "cms-detached", "signature": "generation.json.cms"},
            "platform_independent": [component("skills", validation.INDEPENDENT_PLATFORM, ["skills:tree:rjw-sdlc"])],
            "platforms": {platform: {"state": "installable-internal", "components": [
                component("flotilla", platform, ["flotilla:protocol=20"],
                          ["cleat:capability:env-clear", "cleat:protocol>=12", "skills:tree:rjw-sdlc"]),
                component("cleat", platform, ["cleat:capability:env-clear", "cleat:protocol=12"])]}
                for platform in validation.PLATFORMS}}


class ComponentValidationTests(unittest.TestCase):
    # Each platform composes only its own pins plus shared pins. Numeric
    # requirements cross the minimum boundary; exact requirements are distinct.
    # Deterministic exhaustive generation covers both platforms, presence/absence
    # of the capability, and every comparison for protocols 0/11/12/13/2**32-1.
    def test_requires_provides_boundaries(self):
        for platform, capability, protocol, operator in itertools.product(validation.PLATFORMS, (False, True), (0, 11, 12, 13, 2**32 - 1), ("=", ">=")):
            with self.subTest(platform=platform, capability=capability, protocol=protocol, operator=operator):
                document = generation()
                flotilla, cleat = document["platforms"][platform]["components"]
                flotilla["requires"][1] = f"cleat:protocol{operator}12"
                cleat["provides"] = [f"cleat:protocol={protocol}"] + (["cleat:capability:env-clear"] if capability else [])
                if capability and (protocol >= 12 if operator == ">=" else protocol == 12):
                    validation.validate_generation(document, GENERATION)
                else:
                    with self.assertRaisesRegex(validation.ValidationError, f"{platform}: flotilla requires cleat:"):
                        validation.validate_generation(document, GENERATION)

    # The signed pin list retains independent components once and exposes the
    # selected platform; ordering pins or facts cannot change compatibility.
    def test_valid_composition_and_order_invariance(self):
        document = generation()
        for platform in validation.PLATFORMS:
            sources, version, entry = validation.validate_generation(document, GENERATION, platform, require_installable=True)
            self.assertEqual(sources, {"flotilla": "a" * 40, "cleat": "b" * 40, "skills": "c" * 40})
            self.assertEqual(version, 20)
            self.assertEqual(entry, document["platforms"][platform])
            entry["components"].reverse()
            for pin in entry["components"]:
                pin["provides"].reverse()
                pin["requires"].reverse()
        validation.validate_generation(document, GENERATION)

    # A measured flag is not a numeric protocol, and another platform or a
    # foreign namespace cannot supply a missing fact. Shared requirements must
    # also be met on every platform, not by pooling platform-specific provides.
    def test_fact_isolation_and_shared_requirements(self):
        document = generation()
        platform = validation.PLATFORMS[1]
        document["platforms"][platform]["components"][1]["provides"] = ["cleat:protocol", "cleat:capability:env-clear"]
        with self.assertRaisesRegex(validation.ValidationError, "cleat:protocol>=12"):
            validation.validate_generation(document, GENERATION)
        document = generation()
        document["platform_independent"][0]["requires"] = ["flotilla:capability:stage-skills"]
        document["platforms"][validation.PLATFORMS[0]]["components"][0]["provides"].append("flotilla:capability:stage-skills")
        with self.assertRaisesRegex(validation.ValidationError, f"{platform}: skills requires flotilla:capability:stage-skills"):
            validation.validate_generation(document, GENERATION)
        pin = component("flotilla", platform, ["cleat:capability:env-clear"])
        with self.assertRaisesRegex(validation.ValidationError, "foreign"):
            validation.validate_component(pin)

    # Malformed, ambiguous or unpinned component metadata must refuse, while
    # optional human version labels and empty capability lists remain valid.
    def test_component_schema(self):
        valid = component("porthole", validation.PLATFORMS[0])
        validation.validate_component(valid)
        valid["version"] = "1.2.3"
        validation.validate_component(valid)
        invalid = {"schema_version": [True, 2], "kind": ["cache"], "component": ["../cleat", "", None],
                   "source_sha": ["main", "a" * 39, "A" * 40, None], "platform": ["linux", None, []],
                   "recipe_sha256": ["d" * 63, None], "archive_sha256": ["e" * 65, "E" * 64],
                   "version": ["", "  ", 1], "requires": [None, "cleat:protocol>=12", [1], ["cleat:protocol>12"], ["cleat:protocol>=-1"], ["cleat:flag", "cleat:flag"]],
                   "provides": [None, ["porthole:protocol>=1"], ["cleat:flag"], ["porthole:flag", "porthole:flag"], ["porthole:protocol=1", "porthole:protocol=2"]]}
        for field, values in invalid.items():
            for value in values:
                pin = copy.deepcopy(valid)
                pin[field] = value
                with self.subTest(field=field, value=value), self.assertRaises(validation.ValidationError):
                    validation.validate_component(pin)
        for field in valid:
            pin = copy.deepcopy(valid)
            del pin[field]
            if field == "version":
                validation.validate_component(pin)
            else:
                with self.subTest(missing=field), self.assertRaises(validation.ValidationError):
                    validation.validate_component(pin)
        valid["build_cache"] = "cargo-target"
        with self.assertRaises(validation.ValidationError):
            validation.validate_component(valid)

    # V2 refuses empty cohorts, misplaced/duplicate pins, conflicting source
    # identities, signing contracts and generation/protocol mismatches.
    def test_invalid_generation(self):
        invalid = []
        def changed(change):
            document = generation()
            change(document)
            invalid.append(document)
        linux, darwin = validation.PLATFORMS
        changed(lambda d: d.update(platforms={}))
        changed(lambda d: d.update(platforms=[]))
        changed(lambda d: d.update(platform_independent={}))
        changed(lambda d: d.update(signing={}))
        changed(lambda d: d.update(schema_version=True))
        changed(lambda d: d.update(peer_protocol_version=True))
        changed(lambda d: d.update(peer_protocol_version=21))
        changed(lambda d: d.update(generation="other"))
        changed(lambda d: d.update(extra="field"))
        changed(lambda d: d["platforms"].update(windows=d["platforms"][linux]))
        changed(lambda d: d["platforms"][linux].update(components=[]))
        for state in ("unknown", [], {}):
            changed(lambda d: d["platforms"][linux].update(state=state))
        changed(lambda d: d["platforms"][linux]["components"].append(copy.deepcopy(d["platforms"][linux]["components"][0])))
        changed(lambda d: d["platforms"][linux]["components"].append(copy.deepcopy(d["platform_independent"][0])))
        changed(lambda d: d["platform_independent"].append(copy.deepcopy(d["platform_independent"][0])))
        changed(lambda d: d["platforms"][linux]["components"][0].update(platform=darwin))
        changed(lambda d: d["platform_independent"][0].update(platform=linux))
        changed(lambda d: d["platforms"][darwin]["components"][0].update(source_sha="f" * 40))
        changed(lambda d: [d["platforms"][p]["components"][0].update(source_sha="f" * 40) for p in validation.PLATFORMS])
        changed(lambda d: d.update(platform_independent=[]))
        changed(lambda d: d["platforms"][linux]["components"].append(component("skills", linux)))
        changed(lambda d: d["platform_independent"].append(component("cleat", validation.INDEPENDENT_PLATFORM)))
        for index, document in enumerate(invalid):
            with self.subTest(index=index), self.assertRaises(validation.ValidationError):
                validation.validate_generation(document, GENERATION)
        document = generation()
        document["platforms"][linux]["state"] = "candidate"
        validation.validate_generation(document, GENERATION)
        validation.validate_generation(document, GENERATION, darwin, require_installable=True)
        for platform in (None, linux):
            with self.assertRaisesRegex(validation.ValidationError, "not installable"):
                validation.validate_generation(document, GENERATION, platform, require_installable=True)
        del document["platforms"][darwin]
        with self.assertRaisesRegex(validation.ValidationError, "no .* composition"):
            validation.validate_generation(document, GENERATION, darwin)

    # The transition preserves v1 decoding exactly; it cannot reinterpret a v2
    # pin list as a monolithic v1 release awaiting the #2764 installer.
    def test_v1_transition(self):
        fixture = Path(__file__).with_name("fixtures") / "valid.json"
        validation.validate_fixture(fixture)
        document = json.loads(fixture.read_text())
        sources, protocol = validation.validate_generation(document["manifest"], document["generation"])
        self.assertEqual(sources, document["manifest"]["sources"])
        self.assertEqual(protocol, 20)
        with self.assertRaisesRegex(validation.ValidationError, "component installer"):
            validation.validate_release("unused", generation(), validation.PLATFORMS[0])

    # Executable validators share the same v2 structural contract. A refused
    # composition names the exact missing capability rather than a traceback.
    def test_cli(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "generation.json"
            document = generation()
            path.write_text(json.dumps(document))
            result = subprocess.run([sys.executable, validation.__file__, "generation", str(path), GENERATION], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            document["platforms"][validation.PLATFORMS[0]]["components"][1]["provides"] = []
            path.write_text(json.dumps(document))
            result = subprocess.run([sys.executable, validation.__file__, "generation", str(path), GENERATION], capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("flotilla requires cleat:capability:env-clear", result.stderr)
            self.assertNotIn("Traceback", result.stderr)

    # A real detached signature authenticates exact manifest bytes to the
    # provisioned signer. Mutation, foreign signers and missing signatures fail.
    # Test certificates are generated locally; no live credentials are used.
    def test_signed_pin_list(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            def openssl(*args):
                subprocess.run(["openssl", *map(str, args)], check=True, capture_output=True)
            for name in ("trusted", "foreign"):
                openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", root / f"{name}.key",
                        "-out", root / f"{name}.pem", "-subj", f"/CN={name}", "-days", "1")
            manifest = root / "generation.json"
            manifest.write_text(json.dumps(generation()))
            signature = root / "generation.json.cms"
            openssl("cms", "-sign", "-binary", "-md", "sha256", "-in", manifest, "-signer", root / "trusted.pem",
                    "-inkey", root / "trusted.key", "-outform", "DER", "-out", signature)
            validation.verify_generation_signature(manifest, signature, root / "trusted.pem")
            for trust in (root / "foreign.pem", root / "missing.pem"):
                with self.assertRaises(validation.ValidationError):
                    validation.verify_generation_signature(manifest, signature, trust)
            manifest.write_text(manifest.read_text() + "\n")
            with self.assertRaises(validation.ValidationError):
                validation.verify_generation_signature(manifest, signature, root / "trusted.pem")
            with self.assertRaises(validation.ValidationError):
                validation.verify_generation_signature(manifest, root / "missing.cms", root / "trusted.pem")


if __name__ == "__main__":
    unittest.main()
