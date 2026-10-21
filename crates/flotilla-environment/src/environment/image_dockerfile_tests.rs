use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use flotilla_resources::{
    parse_layer_directory_with_executables, ImageContextFile, ImageLayerPin, ImageLayerSource, ImageLayerSpec, ImageLayerTier,
    ImagePinDigest,
};
use hegel::generators as gs;

use super::{generate_build, layer_stage_name, DOCKERFILE_SYNTAX};

const BASE_FROM: &str = "registry.example/base@sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn hex(fill: char) -> String {
    fill.to_string().repeat(64)
}

fn layer(name: &str, tier: ImageLayerTier, fragment: &str) -> ImageLayerSpec {
    ImageLayerSpec::builder()
        .name(name.into())
        .version("1.0".into())
        .tier(tier)
        .source(ImageLayerSource::Stdlib)
        .maybe_from((tier == ImageLayerTier::Base).then(|| BASE_FROM.to_string()))
        .fragment(fragment.into())
        .probe("true".into())
        .build()
}

fn is_from(line: &str) -> bool {
    line.split_whitespace().next().is_some_and(|word| word.eq_ignore_ascii_case("FROM"))
}

/// Independent oracle for one layer's pin arguments (values in these fixtures
/// never need quoting).
fn expected_args(layer: &ImageLayerSpec, architecture: &str) -> Vec<String> {
    let mut args = Vec::new();
    for (name, pin) in &layer.pins {
        let key = name.to_uppercase().replace('-', "_");
        args.push(format!("ARG PIN_{key}_VERSION={}\n", pin.version));
        if let Some(url) = &pin.url {
            args.push(format!("ARG PIN_{key}_URL={url}\n"));
        }
        if let Some(sha256) = &pin.sha256 {
            let hex = match sha256 {
                ImagePinDigest::Single(hex) => hex,
                ImagePinDigest::PerArchitecture(map) => &map[architecture],
            };
            args.push(format!("ARG PIN_{key}_SHA256={hex}\n"));
        }
    }
    args
}

/// Walk the generated Dockerfile against the layers: every non-FROM fragment
/// line appears byte-for-byte in order; every FROM line has `${BASE}` replaced
/// by the previous output stage (or the base's `from`), the output stage gains
/// `AS flotilla-layer-<i>`, and exactly the layer's pin ARGs follow each FROM.
fn assert_structure(layers: &[ImageLayerSpec], architecture: &str, dockerfile: &str) {
    let mut out = dockerfile.split_inclusive('\n');
    assert_eq!(out.next(), Some(format!("# syntax={DOCKERFILE_SYNTAX}\n").as_str()));
    for (index, layer) in layers.iter().enumerate() {
        assert_eq!(out.next(), Some("\n"));
        assert_eq!(out.next(), Some(format!("# flotilla layer {index}: {} {}\n", layer.name, layer.version).as_str()));
        let previous = if index == 0 { layer.from.clone().expect("base from") } else { layer_stage_name(index - 1) };
        let args = expected_args(layer, architecture);
        let mut outputs = 0;
        for line in layer.fragment.split_inclusive('\n') {
            let got = out.next().expect("generated line");
            if !is_from(line) {
                let expected = if line.ends_with('\n') { line.to_string() } else { format!("{line}\n") };
                assert_eq!(got, expected, "non-FROM fragment text is preserved");
                continue;
            }
            let mut expected: Vec<String> =
                line.split_whitespace().map(|word| if word == "${BASE}" { previous.clone() } else { word.to_string() }).collect();
            if expected.len() == 2 {
                expected.extend(["AS".to_string(), layer_stage_name(index)]);
                outputs += 1;
            }
            assert_eq!(got.split_whitespace().collect::<Vec<_>>(), expected, "FROM line chains to the previous stage");
            assert!(got.ends_with('\n'));
            for arg in &args {
                assert_eq!(out.next(), Some(arg.as_str()), "pin ARGs follow every FROM line");
            }
        }
        assert_eq!(outputs, 1, "each layer has exactly one output stage");
    }
    assert_eq!(out.next(), None);
}

