# Edo Storage Component - Detailed Design

## 1. Overview

The Storage component is one of Edo's four core architectural pillars, responsible for managing the caching and persistence of all artifacts in the build system. This component provides a unified interface for storing, retrieving, and managing artifacts regardless of their underlying storage mechanism.

## 2. Core Responsibilities

The Storage component is responsible for:

1. **Artifact Storage**: Persisting artifacts in a content-addressable manner
2. **Artifact Retrieval**: Retrieving artifacts by their unique identifiers
3. **Cache Management**: Handling both local and remote artifact caches
4. **Artifact Organization**: Maintaining a structured representation of artifacts
5. **Integrity Verification**: Validating artifact integrity through hashing
6. **Storage Backend Abstraction**: Providing a consistent interface across different storage backends

## 3. Component Architecture

### 3.1 Key Abstractions

#### 3.1.1 Artifact

An Artifact represents a single unit of data within the build system, implemented as:

```rust
#[derive(Serialize, Deserialize, Debug, Clone, Builder)]
#[builder(setter(into))]
pub struct Artifact {
    #[builder(setter(into), default)]
    media_type: MediaType,
    config: Config,
    #[builder(setter(into), default)]
    layers: Vec<Layer>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Builder)]
#[builder(setter(into))]
pub struct Config {
    id: Id,
    #[builder(setter(into), default)]
    provides: BTreeSet<String>,
    #[builder(setter(into), default)]
    requires: Requires,
    #[builder(setter(into), default)]
    metadata: Metadata,
}

#[derive(Serialize, Deserialize, Debug, Clone, Builder)]
#[builder(setter(into))]
pub struct Layer {
    media_type: MediaType,
    digest: LayerDigest,
    size: usize,
    #[builder(setter(into), default)]
    platform: Option<Platform>,
}
```

Key characteristics include:

- **Unique Identifier**: Complex identifier with name, optional package name, version, architecture, and content-based Blake3 hash
- **OCI Compatibility**: Structured as an OCI-compatible artifact
- **Config**: Contains core metadata including:
  - **ID**: The unique identifier for the artifact
  - **Provides**: Capabilities provided by this artifact
  - **Requires**: Dependencies on other artifacts with version requirements
  - **Metadata**: Custom metadata for the artifact
- **Layer Structure**: One or more content layers, each with a media type, digest, and size
- **Media Types**: Support for various content types (Manifest, File, Tar, OCI, Image, Zip, Custom)
- **Compression**: Optional compression for content layers (Zstd, Gzip, Bzip2, Lz4, Xz)
- **Platform**: Optional platform-specific information

##### Media Type System

Edo implements a rich media type system for artifact content:

```rust
/// Denotes the use of any compression algorithm on a layer
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum Compression {
    #[serde(rename = ".zst")]
    Zstd,
    #[serde(rename = ".gz", alias = ".gzip", alias = ".gzip2")]
    Gzip,
    #[serde(rename = ".bz2", alias = ".bzip2", alias = ".bzip")]
    Bzip2,
    #[serde(rename = ".lz4", alias = ".lzma")]
    Lz,
    #[serde(rename = ".xz")]
    Xz,
    #[serde(other, rename = "")]
    None,
}

/// Denotes the content of a layer
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum MediaType {
    #[default]
    Manifest,
    File(Compression),
    Tar(Compression),
    Oci(Compression),
    Image(Compression),
    Zip(Compression),
    Custom(String, Compression),
}
```

The media type system supports:

1. **Standard Types**: Pre-defined media types for common content:
   - `Manifest`: For artifact manifests (uncompressed)
   - `File`: For individual files (with optional compression)
   - `Tar`: For tar archives (with optional compression)
   - `Oci`: For OCI container images (with optional compression)
   - `Image`: For generic images (with optional compression)
   - `Zip`: For zip archives (with optional compression)

2. **Custom Types**: User-defined media types with optional compression

3. **Compression Detection**: Automatic detection of compression formats using regex patterns:
   ```rust
   pub fn detect(input: &str) -> StorageResult<(String, Compression)> {
       // Detects compression format from file extensions
       // Returns base name and detected compression format
   }
   ```

