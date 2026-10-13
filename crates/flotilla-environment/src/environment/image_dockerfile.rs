//! Pure generation of one multi-stage Dockerfile per composed identity and
//! architecture (ADR 0062). Fragments stay opaque: the only text the generator
//! changes is each fragment's declared `FROM` lines, where `${BASE}` becomes the
//! previous layer's output stage and the output stage gains its generated
//! alias. Pins become per-stage `ARG`s emitted after every `FROM` line of their
//! layer, and each layer's declared context files form one named build context.
use std::collections::{BTreeMap, BTreeSet};

use flotilla_resources::{ImageContextFile, ImageLayerPin, ImageLayerSpec, ImageLayerTier};

/// The Dockerfile frontend every generated build uses. A fixed minor keeps the
/// frontend's language stable (`COPY --link`, `--chmod`, `RUN --mount=type=secret`)
/// while BuildKit receives patch releases; bumping it changes no identity.
pub const DOCKERFILE_SYNTAX: &str = "docker/dockerfile:1.17";

/// The text substituted in fragment `FROM` lines.
const BASE_PLACEHOLDER: &str = "${BASE}";

/// One generated build: the Dockerfile text, the named build contexts it
/// references, and the build secret names its fragments mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedBuild {
    pub dockerfile: String,
    /// One context per layer declaring context files, in layer order.
    pub contexts: Vec<GeneratedContext>,
    pub secrets: BTreeSet<String>,
}

/// A named build context (`--build-context <name>=<dir>`), named after its layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedContext {
    pub name: String,
    /// Relative path to content and executable bit.
    pub files: BTreeMap<String, ImageContextFile>,
}

/// The output stage of layer `index`.
pub fn layer_stage_name(index: usize) -> String {
    format!("flotilla-layer-{index}")
}

/// Generate the build for `layers` (ordered base first) on `architecture`.
/// Every layer must satisfy its per-definition validation; a pin with a
/// per-architecture checksum must name `architecture`.
pub fn generate_build(layers: &[ImageLayerSpec], architecture: &str) -> Result<GeneratedBuild, String> {
    let Some(base) = layers.first() else { return Err("an image build needs at least one layer".into()) };
    if base.tier != ImageLayerTier::Base {
        return Err(format!("image layer `{}` is first but is not a base layer", base.name));
    }
    if let Some(layer) = layers.iter().skip(1).find(|layer| layer.tier == ImageLayerTier::Base) {
        return Err(format!("image layer `{}` is a base layer above the first layer", layer.name));
    }
    let mut stage_names = BTreeSet::new();
    let mut context_names = BTreeSet::new();
    let mut dockerfile = format!("# syntax={DOCKERFILE_SYNTAX}\n");
    let mut contexts = Vec::new();
    let mut secrets = BTreeSet::new();
    for (index, layer) in layers.iter().enumerate() {
        layer.validate()?;
        let name = &layer.name;
        let base = match &layer.from {
            Some(from) => from.clone(),
            None => layer_stage_name(index.checked_sub(1).ok_or_else(|| format!("image layer `{name}` has no base image"))?),
        };
        let output = layer_stage_name(index);
        let args = pin_args(layer, architecture)?;
        dockerfile.push_str(&format!("\n# flotilla layer {index}: {name} {}\n", layer.version));
        let mut lines = layer.fragment.split_inclusive('\n').peekable();
        while let Some(line) = lines.next() {
            let Some(stage) = from_line(line) else {
                dockerfile.push_str(line);
                if lines.peek().is_none() && !line.ends_with('\n') {
                    dockerfile.push('\n');
                }
                continue;
            };
            let alias = match stage.alias {
                Some(alias) => alias.to_ascii_lowercase(),
                None => output.clone(),
            };
            if !stage_names.insert(alias.clone()) {
                return Err(format!("image layer `{name}`: stage name `{alias}` is already used by an earlier stage"));
            }
            let (body, ending) = split_ending(line);
            let body = if stage.image == BASE_PLACEHOLDER { body.replacen(BASE_PLACEHOLDER, &base, 1) } else { body.to_string() };
            dockerfile.push_str(&body);
            if stage.alias.is_none() {
                dockerfile.push_str(" AS ");
                dockerfile.push_str(&output);
            }
            dockerfile.push_str(if ending.is_empty() { "\n" } else { ending });
            for arg in &args {
                dockerfile.push_str(arg);
                dockerfile.push('\n');
            }
        }
        if !layer.context_files.is_empty() {
            context_names.insert(name.clone());
            contexts.push(GeneratedContext { name: name.clone(), files: layer.context_files.clone() });
        }
        secrets.extend(layer.build_secrets.iter().cloned());
    }
    if let Some(clash) = context_names.intersection(&stage_names).next() {
        return Err(format!("image layer `{clash}`: its named build context clashes with a stage of the same name"));
    }
    Ok(GeneratedBuild { dockerfile, contexts, secrets })
}

struct FromLine<'a> {
    image: &'a str,
    alias: Option<&'a str>,
}

/// The same reading of a `FROM` line as the declared fragment contract: the
/// first whitespace-separated word is `FROM` (any case). Validation has already
/// refused every other shape.
fn from_line(line: &str) -> Option<FromLine<'_>> {
    let mut words = line.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case("FROM") {
        return None;
    }
    let image = words.next()?;
    let alias = match (words.next(), words.next()) {
        (Some(keyword), Some(alias)) if keyword.eq_ignore_ascii_case("AS") => Some(alias),
        _ => None,
    };
    Some(FromLine { image, alias })
}

/// Split a line into its trimmed body and its original line ending. Trailing
/// spaces before the ending are dropped from FROM lines only, so the alias
/// follows the image directly.
fn split_ending(line: &str) -> (&str, &str) {
    let without_newline = line.strip_suffix('\n').unwrap_or(line);
    let without_return = without_newline.strip_suffix('\r').unwrap_or(without_newline);
    (without_return.trim_end(), &line[without_return.len()..])
}

/// `ARG PIN_<NAME>_VERSION/_URL/_SHA256` lines for one layer, in pin-name order.
fn pin_args(layer: &ImageLayerSpec, architecture: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    for (pin, ImageLayerPin { version, url, sha256 }) in &layer.pins {
        let name = pin.to_ascii_uppercase().replace('-', "_");
        args.push(arg_line(&format!("PIN_{name}_VERSION"), version, &layer.name)?);
        if let Some(url) = url {
            args.push(arg_line(&format!("PIN_{name}_URL"), url, &layer.name)?);
        }
        if let Some(sha256) = sha256 {
            let hex = sha256
                .for_architecture(architecture)
                .ok_or_else(|| format!("image layer `{}`: pin `{pin}` has no sha256 for architecture `{architecture}`", layer.name))?;
            args.push(arg_line(&format!("PIN_{name}_SHA256"), hex, &layer.name)?);
        }
    }
    Ok(args)
}

/// Values are emitted verbatim when they need no quoting (including `{arch}`
/// placeholders). Others are double-quoted with Dockerfile escapes so `$`,
/// quotes and whitespace stay literal. Control characters cannot be expressed.
fn arg_line(key: &str, value: &str, layer: &str) -> Result<String, String> {
    if value.chars().any(char::is_control) {
        return Err(format!("image layer `{layer}`: `{key}` contains a control character"));
    }
    let plain = !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || "-._~:/?#[]@!&()*+,;=%{}".contains(c));
    if plain {
        return Ok(format!("ARG {key}={value}"));
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    Ok(format!("ARG {key}=\"{quoted}\""))
}

#[cfg(test)]
#[path = "image_dockerfile_tests.rs"]
mod tests;
