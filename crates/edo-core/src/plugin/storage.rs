use super::error;
use super::{WasmResult, bindings::edo::plugin::host, error::wasm_ok, host::Host};
use crate::storage::{ConfigBuilder, Layer, Name};
use crate::{
    storage::{Artifact, ArtifactBuilder, Config, Id, IdBuilder, MediaType, Storage},
    util::{Reader, Writer},
};
use edo_oci::models::Platform;
use semver::{Version, VersionReq};
use snafu::ResultExt;
use std::collections::BTreeSet;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wasmtime::component::Resource;

impl host::HostReader for Host {
    async fn read(&mut self, self_: Resource<Reader>, size: u64) -> WasmResult<Vec<u8>> {
        let reader = self.table.get_mut(&self_)?;
        let mut buffer = vec![0; size as usize];
        let result = wasm_ok!(
            async self.table => {
                reader.read(&mut buffer).await.context(error::IoSnafu)?;
                Ok::<Vec<u8>, error::PluginError>(buffer)
            }
        );
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Reader>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostWriter for Host {
    async fn write(&mut self, self_: Resource<Writer>, data: Vec<u8>) -> WasmResult<u64> {
        let writer = self.table.get_mut(&self_)?;
        let result = wasm_ok!(
            async self.table => {
                let amount = writer.write(data.as_slice()).await.context(error::IoSnafu)?;
                Ok::<u64, error::PluginError>(amount as u64)
            }
        );
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Writer>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostId for Host {
    async fn new(
        &mut self,
        name: String,
        digest: String,
        version: Option<String>,
        pkg: Option<String>,
        arch: Option<String>,
    ) -> wasmtime::Result<Resource<Id>> {
        let id = IdBuilder::default()
            .name(name)
            .digest(digest)
            .version(version.map(|x| Version::parse(x.as_str()).unwrap()))
            .package(pkg.map(Name::from))
            .arch(arch)
            .build()?;
        let handle = self.table.push(id)?;
        Ok(handle)
    }

    async fn name(&mut self, self_: Resource<Id>) -> wasmtime::Result<String> {
        let this = self.table.get(&self_)?;
        Ok(this.name())
    }

    async fn digest(&mut self, self_: Resource<Id>) -> wasmtime::Result<String> {
        let this = self.table.get(&self_)?;
        Ok(this.digest().clone())
    }

    async fn set_digest(&mut self, self_: Resource<Id>, digest: String) -> wasmtime::Result<()> {
        let this = self.table.get_mut(&self_)?;
        this.set_digest(digest.as_str());
        Ok(())
    }

    async fn version(&mut self, self_: Resource<Id>) -> wasmtime::Result<Option<String>> {
        let this = self.table.get(&self_)?;
        Ok(this.version().map(|x| x.to_string()))
    }

    async fn set_version(&mut self, self_: Resource<Id>, version: String) -> wasmtime::Result<()> {
        let this = self.table.get_mut(&self_)?;
        let version = Version::parse(version.as_str())?;
        this.set_version(&version);
        Ok(())
    }

    async fn clear_version(&mut self, self_: Resource<Id>) -> wasmtime::Result<()> {
        let this = self.table.get_mut(&self_)?;
        this.clear_version();
        Ok(())
    }

    async fn pkg(&mut self, self_: Resource<Id>) -> wasmtime::Result<Option<String>> {
        let this = self.table.get(&self_)?;
        Ok(this.package())
    }

    async fn arch(&mut self, self_: Resource<Id>) -> wasmtime::Result<Option<String>> {
        let this = self.table.get(&self_)?;
        Ok(this.arch())
    }

    async fn from_string(&mut self, input: String) -> wasmtime::Result<Resource<Id>> {
        let id = Id::from_str(input.as_str())?;
        let handle = self.table.push(id)?;
        Ok(handle)
    }

    async fn drop(&mut self, self_: Resource<Id>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostLayer for Host {
    async fn media_type(&mut self, self_: Resource<Layer>) -> wasmtime::Result<String> {
        let this = self.table.get(&self_)?;
        Ok(this.media_type().to_string())
    }

    async fn digest(&mut self, self_: Resource<Layer>) -> wasmtime::Result<String> {
        let this = self.table.get(&self_)?;
        Ok(this.digest().digest())
    }

    async fn size(&mut self, self_: Resource<Layer>) -> wasmtime::Result<u64> {
        let this = self.table.get(&self_)?;
        Ok(*this.size() as u64)
    }

    async fn platform(&mut self, self_: Resource<Layer>) -> wasmtime::Result<Option<String>> {
        let this = self.table.get(&self_)?;
        Ok(this.platform().clone().map(|x| x.to_string()))
    }

    async fn drop(&mut self, self_: Resource<Layer>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostArtifactConfig for Host {
    async fn new(
        &mut self,
        id: Resource<Id>,
        provides: Vec<String>,
        metadata: Option<String>,
    ) -> wasmtime::Result<Resource<Config>> {
        let id = self.table.get(&id)?;
        let config = ConfigBuilder::default()
            .id(id.clone())
            .provides(BTreeSet::from_iter(provides))
            .metadata(metadata.map(|x| {
                let value: serde_json::Value = serde_json::from_str(x.as_str()).unwrap();
                value
            }))
            .build()?;
        let result = self.table.push(config)?;
        Ok(result)
    }

    async fn id(&mut self, self_: Resource<Config>) -> wasmtime::Result<Resource<Id>> {
        let this = self.table.get(&self_)?;
        let result = self.table.push(this.id().clone())?;
        Ok(result)
    }

    async fn provides(&mut self, self_: Resource<Config>) -> wasmtime::Result<Vec<String>> {
        let this = self.table.get(&self_)?;
        Ok(this.provides().iter().cloned().collect())
    }

    async fn requires(
        &mut self,
        self_: Resource<Config>,
    ) -> wasmtime::Result<Vec<(String, Vec<(String, String)>)>> {
        let this = self.table.get(&self_)?;
        Ok(this
            .requires()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    v.iter()
                        .map(|(k2, v2)| (k2.clone(), v2.to_string()))
                        .collect(),
                )
            })
            .collect())
    }

    async fn add_requirement(
        &mut self,
        self_: Resource<Config>,
        group: String,
        name: String,
        version: String,
    ) -> wasmtime::Result<()> {
        let this = self.table.get_mut(&self_)?;
        this.requires_mut()
            .entry(group)
            .or_default()
            .insert(name, VersionReq::parse(version.as_str())?);
        Ok(())
    }

    async fn drop(&mut self, self_: Resource<Config>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostArtifact for Host {
    async fn new(&mut self, config: Resource<Config>) -> wasmtime::Result<Resource<Artifact>> {
        let config = self.table.get(&config)?;
        let artifact = ArtifactBuilder::default()
            .config(config.clone())
            .media_type(MediaType::Manifest)
            .build()?;
        let result = self.table.push(artifact)?;
        Ok(result)
    }

    async fn config(&mut self, self_: Resource<Artifact>) -> wasmtime::Result<Resource<Config>> {
        let this = self.table.get(&self_)?;
        let config = this.config();
        let result = self.table.push(config.clone())?;
        Ok(result)
    }

    async fn layers(
        &mut self,
        self_: Resource<Artifact>,
    ) -> wasmtime::Result<Vec<Resource<Layer>>> {
        let this = self.table.get_mut(&self_)?;
        let mut layers = Vec::new();
        for layer in this.layers().clone() {
            let handle = self.table.push(layer).unwrap();
            layers.push(handle);
        }
        Ok(layers)
    }

    async fn add_layer(
        &mut self,
        self_: Resource<Artifact>,
        layer: Resource<Layer>,
    ) -> wasmtime::Result<()> {
        let layer = self.table.get(&layer)?.clone();
        let this = self.table.get_mut(&self_)?;
        this.layers_mut().push(layer);
        Ok(())
    }

    async fn drop(&mut self, self_: Resource<Artifact>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}

impl host::HostStorage for Host {
    async fn open(
        &mut self,
        self_: Resource<Storage>,
        id: Resource<Id>,
    ) -> WasmResult<Resource<Artifact>> {
        let this = self.table.get(&self_)?;
        let id = self.table.get(&id)?;
        let result = wasm_ok!(with self.table => this.safe_open(id).await;
        with result {
            let handle = self.table.push(result).unwrap();
            Ok(handle)
        });
        Ok(result)
    }

    async fn read(
        &mut self,
        self_: Resource<Storage>,
        layer: Resource<Layer>,
    ) -> WasmResult<Resource<Reader>> {
        let this = self.table.get(&self_)?;
        let layer = self.table.get(&layer)?;
        let result = wasm_ok! {
            with self.table => this.safe_read(layer).await;
            with result {
                let handle = self.table.push(result).unwrap();
                Ok(handle)
            }
        };
        Ok(result)
    }

    async fn start_layer(&mut self, self_: Resource<Storage>) -> WasmResult<Resource<Writer>> {
        let this = self.table.get(&self_)?;
        let result = wasm_ok! {
            with self.table => this.safe_start_layer().await;
            with result {
                let handle = self.table.push(result).unwrap();
                Ok(handle)
            }
        };
        Ok(result)
    }

    async fn finish_layer(
        &mut self,
        self_: Resource<Storage>,
        media_type: String,
        platform: Option<String>,
        writer: Resource<Writer>,
    ) -> WasmResult<Resource<Layer>> {
        let this = match self.table.get(&self_) {
            Ok(data) => Ok(data),
            Err(e) => Err(e),
        }?;
        let writer = match self.table.get(&writer) {
            Ok(data) => Ok(data),
            Err(e) => Err(e),
        }?;
        let media_type: MediaType = MediaType::from_str(media_type.as_str())?;
        let platform = platform.map(Platform::from);
        let result = wasm_ok! {
            with self.table => this.safe_finish_layer(&media_type, platform, writer).await;
            with result  {
                let handle = self.table.push(result).unwrap();
                Ok(handle)
            }
        };
        Ok(result)
    }

    async fn save(
        &mut self,
        self_: Resource<Storage>,
        artifact: Resource<Artifact>,
    ) -> WasmResult<()> {
        let this = self.table.get(&self_)?;
        let artifact = self.table.get(&artifact)?;
        let result = wasm_ok! {
            with self.table => this.safe_save(artifact).await
        };
        Ok(result)
    }

    async fn drop(&mut self, self_: Resource<Storage>) -> wasmtime::Result<()> {
        self.table.delete(self_)?;
        Ok(())
    }
}