4. **String Representation**: Follows the pattern:
   ```
   vnd.edo.artifact.<version>.<type>[.<compression>]
   ```
   For example:
   - `vnd.edo.artifact.v1.manifest` (artifact manifest)
   - `vnd.edo.artifact.v1.tar.gz` (gzip-compressed tar archive)
   - `vnd.edo.artifact.v1.custom-type.zst` (zstd-compressed custom type)

#### 3.1.2 Artifact ID

The Id structure represents a unique identifier for artifacts in the system:

```rust
/// Represents the unique id for an artifact, it optionally can contain a
/// secondary name called the package name, along with an optional version.
/// All ids contain a blake3 digest
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Builder)]
pub struct Id {
    name: Name,               // Required artifact name
    package: Option<Name>,    // Optional package name
    version: Option<Version>, // Optional semantic version
    arch: Option<String>,     // Optional architecture
    digest: String,          // Required blake3 digest
}
```

Key characteristics:

- **Name**: Sanitized string without special characters (@, :, ., -, /)
- **Package** (Optional): Secondary identifier for grouping related artifacts
- **Version** (Optional): Semantic version number
- **Architecture** (Optional): Target architecture identifier
- **Digest**: Blake3 hash for content addressing and uniqueness

String representation format:
- Basic: `name-digest`
- With package: `package+name-digest`
- With version: `name-version-digest`
- With architecture: `name.arch-digest`
- Full: `package+name-version.arch-digest`

#### 3.1.3 ArtifactManifest

The ArtifactManifest contains metadata about an artifact, including:

- **Origin Information**: How the artifact was created (source or transform)
- **Dependency References**: Links to dependent artifacts
- **Content Descriptors**: References to content layers
- **Creation Timestamp**: When the artifact was created
- **Tool Metadata**: Information about the tools used to create the artifact

#### 3.1.3 Storage

The main Storage component manages multiple caches:

- **Local Cache**: Required cache for storing artifacts needed by transforms locally
- **Source Caches**: Multiple named caches for fetching remote artifacts, ordered by priority
- **Build Cache**: Optional cache for reusing pre-built artifacts
- **Output Cache**: Optional cache for publishing transform results

#### 3.1.4 StorageBackend

A StorageBackend is an abstraction for the actual storage mechanism, defined by traits that can be implemented by plugins:

- **Local File System**: Default implementation for local storage
- **Remote Storage**: Implementations for S3, HTTP, etc.
- **Custom Backends**: Plugin-defined storage backends

### 3.2 Component Structure