// Golden: a small composition with a per-architecture pin, a fetch stage, a
// scratch stage and a context file renders exactly this Dockerfile.
#[test]
fn small_composition_renders_golden_dockerfile() {
    let mut base = layer("base", ImageLayerTier::Base, "FROM ${BASE}\nRUN apt-get update\n");
    base.pins.insert("apt-snapshot".into(), ImageLayerPin { version: "20261012T000000Z".into(), url: None, sha256: None });
    let mut tool = layer(
        "tool",
        ImageLayerTier::Shared,
        "FROM scratch AS tool-empty\nFROM ${BASE} AS tool-fetch\nRUN fetch \"${PIN_TOOL_URL}\"\nFROM ${BASE}\nCOPY --link --from=tool-fetch /opt/tool /opt/tool\nCOPY --from=tool --chmod=0755 bin/run /usr/local/bin/run",
    );
    tool.pins.insert(
        "tool".into(),
        ImageLayerPin {
            version: "2.0".into(),
            url: Some("https://example.test/tool-{arch}.tar.gz".into()),
            sha256: Some(ImagePinDigest::PerArchitecture(BTreeMap::from([("amd64".into(), hex('a')), ("arm64".into(), hex('b'))]))),
        },
    );
    tool.context_files.insert("bin/run".into(), ImageContextFile { content: "#!/bin/sh\n".into(), executable: true });
    tool.build_secrets.insert("npm-token".into());
    let generated = generate_build(&[base, tool.clone()], "arm64").expect("generate");
    let tool_args =
        format!("ARG PIN_TOOL_VERSION=2.0\nARG PIN_TOOL_URL=https://example.test/tool-{{arch}}.tar.gz\nARG PIN_TOOL_SHA256={}\n", hex('b'));
    let expected = format!(
        "# syntax=docker/dockerfile:1.17\n\
         \n# flotilla layer 0: base 1.0\n\
         FROM {BASE_FROM} AS flotilla-layer-0\n\
         ARG PIN_APT_SNAPSHOT_VERSION=20261012T000000Z\n\
         RUN apt-get update\n\
         \n# flotilla layer 1: tool 1.0\n\
         FROM scratch AS tool-empty\n{tool_args}\
         FROM flotilla-layer-0 AS tool-fetch\n{tool_args}\
         RUN fetch \"${{PIN_TOOL_URL}}\"\n\
         FROM flotilla-layer-0 AS flotilla-layer-1\n{tool_args}\
         COPY --link --from=tool-fetch /opt/tool /opt/tool\n\
         COPY --from=tool --chmod=0755 bin/run /usr/local/bin/run\n"
    );
    assert_eq!(generated.dockerfile, expected);
    assert_eq!(generated.contexts.len(), 1);
    assert_eq!(generated.contexts[0].name, "tool");
    assert_eq!(generated.contexts[0].files, tool.context_files);
    assert_eq!(generated.secrets, BTreeSet::from(["npm-token".to_string()]));
}

// A per-architecture checksum without the target architecture refuses the
// build, naming the pin and the architecture; invalid compositions are refused.
#[test]
fn refuses_missing_architecture_checksum_and_malformed_compositions() {
    let base = layer("base", ImageLayerTier::Base, "FROM ${BASE}\n");
    let mut tool = layer("tool", ImageLayerTier::Shared, "FROM ${BASE}\n");
    tool.pins.insert(
        "codex".into(),
        ImageLayerPin {
            version: "1".into(),
            url: Some("https://example.test/x".into()),
            sha256: Some(ImagePinDigest::PerArchitecture(BTreeMap::from([("amd64".into(), hex('a'))]))),
        },
    );
    generate_build(&[base.clone(), tool.clone()], "amd64").expect("amd64 checksum present");
    let error = generate_build(&[base.clone(), tool.clone()], "arm64").expect_err("arm64 checksum absent");
    assert!(error.contains("`codex`") && error.contains("`arm64`") && error.contains("`tool`"), "{error}");

    assert!(generate_build(&[], "amd64").is_err());
    assert!(generate_build(std::slice::from_ref(&tool), "amd64").expect_err("no base").contains("not a base"));
    assert!(generate_build(&[base.clone(), base.clone()], "amd64").expect_err("second base").contains("base layer above"));
    let invalid = layer("bad", ImageLayerTier::Shared, "FROM ubuntu\n");
    assert!(generate_build(&[base.clone(), invalid], "amd64").is_err(), "per-definition validation applies");

    // Stage aliases are prefixed by their layer name, so `a-b-x` is valid for
    // both `a` and `a-b`; the generated Dockerfile cannot hold both.
    let first = layer("a", ImageLayerTier::Shared, "FROM ${BASE} AS a-b-x\nFROM ${BASE}\n");
    let second = layer("a-b", ImageLayerTier::Shared, "FROM ${BASE} AS a-b-x\nFROM ${BASE}\n");
    assert!(generate_build(&[base.clone(), first.clone(), second], "amd64").expect_err("stage clash").contains("a-b-x"));
    // A named context shadowing a stage name is refused too.
    let mut context = layer("a-b-x", ImageLayerTier::Shared, "FROM ${BASE}\n");
    context.context_files.insert("f".into(), ImageContextFile { content: String::new(), executable: false });
    assert!(generate_build(&[base, first, context], "amd64").expect_err("context clash").contains("a-b-x"));
}

