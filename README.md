# Edo

A next-generation build tool with flexible environment control and reproducible builds.

## Overview

Edo is a modern build tool implemented in Rust that addresses critical limitations in existing build systems like Bazel, Buck2, and BuildStream. The primary innovation of Edo is its flexible approach to build environments while maintaining reproducibility.

### Key Differentiators

- **Environment Control**: Precise control over build environments without being tied to specific technologies
- **Extensibility**: WebAssembly-based plugin system that allows customization of any component
- **Artifact-Centric Design**: OCI-compatible artifact model for consistent handling across storage backends
- **Build Reproducibility**: Deterministic builds with comprehensive input tracking
- **Cross-Platform Support**: Runs on Linux, macOS, and Windows with consistent behavior

## Features

- **Flexible Build Environments**: Choose between local system, Docker containers, or custom environments
- **Powerful Caching**: Multi-level caching with content-addressed storage
- **Dependency Resolution**: Sophisticated dependency management with version constraint satisfaction
- **Parallel Execution**: Optimized parallel build execution based on dependency graph
- **WebAssembly Plugins**: Extend any component with plugins written in any language that compiles to WebAssembly
- **Starlark Configuration**: Familiar, deterministic configuration language with a focus on readability

## Installation

### Prerequisites

- Rust 1.86 or newer
- For container-based builds: Docker, Podman, or Finch

### From Source

```bash
# Clone the repository
git clone https://github.com/awslabs/edo.git
cd edo

# Build the project
cargo build --package edo --release

# Add to your PATH
export PATH="$PATH:$(pwd)/target/release"
```

### Cargo Install

```bash
cargo install --git https://github.com/awslabs/edo.git --package edo
```

### Prebuilt Binaries

Prebuilt binaries for major platforms are available on the [releases page](https://github.com/awslabs/edo/releases).

## Quick Start

### Creating a Basic Project

_TODO: Create this section_

## Architecture

Edo is built on four core components that work together to provide a flexible, reproducible build experience:

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   Storage   │◄────┤    Source   │◄────┤ Environment │◄────┤  Transform  │
└─────────────┘     └─────────────┘     └─────────────┘     └─────────────┘
        │                  │                  │                   │
        └──────────────────┴──────────────────┴───────────────────┘
                                    │
                            ┌───────▼───────┐
                            │ Plugin System │
                            └───────────────┘
```

### Storage Component

Manages the caching and persistence of all artifacts in the build system. Provides a unified interface for storing, retrieving, and managing artifacts regardless of their underlying storage mechanism.

### Source Component

Handles the acquisition of external code and artifacts for the build system. Ensures reproducible builds by providing a consistent, verifiable way to fetch dependencies from various origins.

### Environment Component

Provides flexible, pluggable control over where and how builds execute. Enables precise environment configuration while maintaining reproducibility across different execution contexts.

### Transform Component

Processes input artifacts into output artifacts according to defined build operations. Manages build dependencies and execution order through a directed acyclic graph (DAG).

### Plugin System

Extends any component using WebAssembly-based plugins. Allows customization of the build system without modifying the core codebase.

## Use Cases

Edo is particularly well-suited for:

- **OS Builders**: Projects like Bottlerocket that need fine-grained control over build environments
- **Application Packaging**: Creating portable applications for platforms like Flatpak and Snap
- **Cross-Platform Development**: Ensuring consistent builds across different operating systems
- **Monorepos**: Managing complex dependencies in large codebases
- **Binary Compatibility**: Creating binaries with specific GLIBC version compatibility

## Configuration Reference

Edo uses Starlark for build configuration. Here's an example showcasing common patterns:

__TODO: Fill out this section__

## Plugin Development

__TODO: Fill out this section__

## Contributing

We welcome contributions to Edo! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for details on how to get started.

## License

Edo is licensed under the Apache License 2.0 or MIT - see the [LICENSE-APACHE](LICENSE-APACHE) or [LICENSE_MIT](LICENSE-MIT) file for details.

## Project Status

Edo is currently in active development. We're working toward our first stable release and welcome early adopters and contributors to help shape the project.

## Acknowledgments

Edo builds upon ideas and concepts from many excellent build tools including:
- Bazel
- Buck2
- BuildStream
- Pants

We're grateful to these projects for advancing the state of build tools and inspiring Edo's development.