```mermaid
classDiagram
    class Storage {
        +Arc~RwLock~Inner~~ inner
        +init(backend: &Backend) async StorageResult~Storage~
        +add_source_cache(name: &str, cache: &Backend) async
        +add_source_cache_front(name: &str, cache: &Backend) async
        +remove_source_cache(name: &str) async Option~Backend~
        +set_build(cache: &Backend) async
        +set_output(cache: &Backend) async
        +safe_open(id: &Id) async StorageResult~Artifact~
        +safe_read(layer: &Layer) async StorageResult~Reader~
        +safe_start_layer() async StorageResult~Writer~
        +safe_finish_layer(media_type: &MediaType, platform: Option~Platform~, writer: &Writer) async StorageResult~Layer~
        +safe_save(artifact: &Artifact) async StorageResult~()~
        +fetch_source(id: &Id) async StorageResult~Option~Artifact~~
        +find_source(id: &Id) async StorageResult~Option~(Artifact, Backend)~~
        +find_build(id: &Id, sync: bool) async StorageResult~Option~Artifact~~
        +upload_build(id: &Id) async StorageResult~()~
        +prune_local(id: &Id) async StorageResult~()~
        +prune_local_all() async StorageResult~()~
    }

    class Inner {
        -local: Backend
        -source: IndexMap~String, Backend~
        -build: Option~Backend~
        -output: Option~Backend~
        -init(backend: Backend) async StorageResult~Self~
        -add_source_cache(name: &str, cache: &Backend)
        -add_source_cache_front(name: &str, cache: &Backend)
        -remove_source_cache(name: &str) Option~Backend~
        -set_build_cache(cache: &Backend)
        -set_output_cache(cache: &Backend)
        -safe_open(id: &Id) async StorageResult~Artifact~
        -safe_read(layer: &Layer) async StorageResult~Reader~
        -safe_start_layer() async StorageResult~Writer~
        -safe_finish_layer(media_type: &MediaType, platform: Option~Platform~, writer: &Writer) async StorageResult~Layer~
        -safe_save(artifact: &Artifact) async StorageResult~()~
        -download(artifact: &Artifact, backend: &Backend) async StorageResult~()~
        -upload(artifact: &Artifact, backend: &Backend) async StorageResult~()~
        -fetch_source(id: &Id) async StorageResult~Option~Artifact~~
        -find_source(id: &Id) async StorageResult~Option~(Artifact, Backend)~~
        -find_build(id: &Id, sync: bool) async StorageResult~Option~Artifact~~
        -upload_build(id: &Id) async StorageResult~()~
        -upload_output(id: &Id) async StorageResult~()~
        -prune_local(id: &Id) async StorageResult~()~
        -prune_local_all() async StorageResult~()~
    }

    class Backend {
        <<interface>>
        +list() -> StorageResult~BTreeSet~Id~~
        +has(id: &Id) -> StorageResult~bool~
        +open(id: &Id) -> StorageResult~Artifact~
        +save(artifact: &Artifact) -> StorageResult~()~
        +del(id: &Id) -> StorageResult~()~
        +copy(from: &Id, to: &Id) -> StorageResult~()~
        +prune(id: &Id) -> StorageResult~()~
        +prune_all() -> StorageResult~()~
        +read(layer: &Layer) -> StorageResult~Reader~
        +start_layer() -> StorageResult~Writer~
        +finish_layer(media_type: &MediaType, platform: Option~Platform~, writer: &Writer) -> StorageResult~Layer~
    }

    class Name {
        +String value
        +parse(value: &str) -> Self
        +from(&str) -> Self
        +from(String) -> Self
        +to_string() -> String
    }

    class Id {
        +name: Name
        +package: Option~Name~
        +version: Option~Version~
        +arch: Option~String~
        +digest: String
        +name() -> String
        +package() -> Option~String~
        +digest() -> &String
        +arch() -> Option~String~
        +version() -> Option~Version~
        +set_digest(&mut self, digest: &str)
        +set_version(&mut self, version: &Version)
        +clear_version(&mut self)
        +prefix() -> String
    }

    class Compression {
        <<enumeration>>
        Zstd
        Gzip
        Bzip2
        Lz
        Xz
        None
        +detect(input: &str) -> StorageResult~(String, Compression)~
    }

    class MediaType {
        <<enumeration>>
        Manifest
        File(Compression)
        Tar(Compression)
        Oci(Compression)
        Image(Compression)
        Zip(Compression)
        Custom(String, Compression)
        +is_compressed() -> bool
        +set_compression(&mut self, compression: Compression)
    }

    class LayerDigest {
        +String value
        +digest() -> String
    }

    class Layer {
        +media_type: MediaType
        +digest: LayerDigest
        +size: usize
        +platform: Option~Platform~
        +media_type() -> &MediaType
        +digest() -> &LayerDigest
        +size() -> &usize
        +platform() -> &Option~Platform~
    }

    class Config {
        +id: Id
        +provides: BTreeSet~String~
        +requires: Requires
        +metadata: Metadata
        +id() -> &Id
        +metadata() -> &Metadata
        +requires() -> &Requires
        +provides() -> &BTreeSet~String~
    }

    class Artifact {
        +media_type: MediaType
        +config: Config
        +layers: Vec~Layer~
        +config() -> &Config
        +media_type() -> &MediaType
        +layers() -> &Vec~Layer~
    }

    class LocalStorageBackend {
        +rootPath: PathBuf
        +blobsPath: PathBuf
        +catalogPath: PathBuf
    }

    class RemoteStorageBackend {
        <<interface>>
        +endpoint: Url
        +credentials: Credentials
    }

    class StorageError {
        <<enumeration>>
        Join{source: JoinError}
        Child{children: Vec~StorageError~}
        Io{source: std::io::Error}
        NotFound{id: Id}
        LayerNotFound{digest: String}
        InvalidArtifact{details: String}
        PathAccess{path: PathBuf, details: String}
        BackendFailure{details: String}
    }

    class Reader {
        +read(buf: &mut [u8]) -> StorageResult~usize~
        +close() -> StorageResult~()~
    }

    class Writer {
        +write(buf: &[u8]) -> StorageResult~usize~
        +close() -> StorageResult~()~
    }

    Storage *-- Inner : contains
    Inner "1" *-- "1" Backend : local cache
    Inner "1" *-- "0..*" Backend : source caches
    Inner "1" *-- "0..1" Backend : build cache
    Inner "1" *-- "0..1" Backend : output cache

    Backend <|.. LocalStorageBackend : implements
    Backend <|.. RemoteStorageBackend : implements

    Artifact *-- Config : contains
    Artifact *-- "1..*" Layer : contains
    Artifact -- MediaType : uses

    Config *-- Id : contains
    Config -- "0..*" BTreeSet~String~ : provides
    Config -- Requires : dependencies

    Layer *-- LayerDigest : contains
    Layer *-- MediaType : uses
    Layer -- "0..1" Platform : optional

    Id *-- Name : contains
    Id -- "0..1" Name : optional package
    Id -- "0..1" Version : optional version

    MediaType -- Compression : uses

    Backend -- Reader : produces
    Backend -- Writer : produces

    StorageResult -- StorageError : error type
```