// Values that need quoting stay literal: `$`, quotes and spaces are escaped.
#[test]
fn pin_values_needing_quotes_are_escaped() {
    let mut base = layer("base", ImageLayerTier::Base, "FROM ${BASE}\n");
    base.pins.insert("odd".into(), ImageLayerPin { version: "1 $HOME \"q\" \\".into(), url: None, sha256: None });
    let generated = generate_build(&[base.clone()], "amd64").expect("generate");
    assert!(generated.dockerfile.contains("ARG PIN_ODD_VERSION=\"1 \\$HOME \\\"q\\\" \\\\\"\n"), "{}", generated.dockerfile);
    base.pins.insert("odd".into(), ImageLayerPin { version: "1\n2".into(), url: None, sha256: None });
    assert!(generate_build(&[base], "amd64").is_err());
}

/// Independent reading of a Dockerfile `ARG KEY=value` line: an unquoted
/// value is the rest of the line verbatim (no shell runs, and `#` starts a
/// comment only at the start of a line); a double-quoted value drops its
/// quotes and backslash escapes.
fn dockerfile_arg_value(dockerfile: &str, key: &str) -> Option<String> {
    let line = dockerfile.lines().find_map(|line| line.strip_prefix(&format!("ARG {key}=")))?;
    let Some(quoted) = line.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) else {
        assert!(!line.contains(char::is_whitespace) && !line.contains(['"', '\\', '$', '`']), "unquoted value `{line}` is plain");
        return Some(line.to_string());
    };
    let mut value = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        value.push(if c == '\\' { chars.next().expect("escaped character") } else { c });
    }
    Some(value)
}

// Pin values with URL and shell punctuation (`&`, `#`, `;`, `?`) reach the
// build unchanged, whether emitted plain or quoted.
#[test]
fn pin_values_with_url_punctuation_round_trip() {
    let values = [
        "https://example.test/dl?version=1.2&arch=x86_64#sha256;x",
        "a&b",
        "#leading-hash",
        "a;b",
        "?query",
        "a & b # c ; d ? e",
        "$HOME&#;?",
    ];
    for value in values {
        let mut base = layer("base", ImageLayerTier::Base, "FROM ${BASE}\n");
        base.pins.insert(
            "tool".into(),
            ImageLayerPin { version: "1".into(), url: Some(value.into()), sha256: Some(ImagePinDigest::Single(hex('a'))) },
        );
        let generated = generate_build(&[base], "amd64").expect("generate");
        assert_eq!(dockerfile_arg_value(&generated.dockerfile, "PIN_TOOL_URL").as_deref(), Some(value), "{}", generated.dockerfile);
    }
}

// The fragment contract reads any line whose first word is FROM as a stage
// line, including one inside a RUN heredoc. A heredoc line naming another
// image would otherwise become a stage, so it is refused loudly rather than
// passed through as heredoc text.
#[test]
fn from_lines_inside_run_heredocs_are_stage_lines_and_refused() {
    let base = layer("base", ImageLayerTier::Base, "FROM ${BASE}\n");
    let heredoc = layer("tool", ImageLayerTier::Shared, "FROM ${BASE}\nRUN <<EOF\necho start\nFROM ubuntu:24.04\nEOF\n");
    let error = heredoc.validate().expect_err("a non-${BASE} FROM inside a heredoc is refused");
    assert!(error.contains("ubuntu:24.04"), "{error}");
    assert!(generate_build(&[base.clone(), heredoc], "amd64").is_err());
    let indented = layer("tool", ImageLayerTier::Shared, "FROM ${BASE}\nRUN <<EOF\n  from x\nEOF\n");
    assert!(indented.validate().is_err(), "leading whitespace and case do not hide a FROM line");
}

