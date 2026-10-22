use async_trait::async_trait;
use edo::context::{Context, Element, FromElement, Log};
use edo::record;
use edo::source::{SourceImpl, SourceResult};
use edo::storage::{Artifact, Compression, Config, Id, LayerOptions, MediaType, Storage};
use edo::util::cmd_noinput;
use sha2::{Digest, Sha256};
use snafu::ResultExt;
use std::collections::HashMap;
use std::path::PathBuf;
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;
use tracing::Instrument;

/// A source that clones a Git repository at a specific reference.
#[derive(serde::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    url: String,
    #[serde(rename = "ref")]
    reference: String,
    out: Option<PathBuf>,
}

#[async_trait]
impl FromElement for GitSource {
    type Error = error::Error;

    async fn new(element: &Element, _: &Context) -> Result<Self, error::Error> {
        element.get().map_err(|e| e.into())
    }
}

#[async_trait]
impl SourceImpl for GitSource {
    async fn get_unique_id(&self) -> SourceResult<Id> {
        // Separate staging variants for prefix-based cache eviction.
        let mut hasher = Sha256::new();
        hasher.update(self.reference.as_bytes());
        if let Some(out) = &self.out {
            hasher.update(out.as_os_str().as_encoded_bytes());
        }
        let digest = base16::encode_lower(hasher.finalize().as_slice());
        let name = match &self.out {
            Some(out) => format!("{}@{}-{}", self.url, self.reference, out.to_string_lossy()),
            None => format!("{}@{}", self.url, self.reference),
        };
        let id = Id::builder().name(name).digest(digest).build();
        trace!(subsystem = "source", component = "git", id = %id, "calculated id");
        Ok(id)
    }

    async fn fetch(&self, log: &Log, storage: &Storage) -> SourceResult<Artifact> {
        let id = self.get_unique_id().await?;
        let id_s = id.to_string();
        record!(log, "clone", "git clone -b {} {}", self.reference, self.url);
        async move {
            let temp = tempdir().context(error::TempDirectorySnafu)?;
            cmd_noinput(
                ".",
                log,
                "git",
                vec![
                    "clone".into(),
                    "-b".into(),
                    self.reference.clone(),
                    self.url.clone(),
                    temp.path().to_string_lossy().to_string(),
                ],
                &HashMap::new(),
            )
            .context(error::GitSnafu)?;
            let mut artifact = Artifact::builder()
                .media_type(MediaType::Manifest)
                .config(
                    Config::builder()
                        .metadata(serde_json::json!({
                            "repository": self.url,
                            "reference": self.reference
                        }))
                        .id(id.clone())
                        .build(),
                )
                .build();

            // Archive the checkout as a single layer.
            let mut writer = storage.safe_start_layer().await?;
            let mut archive = tokio_tar::Builder::new(writer.clone());
            archive
                .append_dir_all(".", temp.path())
                .await
                .context(error::ArchiveSnafu)?;
            writer.flush().await.context(error::ArchiveSnafu)?;
            archive.finish().await.context(error::ArchiveSnafu)?;
            let layer = storage
                .safe_finish_layer(
                    &writer,
                    &LayerOptions::builder()
                        .media_type(MediaType::Tar(Compression::None))
                        .build(),
                )
                .await?;
            // Associate the staging path with the layer digest.
            if let Some(hint) = self.out.clone() {
                artifact
                    .config_mut()
                    .path_hints_mut()
                    .insert(layer.digest().digest(), hint);
            }
            artifact.layers_mut().push(layer);
            storage.safe_save(&artifact).await?;
            Ok(artifact.clone())
        }
        .instrument(info_span!(
            "source-fetch",
            subsystem = "source",
            component = "git",
            id = %id_s,
            url = %self.url,
            reference = %self.reference
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(out: Option<PathBuf>) -> GitSource {
        GitSource {
            url: "https://example.com/repo".into(),
            reference: "main".into(),
            out,
        }
    }

    #[tokio::test]
    async fn no_output_path_preserves_the_existing_cache_id() {
        let id = source(None).get_unique_id().await.unwrap();
        let expected = Id::builder()
            .name("https://example.com/repo@main")
            .digest(base16::encode_lower(Sha256::digest(b"main").as_slice()))
            .build();
        assert_eq!(id, expected);
    }

    #[tokio::test]
    async fn output_paths_have_distinct_cache_prefixes() {
        let first = source(Some("src/first".into()))
            .get_unique_id()
            .await
            .unwrap();
        let second = source(Some("src/second".into()))
            .get_unique_id()
            .await
            .unwrap();
        assert_eq!(first.name(), "example_com_repo_main_src_first");
        assert_ne!(first.prefix(), second.prefix());
        assert_ne!(first.digest(), second.digest());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_output_paths_have_distinct_digests() {
        use std::os::unix::ffi::OsStringExt;
        let first = source(Some(std::ffi::OsString::from_vec(vec![0xff]).into()))
            .get_unique_id()
            .await
            .unwrap();
        let second = source(Some(std::ffi::OsString::from_vec(vec![0xfe]).into()))
            .get_unique_id()
            .await
            .unwrap();
        let none = source(None).get_unique_id().await.unwrap();
        assert_ne!(first.digest(), second.digest());
        assert_ne!(first.digest(), none.digest());
    }
}

pub mod error {
    use edo::{context::error::ContextError, source::SourceError};
    use snafu::Snafu;

    #[derive(Snafu, Debug)]
    #[snafu(visibility(pub))]
    pub enum Error {
        #[snafu(display("failed to archive git repository: {source}"))]
        Archive { source: std::io::Error },
        #[snafu(display("failed to invoke git cli: {source}"))]
        Git { source: std::io::Error },
        #[snafu(transparent)]
        Project {
            #[snafu(source(from(edo::context::ContextError, Box::new)))]
            source: Box<edo::context::error::ContextError>,
        },
        #[snafu(display("failed to create temporary directory: {source}"))]
        TempDirectory { source: std::io::Error },
    }

    impl From<Error> for SourceError {
        fn from(value: Error) -> Self {
            Self::Implementation {
                source: Box::new(value),
            }
        }
    }

    impl From<Error> for ContextError {
        fn from(value: Error) -> Self {
            Self::Component {
                source: Box::new(value),
            }
        }
    }
}