## 4. Key Interfaces

### 4.1 Storage Interface

```rust
/// Main Storage component that manages multiple cache backends
#[derive(Clone)]
pub struct Storage {
    // We protect the implementation inside an arced rwlock as we do
    // operate with same storage over multiple tokio routines/threads
    inner: Arc<RwLock<Inner>>,
}

impl Storage {
    /// Initialize a new Storage instance with a local backend
    pub async fn init(backend: &Backend) -> StorageResult<Self>;

    /// Add a new source cache to the end of the priority list
    pub async fn add_source_cache(&self, name: &str, cache: &Backend);

    /// Add a new source cache to the front of the priority list
    pub async fn add_source_cache_front(&self, name: &str, cache: &Backend);

    /// Remove a source cache
    pub async fn remove_source_cache(&self, name: &str) -> Option<Backend>;

    /// Set the build cache
    pub async fn set_build(&self, cache: &Backend);

    /// Set the output cache
    pub async fn set_output(&self, cache: &Backend);

    /// Open an artifact stored in the local cache
    /// **safe operation** This operation is safe to call in a networkless environment or in the
    /// build stages as it will make no network calls
    pub async fn safe_open(&self, id: &Id) -> StorageResult<Artifact>;

    /// Open a layer stored in the local cache
    /// **safe operation** This operation is safe to call in a networkless environment or in the
    /// build stages as it will make no network calls
    pub async fn safe_read(&self, layer: &Layer) -> StorageResult<Reader>;

    /// All new artifacts should be created first in the local cache with safe_create
    pub async fn safe_start_layer(&self) -> StorageResult<Writer>;

    /// Finish writing of a local layer
    pub async fn safe_finish_layer(
        &self,
        media_type: &MediaType,
        platform: Option<Platform>,
        writer: &Writer,
    ) -> StorageResult<Layer>;

    /// Finish creation of a new local artifact
    pub async fn safe_save(&self, artifact: &Artifact) -> StorageResult<()>;

    /// Fetch a source to local cache and open it for any uses
    /// **unsafe operation** This operation is unsafe because it could reach out to a networked back source
    /// cache.
    pub async fn fetch_source(&self, id: &Id) -> StorageResult<Option<Artifact>>;

    /// Find a source in the source caches
    /// **unsafe operation** This operation is unsafe because it could reach out to a networked back source
    /// cache.
    pub async fn find_source(&self, id: &Id) -> StorageResult<Option<(Artifact, Backend)>>;

    /// Check for a build artifact
    /// **unsafe operation** This operation is unsafe because it could reach out to a remotely backed
    /// build cache.
    pub async fn find_build(&self, id: &Id, sync: bool) -> StorageResult<Option<Artifact>>;

    /// Upload a build artifact if we have a build cache
    pub async fn upload_build(&self, id: &Id) -> StorageResult<()>;

    /// Prune the local cache of rerun artifacts
    pub async fn prune_local(&self, id: &Id) -> StorageResult<()>;

    /// Prune all duplicate artifacts from the local cache
    pub async fn prune_local_all(&self) -> StorageResult<()>;
}
```

### 4.2 StorageBackend Interface

