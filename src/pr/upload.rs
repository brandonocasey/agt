//! `agt pr upload`: hosts images on the forge and prints the Markdown that shows them, for a
//! PR/MR body.

use std::fs;
use std::path::{self, Path, PathBuf};

use serde_json::{Value, json};

use super::provider::Provider;
use crate::core::error::{Error, Result, fail};
use crate::core::output::{self, Output};

/// The image types both forges render, by lowercase file extension.
const IMAGE_TYPES: [(&str, &str); 6] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
];

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

/// Uploads one image per call.
pub type Uploader<'a> = Box<dyn Fn(&Image) -> Result<Uploaded> + 'a>;

impl Image {
    fn open(given: &Path) -> Result<Self> {
        let shown = given.display();
        // An absolute path also keeps glab from reading stdin for a file named `-`. A symlink
        // keeps its own name, which names the upload and decides its type.
        let path = path::absolute(given).map_err(|error| Error(format!("{shown}: {error}")))?;
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
pub fn run(provider: &dyn Provider, files: &[PathBuf]) -> Result<Output> {
    provider.allow_write("agt pr upload")?;
    provider.upload_check()?;
    let images = files
        .iter()
        .map(|file| Image::open(file))
        .collect::<Result<Vec<_>>>()?;
    let upload = provider.uploader()?;
    let mut done = Vec::new();
    for image in &images {
        match upload(image) {
            Ok(uploaded) => done.push(json!({
                "file": image.path,
                "url": uploaded.url,
                "markdown": uploaded.markdown,
            })),
            Err(error) => {
                if !done.is_empty() {
                    output::print(Output::Json(Value::Array(done)))?;
                }
                // The cause stays first: `explain` names a missing or logged-out gh/glab from it.
                return fail(format!("{}\nupload of {} failed", error.0, image.name));
            }
        }
    }
    Ok(Output::Json(Value::Array(done)))
}
