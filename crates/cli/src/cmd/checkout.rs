use std::collections::HashMap;
use std::path::PathBuf;

use crate::Result;
use crate::error;
use async_compression::tokio::bufread::BzDecoder;
use async_compression::tokio::bufread::GzipDecoder;
use async_compression::tokio::bufread::Lz4Decoder;
use async_compression::tokio::bufread::LzmaDecoder;
use async_compression::tokio::bufread::XzDecoder;
use async_compression::tokio::bufread::ZstdDecoder;
use clap::Parser;
use edo::context::Addr;
use edo::storage::Compression;
use edo::storage::MediaType;
use edo::util::Reader;
use edo_core::environment::extract_zip_stream;
use snafu::ResultExt;
use std::pin::Pin;
use tokio::fs::{File, create_dir_all};
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
            &self.addr,
            self.args
                .clone()
                .map(HashMap::from_iter)
                .unwrap_or_default(),
            true,
            false,
        )
        .await?;
        let addr = Addr::parse(self.addr.as_str())?;
        let transform = ctx
            .get_transform(&addr)
            .ok_or_else(|| error::Error::UnknownTransform {
                addr: addr.to_string(),
            })?;
        let handle = ctx.get_handle();
        let id = transform.get_unique_id(&handle).await?;
        let artifact = ctx.storage().safe_open(&id).await?;
        if !self.output.exists() {
            create_dir_all(&self.output).await.context(error::IoSnafu)?;
        }
        for layer in artifact.layers() {
            let raw_reader = ctx.storage().safe_read(layer).await?;
            let media_type = layer.media_type();
            match media_type {
                MediaType::Tar(compression) => {
                    let buffered = BufReader::new(raw_reader);
                    let reader: Pin<Box<dyn tokio::io::AsyncRead>> = match compression {
                        Compression::Bzip2 => Box::pin(BzDecoder::new(buffered)),
                        Compression::Lz4 => Box::pin(Lz4Decoder::new(buffered)),
                        Compression::Lzma => Box::pin(LzmaDecoder::new(buffered)),
                        Compression::Xz => Box::pin(XzDecoder::new(buffered)),
                        Compression::Gzip => Box::pin(GzipDecoder::new(buffered)),
                        Compression::Zstd => Box::pin(ZstdDecoder::new(buffered)),
                        Compression::None => Box::pin(buffered),
                    };
                    let mut archive = Archive::new(reader);
                    archive.unpack(&self.output).await.context(error::IoSnafu)?;
                }
                MediaType::Zip(compression) => {
                    // Outer compression for zip layers is unusual, but honor
                    // it via Reader::with_decompression so the inner
                    // async_zip stream sees a plain zip byte stream.
                    let reader = if matches!(compression, Compression::None) {
                        raw_reader
                    } else {
                        Reader::with_decompression(raw_reader, compression)
                    };
                    extract_zip_stream(&self.output, reader)
                        .await
                        .map_err(|source| error::Error::ZipExtract { source })?;
                }
                MediaType::File(compression) => {
                    let filename = artifact
                        .config()
                        .path_hint_for(layer.digest())
                        .cloned()
                        .unwrap_or_else(|| PathBuf::from(layer.digest().digest()));
                    let dest = self.output.join(filename);
                    if let Some(parent) = dest.parent() {
                        create_dir_all(parent).await.context(error::IoSnafu)?;
                    }
                    let mut reader: Pin<Box<dyn tokio::io::AsyncRead>> =
                        if matches!(compression, Compression::None) {
                            Box::pin(raw_reader)
                        } else {
                            let buffered = BufReader::new(raw_reader);
                            match compression {
                                Compression::Bzip2 => Box::pin(BzDecoder::new(buffered)),
                                Compression::Lz4 => Box::pin(Lz4Decoder::new(buffered)),
                                Compression::Lzma => Box::pin(LzmaDecoder::new(buffered)),
                                Compression::Xz => Box::pin(XzDecoder::new(buffered)),
                                Compression::Gzip => Box::pin(GzipDecoder::new(buffered)),
                                Compression::Zstd => Box::pin(ZstdDecoder::new(buffered)),
                                Compression::None => unreachable!(),
                            }
                        };
                    let mut file = File::create(&dest).await.context(error::IoSnafu)?;
                    tokio::io::copy(&mut reader, &mut file)
                        .await
                        .context(error::IoSnafu)?;
                }
                value => {
                    return Err(error::Error::UnsupportedMediaType {
                        media_type: value.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}
