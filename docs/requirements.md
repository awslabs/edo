# Edo Build Tool Requirements Specification

## Executive Summary

Edo is a next-generation build tool implemented in Rust that addresses key limitations in existing build tools like Bazel, Buck2, and BuildStream. It provides significantly more control and flexibility around build environments while maintaining reproducibility, particularly focused on enabling software developers to build applications for platforms like Flatpak and Snap, as well as OS builders like Bottlerocket.

## 1. Problem Statement

Modern software development, particularly in monorepo environments, faces significant challenges in controlling build environments while maintaining reproducibility and portability. While contemporary build tools like Buck2, Bazel, and Pants excel at managing complex dependency graphs and providing efficient builds for monorepo architectures, they lack robust capabilities for fine-grained control over the build environment itself.

Existing solutions either provide insufficient environment control or impose restrictive implementation requirements. For instance, Apache BuildStream offers environment specification capabilities but tightly couples this functionality with specific technologies like chroots and bubblewrap namespaces, limiting its flexibility and broader adoption.

A recurring challenge in software development is the need to produce portable binaries with specific compatibility requirements, particularly regarding GLIBC versions. Current build tools do not provide a straightforward, reliable mechanism to control compiler versions and build environments to achieve these compatibility targets. This limitation often results in binaries that are unnecessarily restricted to newer GLIBC versions, reducing their portability across different Linux distributions and versions.

## 2. Goals

### 2.1 Flexible Architecture
- Create a build tool that builds upon the proven concepts of established solutions like Buck2, Bazel, Pants, and BuildStream
- Implement a modular design that allows components to be replaced or customized without affecting the core build logic
- Maintain separation between build environment specification and build execution

### 2.2 Extensible Plugin System
- Provide a robust plugin API that enables runtime loading of custom build logic
- Allow users to implement and integrate their own build strategies without modifying the core system
- Support plugin versioning and compatibility checking to ensure system stability

### 2.3 Reliable Build Reproducibility
- Implement a secure hashing mechanism that uniquely identifies build artifacts based on their inputs
- Ensure deterministic builds by tracking all inputs that could affect build outputs
- Optimize build performance by skipping unnecessary rebuilds when inputs haven't changed
- Maintain a clear audit trail of build inputs and their corresponding outputs

## 3. Architectural Requirements

### 3.1 Core Components

#### 3.1.1 Storage Component
- Must manage caching and persistence of all artifacts in the build system
- Must handle source, intermediate build, and output artifacts
- Must implement a pluggable backend system for different storage solutions
- Must support expandability through plugins implementing a standard interface
- Each artifact must have a unique ID containing name and a blake3 digest
- Must store artifacts in the shape of an OCI Artifact with a custom image config
- Must validate all hashes in both upload and download operations

#### 3.1.2 Source Component
- Must define interfaces for obtaining external code and artifacts
- Must manage retrieval of external dependencies
- Must handle source code fetching from various locations
- Must support remote package downloads and local source references
- Must ensure reproducible source acquisition through content addressing
- Must support the concept of vendors and wants definitions for external dependencies

#### 3.1.3 Environment Component
- Must define the execution context for build operations
- Must provide a flexible interface for implementing different runtime environments
- Must support various execution strategies:
  - Local system execution
  - Container-based isolation (Finch/Docker/Podman)
  - Custom environment implementations
- Must enable precise control over build environment characteristics
- Must respect sandboxing principles, particularly network isolation for non-local environments

#### 3.1.4 Transform Component
- Must process one or more input artifacts into output artifacts
- Must maintain deterministic relationship between inputs and outputs
- Must support dependency tracking and incremental builds
- Must allow for custom transform implementations
- Must participate in a DAG-based dependency resolution system for build optimization

### 3.2 Extensibility Requirements

- Must implement a WebAssembly-based plugin system using the WebAssembly Interface Types (WIT) specification
- Must support plugins written in any language that compiles to WebAssembly
- Must allow plugins to extend any of the core components:
  - Storage Backends
  - Source providers
  - Environment implementations
  - Transform definitions
