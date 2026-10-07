"""Compile a job's target union once, then preserve each selector's test scope."""
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def run(commands):
    if not commands:
        return
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], text=True, encoding="utf-8"))
    packages = {p["name"]: p for p in metadata["packages"]}
    default = next(p["name"] for p in metadata["packages"]
                   if Path(p["manifest_path"]).parent == Path(metadata["workspace_root"]))
    rows = []
    union = []
    for command in commands:
        tokens = command.split()
        package = default
        explicit_package = None
        targets = []
        features = []
        filters = []
        harness = []
        index = 0
        while index < len(tokens):
            token = tokens[index]
            if token == "--":
                harness = tokens[index + 1:]
                break
            if token in ("-p", "--package", "--bin", "--test", "--features"):
                index += 1
                if index == len(tokens):
                    raise ValueError(f"missing value for {token}")
                value = tokens[index]
                if token in ("-p", "--package"):
                    if explicit_package is not None and explicit_package != value:
                        raise ValueError("each selector must name one package")
                    explicit_package = package = value
                elif token == "--features":
                    if "/" in value:
                        raise ValueError("selector features must be bare names, not package-qualified")
                    features.extend(value.split(","))
                else:
                    targets.append((token[2:], value))
            elif token == "--lib":
                targets.append(("lib", None))
            elif token != "--locked":
                if token.startswith("-"):
                    raise ValueError(f"unsupported selector option: {token}")
                filters.append(token)
            index += 1
        if package not in packages:
            raise ValueError(f"unknown selector package: {package}")
        if not targets:
            raise ValueError("selectors require --lib, --bin or --test; implicit targets include doctests")
        rows.append((packages[package]["id"], targets, filters, harness))
        for args in [("-p", package)] + [("--" + kind,) if name is None else ("--" + kind, name)
                                         for kind, name in targets] + [("--features", f"{package}/{f}") for f in features]:
            if args not in union:
                union.append(args)
    version = subprocess.check_output(["rustc", "-vV"], text=True, encoding="utf-8")
    host = next((line.removeprefix("host: ") for line in version.splitlines() if line.startswith("host: ")), None)
    if not host:
        raise ValueError("rustc did not report its host target")
    runner = json.dumps([sys.executable, str(Path(__file__).resolve()), "--dispatch"])
    common = ["cargo", "--config", 'profile.dev.package."*".debug=0',
              "--config", f"target.{host}.runner={runner}", "test", "--locked", "--target", host]
    common += [arg for args in union for arg in args]
    argv = common + ["--no-run", "--message-format=json", "--timings"]
    print("Unified build:", " ".join(argv), flush=True)
    build_start = time.monotonic()
    artifacts = []
    with subprocess.Popen(argv, stdout=subprocess.PIPE, text=True, encoding="utf-8") as build:
        for line in build.stdout:
            try:
                message = json.loads(line)
            except ValueError as error:
                raise ValueError(f"invalid Cargo build JSON: {line.rstrip()}") from error
            if message["reason"] == "compiler-artifact" and message.get("executable") and message["profile"]["test"]:
                artifacts.append(message)
            elif message["reason"] == "compiler-message":
                print(message["message"].get("rendered", ""), file=sys.stderr, end="")
        if build.wait():
            raise subprocess.CalledProcessError(build.returncode, argv)
    print(f"Unified build seconds: {time.monotonic() - build_start:.2f}", flush=True)
    # Cargo owns the runtime environment, including native library search paths
    # and package variables. Every invocation keeps the same feature graph; the
    # target runner prevents matching filters from leaking into other binaries.
    for package, targets, filters, harness in rows:
        matched = [a for a in artifacts if a["package_id"] == package and any(
            kind_matches(kind, a["target"]["kind"]) and (name is None or name == a["target"]["name"])
            for kind, name in targets)]
        if not matched:
            raise ValueError(f"no test binary for {package}: {targets}")
        print("Selected targets:", package, [a["target"]["name"] for a in matched], filters + harness, flush=True)
        test_start = time.monotonic()
        selected = {os.path.normcase(str(Path(a["executable"]).resolve())) for a in matched}
        with tempfile.TemporaryDirectory() as receipt_directory:
            receipt = Path(receipt_directory) / "executed.jsonl"
            subprocess.run(common + filters + ["--", *harness], check=True,
                           env=dict(os.environ, FLOTILLA_SELECTED_TEST_BINARIES=json.dumps(sorted(selected)),
                                    FLOTILLA_TEST_DISPATCH_LOG=str(receipt)))
            executed = {json.loads(line) for line in receipt.read_text(encoding="utf-8").splitlines()} if receipt.exists() else set()
            missing = selected - executed
            if missing:
                raise ValueError(f"selected test binaries did not execute: {sorted(missing)}")
        print(f"Selected test seconds (including Cargo dispatch): {time.monotonic() - test_start:.2f}", flush=True)


def kind_matches(selector, kinds):
    # Cargo may describe library targets with their crate types instead of `lib`.
    libraries = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
    return bool(libraries.intersection(kinds)) if selector == "lib" else selector in kinds


def dispatch(binary, arguments):
    selected = {os.path.normcase(path) for path in json.loads(os.environ["FLOTILLA_SELECTED_TEST_BINARIES"])}
    canonical = os.path.normcase(str(Path(binary).resolve()))
    if canonical in selected:
        subprocess.run([binary, *arguments], check=True)
        with open(os.environ["FLOTILLA_TEST_DISPATCH_LOG"], "a", encoding="utf-8") as receipt:
            receipt.write(json.dumps(canonical) + "\n")


if __name__ == "__main__":
    try:
        if sys.argv[1:2] == ["--dispatch"]:
            dispatch(sys.argv[2], sys.argv[3:])
        else:
            run(sys.argv[1:])
    except (ValueError, KeyError, StopIteration) as error:
        print(f"invalid selector or Cargo metadata: {error}", file=sys.stderr)
        sys.exit(2)
    except subprocess.CalledProcessError as error:
        status = error.returncode
        if status < 0 and os.name == "posix":
            status = 128 - status
        sys.exit(status)