const BODY_LINES: &[&str] = &[
    "RUN echo ready",
    "",
    "# comment FROM ${BASE}",
    "  RUN echo ${BASE}",
    "RUN echo FROM ${BASE}",
    "ENV PATH=\"/opt/x/bin:${PATH}\"\r",
    "ARG PIN_FAKE_VERSION=0",
    "    && from-cache",
    "COPY --from=l0 a b",
    "RUN --mount=type=secret,id=token cat /run/secrets/token",
];

// Generated span: 1..5 layers (base plus shared/agent-runtime), each with 0..2
// aliased `${BASE}` or `scratch` stages before its output stage, FROM keywords
// in any case and spacing, CRLF and missing final newlines, 0..3 body lines
// drawn from tricky text (fake FROM/ARG text, `${BASE}` outside FROM), 0..2
// pins with single or per-architecture checksums, and both architectures.
#[hegel::test]
fn generated_dockerfile_is_deterministic_chained_and_preserves_fragments(tc: hegel::TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let architecture = if tc.draw(gs::booleans()) { "amd64" } else { "arm64" };
    let mut layers = Vec::new();
    for index in 0..count {
        let name = format!("l{index}");
        let tier = match index {
            0 => ImageLayerTier::Base,
            _ if tc.draw(gs::booleans()) => ImageLayerTier::Shared,
            _ => ImageLayerTier::AgentRuntime,
        };
        let keyword = ["FROM", "from", "From"][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let gap = if tc.draw(gs::booleans()) { " " } else { "  " };
        let body = |tc: &hegel::TestCase| {
            let lines = tc.draw(gs::integers::<usize>().min_value(0).max_value(3));
            (0..lines)
                .map(|_| format!("{}\n", BODY_LINES[tc.draw(gs::integers::<usize>().min_value(0).max_value(BODY_LINES.len() - 1))]))
                .collect::<String>()
        };
        let mut fragment = String::new();
        let stages = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
        for stage in 0..stages {
            let image = if tc.draw(gs::booleans()) { "${BASE}" } else { "scratch" };
            fragment.push_str(&format!("{keyword}{gap}{image} AS {name}-s{stage}\n"));
            fragment.push_str(&body(&tc));
        }
        let ending = if tc.draw(gs::booleans()) { "\r\n" } else { "\n" };
        fragment.push_str(&format!("{keyword}{gap}${{BASE}}{ending}"));
        fragment.push_str(&body(&tc));
        if tc.draw(gs::booleans()) {
            fragment.push_str("RUN true");
        }
        let mut spec = layer(&name, tier, &fragment);
        for pin in 0..tc.draw(gs::integers::<usize>().min_value(0).max_value(2)) {
            let sha256 = match tc.draw(gs::integers::<usize>().min_value(0).max_value(2)) {
                0 => None,
                1 => Some(ImagePinDigest::Single(hex('c'))),
                _ => Some(ImagePinDigest::PerArchitecture(BTreeMap::from([("amd64".into(), hex('a')), ("arm64".into(), hex('b'))]))),
            };
            // Only a per-architecture digest may pair with an `{arch}` URL;
            // validation rejects a single digest for an arch-varying URL.
            let url = sha256.as_ref().map(|digest| match digest {
                ImagePinDigest::Single(_) => format!("https://example.test/p{pin}"),
                ImagePinDigest::PerArchitecture(_) => format!("https://example.test/p{pin}/{{arch}}"),
            });
            spec.pins.insert(format!("p-{pin}"), ImageLayerPin { version: format!("{pin}.0"), url, sha256 });
        }
        if tc.draw(gs::booleans()) {
            spec.context_files
                .insert(format!("files/{index}"), ImageContextFile { content: "x".into(), executable: tc.draw(gs::booleans()) });
        }
        layers.push(spec);
    }
    let generated = generate_build(&layers, architecture).expect("valid composition generates");
    assert_eq!(generate_build(&layers.clone(), architecture).expect("again"), generated, "generation is deterministic");
    assert_structure(&layers, architecture, &generated.dockerfile);
    let expected_contexts: Vec<_> = layers.iter().filter(|layer| !layer.context_files.is_empty()).map(|layer| layer.name.clone()).collect();
    assert_eq!(generated.contexts.iter().map(|context| context.name.clone()).collect::<Vec<_>>(), expected_contexts);
}

fn read_layer(directory: &Path) -> ImageLayerSpec {
    let mut files = BTreeMap::new();
    let mut executable = BTreeSet::new();
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, Vec<u8>>, executable: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(path).expect("layer directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                visit(root, &path, files, executable);
                continue;
            }
            let relative = path.strip_prefix(root).expect("relative").to_str().expect("UTF-8").to_string();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o111 != 0 {
                    executable.insert(relative.clone());
                }
            }
            files.insert(relative, std::fs::read(&path).expect("file"));
        }
    }
    visit(directory, directory, &mut files, &mut executable);
    parse_layer_directory_with_executables(&files, &executable, ImageLayerSource::Stdlib, None).expect("stdlib layer parses")
}

