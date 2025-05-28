use std::collections::HashMap;
use std::path::PathBuf;

use crate::Result;
use crate::error;
use async_compression::tokio::bufread::BzDecoder;
use async_compression::tokio::bufread::GzipDecoder;
use async_compression::tokio::bufread::LzmaDecoder;
use async_compression::tokio::bufread::XzDecoder;
use async_compression::tokio::bufread::ZstdDecoder;
use clap::Parser;
use edo_core::context::Addr;
use edo_core::storage::Compression;
use edo_core::storage::MediaType;
use snafu::ResultExt;
use std::pin::Pin;
use tokio::fs::create_dir_all;
use tokio::io::BufReader;
use tokio_tar::Archive;

use crate::Args;

#[derive(Parser, Debug, Clone)]
#[clap(version, about = "Checkout an artifact to local directory", long_about = None)]
pub struct Checkout {
    addr: String,
    output: PathBuf,
    #[clap(long = "arg", short = 'a', value_parser = crate::cmd::util::parse_key_val::<String, String>)]
    args: Option<Vec<(String, String)>>,
}

impl Checkout {
    pub async fn run(&self, args: Args) -> Result<()> {
        let ctx = super::create_context(
            &args,
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
        )
        .await?;
        let addr = Addr::parse(self.addr.as_str())?;
        let transform = ctx.get_transform(&addr).unwrap();
        let handle = ctx.get_handle();
        let id = transform.get_unique_id(&handle).await?;
        let artifact = ctx.storage().safe_open(&id).await?;
        if !self.output.exists() {
            create_dir_all(&self.output).await.context(error::IoSnafu)?;
        }
        for layer in artifact.layers() {
            // Do different things depending on the media_type
            let reader = BufReader::new(ctx.storage().safe_read(layer).await?);
            match layer.media_type() {
                MediaType::Tar(compression) => {
                    let reader: Pin<Box<dyn tokio::io::AsyncRead>> = match compression {
                        Compression::Bzip2 => Box::pin(BzDecoder::new(reader)),
                        Compression::Lz => Box::pin(LzmaDecoder::new(reader)),
                        Compression::Xz => Box::pin(XzDecoder::new(reader)),
                        Compression::Gzip => Box::pin(GzipDecoder::new(reader)),
                        Compression::Zstd => Box::pin(ZstdDecoder::new(reader)),
                        _ => Box::pin(reader),
                    };
                    let mut archive = Archive::new(reader);
                    archive.unpack(&self.output).await.context(error::IoSnafu)?;
                }
                value => {
                    tracing::error!(
                        "skipping artifact layer with media_type {value} as we do not know how to extract it"
                    );
                }
            }
        }
        Ok(())
    }
}
