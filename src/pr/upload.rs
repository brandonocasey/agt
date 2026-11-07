//! `agt pr upload`: hosts images on the forge and prints the Markdown that shows them, for a
//! PR/MR body.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::Remote;
use crate::core::error::{Error, Result, fail};
use crate::core::output::{self, Output};
use crate::store::model::Forge;

/// The image types both forges render, by lowercase file extension.
const IMAGE_TYPES: [(&str, &str); 6] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
];

/// The only host whose upload endpoint is known; GitHub Enterprise Server's is not.
const GITHUB_HOST: &str = "github.com";

/// A checked image file: an absolute UTF-8 path to a non-empty regular file of a known type.
pub struct Image {
    pub path: PathBuf,
    pub name: String,
    pub content_type: &'static str,
}

/// An uploaded image: its address and the Markdown that shows it.
pub struct Uploaded {
    pub url: String,
    pub markdown: String,
}

impl Image {
    fn open(given: &Path) -> Result<Self> {
        let shown = given.display();
        // An absolute path also keeps glab from reading stdin for a file named `-`.
        let path = fs::canonicalize(given).map_err(|error| Error(format!("{shown}: {error}")))?;
        let meta = fs::metadata(&path).map_err(|error| Error(format!("{shown}: {error}")))?;
        if !meta.is_file() {
            return fail(format!("{shown} is not a file"));
        }
        if meta.len() == 0 {
            return fail(format!("{shown} is empty"));
        }
        let (Some(_), Some(name)) = (
            path.to_str(),
            path.file_name().and_then(|name| name.to_str()),
        ) else {
            return fail(format!("{shown} must have a UTF-8 path"));
        };
        let extension = Path::new(name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let Some((_, content_type)) = IMAGE_TYPES.iter().find(|(known, _)| *known == extension)
        else {
            let known: Vec<&str> = IMAGE_TYPES.iter().map(|(known, _)| *known).collect();
            return fail(format!(
                "{shown} is not an image type the forges render: use {}",
                known.join(", ")
            ));
        };
        Ok(Self {
            name: name.to_owned(),
            content_type,
            path,
        })
    }

    /// The file name without its extension, safe inside Markdown's `![...]`.
    pub fn alt(&self) -> String {
        let stem = Path::new(&self.name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(&self.name);
        stem.replace('\\', "\\\\")
            .replace('[', "\\[")
            .replace(']', "\\]")
    }
}

/// Uploads each file in turn. When one fails after others landed, prints those before failing.
pub fn run(remote: &Remote, files: &[PathBuf]) -> Result<Output> {
    remote.allow_write("agt pr upload")?;
    if remote.origin.forge == Forge::Github && remote.origin.host != GITHUB_HOST {
        return fail(format!(
            "agt pr upload supports GitHub only on {GITHUB_HOST}, not {}",
            remote.origin.host
        ));
    }
    let images = files
        .iter()
        .map(|file| Image::open(file))
        .collect::<Result<Vec<_>>>()?;
    let mut done = Vec::new();
    for image in &images {
        match remote.upload(image) {
            Ok(uploaded) => done.push(json!({
                "file": image.path,
                "url": uploaded.url,
                "markdown": uploaded.markdown,
            })),
            Err(error) => {
                if !done.is_empty() {
                    output::print(Output::Json(Value::Array(done)))?;
                }
                return fail(format!("upload of {} failed: {}", image.name, error.0));
            }
        }
    }
    Ok(Output::Json(Value::Array(done)))
}