// The shipped stdlib layers (`stdlib/image-layers/` at the workspace root,
// the same directories the daemon embeds) compose into one Dockerfile per
// architecture: the
// structural contract holds, `{arch}` pin URLs are emitted verbatim, the
// architecture's checksum is selected, and declared context files (with
// their executable bits) become one named context per layer.
#[test]
fn stdlib_layers_generate_for_both_architectures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stdlib/image-layers");
    let order = [
        "base-ubuntu-24.04",
        "display-x11",
        "formal-modelling",
        "node",
        "rust",
        "zig",
        "crew-utilities",
        "harness-claude-code",
        "harness-codex",
    ];
    let layers: Vec<_> = order.iter().map(|name| read_layer(&root.join(name))).collect();
    assert_eq!(layers.iter().map(|layer| layer.name.as_str()).collect::<Vec<_>>(), order);
    for architecture in ["amd64", "arm64"] {
        let generated = generate_build(&layers, architecture).expect("stdlib generates");
        assert_structure(&layers, architecture, &generated.dockerfile);
        let codex = &layers[8].pins["codex"];
        let ImagePinDigest::PerArchitecture(digests) = codex.sha256.as_ref().expect("codex sha") else { panic!("per-arch codex pin") };
        assert!(generated.dockerfile.contains(&format!("ARG PIN_CODEX_URL={}\n", codex.url.as_deref().expect("url"))));
        assert!(generated.dockerfile.contains("codex-package-{arch}-unknown-linux-musl.tar.gz"));
        assert!(generated.dockerfile.contains(&format!("ARG PIN_CODEX_SHA256={}\n", digests[architecture])));
        assert!(generated.dockerfile.contains("FROM flotilla-layer-7 AS harness-codex-fetch\n"));
        assert!(generated.dockerfile.contains("FROM flotilla-layer-7 AS flotilla-layer-8\n"));
        let contexts: Vec<_> = generated.contexts.iter().map(|context| context.name.as_str()).collect();
        assert_eq!(contexts, ["display-x11", "formal-modelling", "rust", "crew-utilities"]);
        let formal = &generated.contexts[1].files;
        assert!(formal["tlc"].executable && formal["probe.sh"].executable && !formal["package.json"].executable);
    }
}

// Glue: the two shipped layer definitions must deliver the shell contract
// through the generated Dockerfile and check it in their runtime probes.
#[test]
fn stdlib_layers_provide_and_probe_crew_shells() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../stdlib/image-layers");
    let base = read_layer(&root.join("base-ubuntu-24.04"));
    let utilities = read_layer(&root.join("crew-utilities"));
    let generated = generate_build(&[base.clone(), utilities.clone()], "amd64").expect("shell layers generate");
    assert!(generated.dockerfile.lines().any(|line| line == "ENV SHELL=/bin/bash"));
    assert!(base.probe.contains("test \"$SHELL\" = /bin/bash"));
    let installs = utilities.fragment.split("apt-get install").skip(1);
    assert!(installs.filter_map(|install| install.split(';').next()).flat_map(str::split_whitespace).any(|package| package == "zsh"));
    let tools = utilities.probe.lines().find_map(|line| line.trim().strip_prefix("for tool in ")).expect("tool probe loop");
    assert!(tools.split(';').next().expect("probed tools").split_whitespace().any(|tool| tool == "zsh"));
}
