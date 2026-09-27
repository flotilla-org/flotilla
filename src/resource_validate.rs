use std::path::{Path, PathBuf};

use color_eyre::{eyre::eyre, Result};
use flotilla_resources::validate_resource_document;
use serde::Deserialize;
use serde_json::Value;

pub fn validate_path(path: &Path) -> Result<()> {
    let mut files = Vec::new();
    collect_files(path, &mut files)?;
    if files.is_empty() {
        return Err(eyre!("no JSON or YAML resource documents found under {}", path.display()));
    }
    files.sort();
    let mut failed = false;
    for file in &files {
        let content = match std::fs::read_to_string(file) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                failed = true;
                continue;
            }
        };
        let documents = parse_documents(file, &content);
        match documents {
            Ok(documents) => {
                for (index, document) in documents.iter().enumerate() {
                    let label = format!("{}#{}", file.display(), index + 1);
                    match validate_resource_document(document) {
                        Ok(()) => println!("{label}: valid"),
                        Err(error) => {
                            eprintln!("{label}: {error}");
                            failed = true;
                        }
                    }
                }
            }
            Err(error) => {
                eprintln!("{}: {error}", file.display());
                failed = true;
            }
        }
    }
    if failed {
        Err(eyre!("resource validation failed"))
    } else {
        Ok(())
    }
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_file() {
        files.push(path.to_path_buf());
    } else if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let candidate = entry.path();
            if candidate.is_dir() {
                collect_files(&candidate, files)?;
            } else if matches!(candidate.extension().and_then(|ext| ext.to_str()), Some("json" | "yaml" | "yml")) {
                files.push(candidate);
            }
        }
    } else {
        return Err(eyre!("resource path does not exist: {}", path.display()));
    }
    Ok(())
}

fn parse_documents(path: &Path, content: &str) -> Result<Vec<Value>> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
        return Ok(vec![serde_json::from_str(content)?]);
    }
    let mut documents = Vec::new();
    for document in serde_yml::Deserializer::from_str(content) {
        let value = Value::deserialize(document)?;
        if !value.is_null() {
            documents.push(value);
        }
    }
    if documents.is_empty() {
        Err(eyre!("no resource documents"))
    } else {
        Ok(documents)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use flotilla_resources::validate_resource_document;

    use super::parse_documents;

    #[test]
    fn reports_the_nested_field_of_a_stale_manifest() {
        let yaml = "apiVersion: flotilla.work/v1\nkind: CredentialSpec\nmetadata:\n  name: lab\nspec:\n  consumer:\n    adapter: forgejo\n    server_url: https://forge.example\n    username: bot\n  source: {}\n  lifecycle: {}\n";
        let document = &parse_documents(Path::new("credential.yaml"), yaml).expect("parse yaml")[0];
        let error = validate_resource_document(document).expect_err("old Forgejo shape must fail");
        assert!(error.to_string().contains("spec.consumer"), "{error}");
        assert!(error.to_string().contains("forge_ref"), "{error}");
    }

    #[test]
    fn parses_multiple_yaml_documents() {
        let yaml = "kind: PlacementPolicy\n---\nkind: Forge\n";
        assert_eq!(parse_documents(Path::new("resources.yaml"), yaml).expect("parse yaml").len(), 2);
    }
}