```rust
/// Interface for storage backend implementations
pub trait Backend {
    /// List all the ids stored in this backend
    fn list(&self) -> StorageResult<BTreeSet<Id>>;

    /// Check if the backend has an artifact by this name
    fn has(&self, id: &Id) -> StorageResult<bool>;

    /// Open an artifact's manifest into memory
    fn open(&self, id: &Id) -> StorageResult<Artifact>;

    /// Save an artifact's manifest
    fn save(&self, artifact: &Artifact) -> StorageResult<()>;

    /// Delete this artifact and all its layers from the backend
    fn del(&self, id: &Id) -> StorageResult<()>;

    /// Copy an artifact to a new id
    fn copy(&self, from: &Id, to: &Id) -> StorageResult<()>;

    /// Prune any other artifact with a different digest from the backend
    fn prune(&self, id: &Id) -> StorageResult<()>;

    /// Prune any duplicate artifacts from the backend
    fn prune_all(&self) -> StorageResult<()>;

    /// Open a reader to a layer
    fn read(&self, layer: &Layer) -> StorageResult<Reader>;

    /// Creates a new layer writer
    fn start_layer(&self) -> StorageResult<Writer>;

    /// Saves and adds a layer to an artifact
    fn finish_layer(&self, media_type: &MediaType, platform: Option<Platform>, writer: &Writer) -> StorageResult<Layer>;
}
```

### 4.3 WebAssembly Plugin Interface (WIT)

```wit
// storage-backend.wit
package edo:storage;

interface storage-backend {
    // Error type for storage operations
    enum storage-error {
        not-found,
        permission-denied,
        corrupt-artifact,
        backend-error,
        io-error,
    }

    // Artifact identifier
    record id {
        name: string,
        digest: list<u8>,
    }

    // Media type
    type media-type = string;

    // Platform information
    record platform {
        os: string,
        arch: string,
        variant: option<string>,
    }

    // Layer information
    record layer {
        digest: list<u8>,
        media-type: media-type,
        size: u64,
    }

    // Reader handle for layer data
    resource reader {
        read: func(buf: list<u8>) -> result<u64, storage-error>;
        close: func() -> result<_, storage-error>;
    }

    // Writer handle for layer data
    resource writer {
        write: func(buf: list<u8>) -> result<u64, storage-error>;
        close: func() -> result<_, storage-error>;
    }

    // Artifact
    record artifact {
        // Artifact details implemented opaquely in the backend
    }

    // Type alias for storage results
    type storage-result<T> = result<T, storage-error>;

    // List all the ids stored in this backend
    list: func() -> storage-result<list<id>>;

    // Check if the backend has an artifact by this id
    has: func(id: id) -> storage-result<bool>;

    // Open an artifact's manifest into memory
    open: func(id: id) -> storage-result<artifact>;

    // Save an artifact's manifest
    save: func(artifact: artifact) -> storage-result<_>;

    // Delete this artifact and all its layers from the backend
    del: func(id: id) -> storage-result<_>;

    // Copy an artifact to a new id
    copy: func(from: id, to: id) -> storage-result<_>;

    // Prune any other artifact with a different digest from the backend
    prune: func(id: id) -> storage-result<_>;

    // Prune any duplicate artifacts from the backend
    prune-all: func() -> storage-result<_>;

    // Open a reader to a layer
    read: func(layer: layer) -> storage-result<reader>;

    // Creates a new layer writer
    start-layer: func() -> storage-result<writer>;

    // Saves and adds a layer to an artifact
    finish-layer: func(media-type: media-type, platform: option<platform>, writer: writer) -> storage-result<layer>;
}
```

## 5. Storage Backend Implementations

### 5.1 Local Storage Backend

The default storage backend uses the local filesystem with this structure:

```
${EDO_CACHE_DIR}/
├── blobs/
│   └── blake3/
│       ├── <digest1>
│       ├── <digest2>
│       └── ...
└── catalog.json
```

Where:
- `blobs/blake3/` contains the actual content of all artifacts, named by their digest
- `catalog.json` contains the mapping between artifact names and their manifests

Implementation details:
- Content deduplication through blob storage
- Atomic write operations to prevent corruption
- Fast lookup via catalog indexing
- Blake3 verification on read/write

### 5.2 Remote Storage Backend

Remote storage backends share a common interface but can be implemented for various services:

- **S3-compatible**: For AWS S3, MinIO, etc.
- **HTTP/HTTPS**: For simple HTTP-based artifact servers
- **Custom protocols**: Via plugin implementations

Common features:
- Authentication handling
- Network resilience (retries, timeouts)
- Content validation
- Parallel download/upload