- Must support runtime loading of plugins to ensure system flexibility
- Must discover plugins through declarations in Starlark build files
- Must fetch plugin binaries according to source instructions in the plugin declaration

### 3.3 Build Configuration Requirements

- Must define build logic using Starlark scripting language
- Must provide a familiar syntax for users of Buck2, Bazel, and Pants
- Must ensure deterministic evaluation of build configurations
- Must support custom rules and macros
- Must maintain clear separation between build definition and execution
- Must use `.edo` file extension for all build configuration files

## 4. Functional Requirements

### 4.1 Build Environment Management

- Must support building in Docker containers
- Must support building in the local environment
- Must allow extension through WebAssembly plugins for custom environments
- Must provide network isolation for container-based builds
- Should allow optional network access for local builds

### 4.2 Artifact Management

- Must support artifact acquisition from external sources via source instructions
- Must support both local and remote caches for artifacts
- Must implement content-addressable storage for artifacts
- Must store artifacts as OCI-compatible artifacts with custom manifests
- Must support artifact invalidation through user-initiated commands (prune/wipe)

### 4.3 Dependency Resolution

- Must support two-fold dependency approaches:
  - Internal dependencies between transform actions (via DAG)
  - External dependencies through vendors and wants declarations
- Must implement dependency resolution for external artifacts via the wants block
- Must generate and utilize resolution lock files (edo.lock.json) for reproducible builds
- Must provide commands to update resolution lock files as needed

### 4.4 Build Execution

- Must efficiently execute builds based on an optimized DAG
- Must skip unnecessary rebuilds when inputs haven't changed
- Must facilitate parallel build execution where dependencies allow
- Must support offloading builds to remote executors (future capability)

### 4.5 Plugin System

- Must support plugin declarations in Starlark build files
- Must resolve plugin sources through the standard source resolution mechanism
- Must load and execute WebAssembly plugins at runtime
- Must enforce interface compliance through the WIT specification

## 5. Non-Functional Requirements

### 5.1 Performance

- Must optimize build performance to minimize overhead beyond actual build operations
- Must aim for improved build times compared to existing tools (e.g., 7 minutes vs 12 minutes for Bottlerocket kernel kit)
- Must efficiently handle large monorepos and extensive dependency graphs
- Must support future capability to offload individual builds to remote executors for scaling

### 5.2 Security

- Must enforce sandboxing for non-local build environments
- Must provide network isolation for container-based builds
- Should implement appropriate security models for the WebAssembly plugin system

### 5.3 Usability

- Must provide clear documentation for all commands and configurations
- Must implement intuitive error handling and messaging
- Must provide useful debug and logging capabilities

### 5.4 Compatibility

- Must remain flexible for integration into any CI/CD environment
- Should consider supporting the Google Remote Execution API (nice-to-have)
- No backward compatibility requirements with existing build tools

## 6. User Experience Requirements

### 6.1 Command Line Interface

- Must provide an intuitive CLI for all operations
- Must support common operations:
  - Building specific targets
  - Updating dependency lock files
  - Pruning or clearing caches
  - Plugin management

### 6.2 Configuration Experience

- Must use Starlark language for build files and configurations
- Must provide clear templates and examples for common configuration patterns
- Must implement helpful error messages for configuration issues

## 7. Future Considerations

### 7.1 Remote Execution

- Should design with the capability to offload builds to remote executors
- Should consider compatibility with existing remote execution APIs

### 7.2 Enhanced Caching Strategies

- Should explore additional caching strategies beyond the initial implementation
- Should consider distributed caching mechanisms for large teams

### 7.3 IDE Integration

- Should consider providing integration points for common IDEs
- Should explore build visualization capabilities

## 8. Implementation Constraints

- Must be implemented in Rust programming language
- Must use WebAssembly for plugin extensibility
- Must use Starlark for build configuration files