## 6. Artifact Cache Management

### 6.1 Cache Structure

Edo's artifact cache system operates with multiple specialized caches:

1. **Local Cache**: Required for all operations, stores artifacts needed during build
2. **Source Caches**: Multiple priority-ordered caches for fetching remote artifacts
3. **Build Cache**: Optional cache for reusing pre-built artifacts
4. **Output Cache**: Optional cache for publishing transform results

This multi-cache architecture optimizes for:

- **Lookup Speed**: Quick determination if an artifact exists
- **Storage Efficiency**: Deduplication of content through content-addressing
- **Integrity**: Validation of artifact contents through hashing
- **Network Efficiency**: Minimizing network operations during builds

### 6.2 Cache Synchronization

Artifacts are synchronized between caches through well-defined operations:

#### 6.2.1 Download Operation

The `download` operation copies an artifact from a remote cache to the local cache:

```rust
async fn download(&self, artifact: &Artifact, backend: &Backend) -> StorageResult<()> {
    // In parallel, copy all layers from remote to local
    let mut handles = Vec::new();
    for layer in artifact.layers() {
        let backend = backend.clone();
        let local = self.local.clone();
        let layer = layer.clone();
        let digest = layer.digest().digest();
        handles.push(tokio::spawn(async move {
            // Read from remote backend
            let mut reader = backend.read(&layer).await?;
            // Write to local backend
            let mut writer = local.start_layer().await?;
            tokio::io::copy(&mut reader, &mut writer).await.context(error::IoSnafu)?;
            // Finalize the layer in local cache
            local.finish_layer(layer.media_type(), layer.platform().clone(), &writer).await?;
            Ok(())
        }));
    }
    // Wait for all layer copies to complete
    wait(handles).await?;
    // Save the artifact manifest in local cache
    self.local.save(artifact).await?;
    Ok(())
}
```

#### 6.2.2 Upload Operation

The `upload` operation copies an artifact from the local cache to a remote cache:

```rust
async fn upload(&self, artifact: &Artifact, backend: &Backend) -> StorageResult<()> {
    // In parallel, copy all layers from local to remote
    let mut handles = Vec::new();
    for layer in artifact.layers() {
        let backend = backend.clone();
        let local = self.local.clone();
        let layer = layer.clone();
        let digest = layer.digest().digest();
        handles.push(tokio::spawn(async move {
            // Read from local backend
            let mut reader = local.read(&layer).await?;
            // Write to remote backend
            let mut writer = backend.start_layer().await?;
            tokio::io::copy(&mut reader, &mut writer).await.context(error::IoSnafu)?;
            // Finalize the layer in remote cache
            backend.finish_layer(layer.media_type(), layer.platform().clone(), &writer).await?;
            Ok(())
        }));
    }
    // Wait for all layer copies to complete
    wait(handles).await?;
    // Save the artifact manifest in remote cache
    backend.save(artifact).await?;
    Ok(())
}
```

### 6.3 Cache Operations

The storage component supports these primary operations:

1. **Safe Operations**: Local-only operations that don't require network access
   - `safe_open`: Open an artifact from local cache
   - `safe_read`: Read a layer from local cache
   - `safe_start_layer`: Begin creating a new layer
   - `safe_finish_layer`: Complete and store a layer
   - `safe_save`: Save an artifact manifest locally

2. **Source Operations**: Operations that may reach out to source caches
   - `fetch_source`: Find an artifact in source caches and synchronize to local if found
   - `find_source`: Locate an artifact in source caches without synchronizing

3. **Build Operations**: Operations that interact with the build cache
   - `find_build`: Find an artifact in the build cache and optionally synchronize
   - `upload_build`: Upload a local artifact to the build cache

4. **Output Operations**: Operations for publishing results
   - `upload_output`: Upload a local artifact to the output cache

### 6.4 Cache Invalidation

Cache invalidation is primarily user-driven through:

1. **Prune Command**: Remove artifacts based on criteria
   - `prune_local`: Remove specific artifact versions
   - `prune_local_all`: Remove all duplicate artifacts

2. **Cache Management**: Add/remove caches or change their priority
   - `add_source_cache`: Add a source cache to the priority list
   - `remove_source_cache`: Remove a source cache

## 7. OCI Artifact Structure

Edo stores artifacts in an OCI-compatible format:

```json
{
  "id": {
    "name": "example-artifact",
    "digest": "blake3:ab3484..."
  },
  "manifest": {
    "schemaVersion": 2,
    "mediaType": "application/vnd.edo.artifact.manifest.v1+json",
    "config": {
      "mediaType": "application/vnd.edo.artifact.config.v1+json",
      "digest": "blake3:fe4521...",
      "size": 1024
    },
    "layers": [
      {
        "mediaType": "application/vnd.edo.layer.v1.tar+gzip",
        "digest": "blake3:c29a12...",
        "size": 10240
      }
    ],
    "annotations": {
      "edo.origin": "source:git",
      "edo.created": "2023-04-15T12:00:00Z"
    }
  }
}
```

## 8. Error Handling

The Storage component uses a comprehensive error handling strategy through the `StorageResult` type:

```rust
/// Type alias for storage operation results
pub type StorageResult<T> = Result<T, StorageError>;

/// Module containing storage error types and utilities
pub mod error {
    use super::*;
    use snafu::Snafu;
    use std::path::PathBuf;
    use tokio::task::JoinError;

    /// Storage error types
    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(crate)))]
    pub enum StorageError {
        /// Failed to join multiple tasks
        #[snafu(display("Failed to join storage tasks: {}", source))]
        Join { source: JoinError },

        /// Multiple child failures occurred
        #[snafu(display("Multiple storage operations failed"))]
        Child { children: Vec<StorageError> },

        /// Error during IO operation
        #[snafu(display("IO error: {}", source))]
        Io { source: std::io::Error },

        /// Artifact not found in storage
        #[snafu(display("Artifact {} not found", id))]
        NotFound { id: Id },

        /// Layer not found in storage
        #[snafu(display("Layer {} not found", digest))]
        LayerNotFound { digest: String },

        /// Invalid artifact structure
        #[snafu(display("Invalid artifact structure: {}", details))]
        InvalidArtifact { details: String },

        /// Failure accessing filesystem path
        #[snafu(display("Path access failure at {}: {}", path.display(), details))]
        PathAccess { path: PathBuf, details: String },

        /// Error in backend operation
        #[snafu(display("Backend operation failed: {}", details))]
        BackendFailure { details: String },
    }
}

// Wait for multiple asynchronous storage operations, handling errors appropriately
async fn wait<I, R>(handles: I) -> StorageResult<Vec<R>>
where
    R: Clone,
    I: IntoIterator,
    I::Item: Future<Output = std::result::Result<StorageResult<R>, JoinError>>,
{
    let result = try_join_all(handles).await;
    let mut success = Vec::new();
    let mut failures = Vec::new();
    for entry in result.context(error::JoinSnafu)? {
        match entry {
            Ok(result) => success.push(result),
            Err(e) => failures.push(e),
        }
    }
    if !failures.is_empty() {
        error::ChildSnafu { children: failures }.fail()
    } else {
        Ok(success)
    }
}
```

## 9. Implementation Considerations

### 9.1 Performance Optimizations

- **Parallel Operations**: Concurrent storage/retrieval where possible
- **Lazy Loading**: Load artifact contents only when needed
- **Partial Retrieval**: Support for retrieving specific layers
- **Index Caching**: In-memory caching of artifact indices

### 9.2 Concurrency

- **Thread-safe Interface**: All public methods are thread-safe
- **Lock Avoidance**: Minimize contention through lock-free designs where possible
- **Atomic Operations**: Ensure consistency during concurrent operations

### 9.3 Resilience

- **Failure Recovery**: Ability to recover from interrupted operations
- **Verification**: Regular integrity checks of stored artifacts
- **Repair Mechanisms**: Tools to repair damaged caches

## 10. Testing Strategy

Testing for the Storage component will focus on:

1. **Unit Tests**: Verify individual component behavior
2. **Integration Tests**: Test interactions between storage and other components
3. **Backend Tests**: Validate backend implementations
4. **Performance Tests**: Benchmark storage operations
5. **Fuzz Tests**: Test resilience against corrupted inputs

## 11. Future Enhancements

1. **Distributed Cache**: Support for distributed artifact caching
2. **Tiered Storage**: Hierarchical storage management
3. **Compression Options**: Pluggable compression algorithms
4. **Advanced Deduplication**: Sub-artifact deduplication techniques
5. **Encryption**: Support for encrypted artifact storage
