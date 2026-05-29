//! Plugin handler registry.
//!
//! [`Registry`] holds factory closures (one per [`crate::context::Element`]
//! kind string) for each plugin component category — backends, environment
//! farms, sources, transforms, and vendors. Closures are invoked through the
//! generic [`Handler`] trait, which is auto-implemented for any
//! `Fn(Element, Context) -> Future<Output = ContextResult<T>>`.

use crate::{
    context::{Context, Element, error},
    environment::Farm,
    source::{Source, Vendor},
    storage::Backend,
    transform::Transform,
};
use dashmap::DashMap;
use futures::future::BoxFuture;
use snafu::OptionExt;
use std::sync::Arc;

use super::ContextResult;

/// Type-erased async factory for a plugin component.
///
/// Implemented automatically for any closure or function pointer of shape
/// `Fn(Addr, Element, Context) -> Future<Output = ContextResult<T>>`.
pub trait Handler<T>: Send + Sync {
    /// Invokes the handler, returning a boxed future for the constructed value.
    fn call(&self, element: Element, context: Context) -> BoxFuture<'static, ContextResult<T>>;
}

impl<T, E, F> Handler<T> for E
where
    E: Fn(Element, Context) -> F + Send + Sync,
    F: std::future::Future<Output = ContextResult<T>> + 'static + Send,
{
    fn call(&self, element: Element, context: Context) -> BoxFuture<'static, ContextResult<T>> {
        Box::pin(self(element, context))
    }
}

/// Maps element-kind strings to factory closures for each plugin component.
#[derive(Default, Clone)]
pub struct Registry {
    /// Storage backend factories keyed by element kind.
    pub backends: DashMap<String, Arc<dyn Handler<Backend>>>,
    /// Environment farm factories keyed by element kind.
    pub farms: DashMap<String, Arc<dyn Handler<Farm>>>,
    /// Source factories keyed by element kind.
    pub sources: DashMap<String, Arc<dyn Handler<Source>>>,
    /// Transform factories keyed by element kind.
    pub transforms: DashMap<String, Arc<dyn Handler<Transform>>>,
    /// Vendor factories keyed by element kind.
    pub vendors: DashMap<String, Arc<dyn Handler<Vendor>>>,
}

impl Registry {
    /// Registers a storage backend handler under the given kind name.
    pub fn register_backend(&self, name: &str, handler: Arc<dyn Handler<Backend>>) {
        self.backends.insert(name.to_string(), handler);
    }

    /// Constructs a backend from an element by dispatching to the registered
    /// handler for `element.kind`.
    pub async fn backend(&self, element: &Element, ctx: &Context) -> ContextResult<Backend> {
        let constructor = self
            .backends
            .get(&element.kind)
            .context(error::NoProviderSnafu {
                component: "backend",
                kind: element.kind.clone(),
            })?;
        constructor.call(element.clone(), ctx.clone()).await
    }

    /// Registers an environment farm handler under the given kind name.
    pub fn register_farm(&self, name: &str, handler: Arc<dyn Handler<Farm>>) {
        self.farms.insert(name.to_string(), handler);
    }

    /// Constructs a farm from an element by dispatching to the registered
    /// handler for `element.kind`.
    pub async fn farm(&self, element: &Element, ctx: &Context) -> ContextResult<Farm> {
        let constructor = self
            .farms
            .get(&element.kind)
            .context(error::NoProviderSnafu {
                component: "environment",
                kind: element.kind.clone(),
            })?;
        constructor.call(element.clone(), ctx.clone()).await
    }

    /// Registers a source handler under the given kind name.
    pub fn register_source(&self, name: &str, handler: Arc<dyn Handler<Source>>) {
        self.sources.insert(name.to_string(), handler);
    }

    /// Constructs a source from an element by dispatching to the registered
    /// handler for `element.kind`.
    pub async fn source(&self, element: &Element, ctx: &Context) -> ContextResult<Source> {
        let constructor = self
            .sources
            .get(&element.kind)
            .context(error::NoProviderSnafu {
                component: "source",
                kind: element.kind.clone(),
            })?;
        constructor.call(element.clone(), ctx.clone()).await
    }

    /// Registers a transform handler under the given kind name.
    pub fn register_transform(&self, name: &str, handler: Arc<dyn Handler<Transform>>) {
        self.transforms.insert(name.to_string(), handler);
    }

    /// Constructs a transform from an element by dispatching to the registered
    /// handler for `element.kind`.
    pub async fn transform(&self, element: &Element, ctx: &Context) -> ContextResult<Transform> {
        let constructor = self
            .transforms
            .get(&element.kind)
            .context(error::NoProviderSnafu {
                component: "transform",
                kind: element.kind.clone(),
            })?;
        constructor.call(element.clone(), ctx.clone()).await
    }

    /// Registers a vendor handler under the given kind name.
    pub fn register_vendor(&self, name: &str, handler: Arc<dyn Handler<Vendor>>) {
        self.vendors.insert(name.to_string(), handler);
    }

    /// Constructs a vendor from an element by dispatching to the registered
    /// handler for `element.kind`.
    pub async fn vendor(&self, element: &Element, ctx: &Context) -> ContextResult<Vendor> {
        let constructor = self
            .vendors
            .get(&element.kind)
            .context(error::NoProviderSnafu {
                component: "vendor",
                kind: element.kind.clone(),
            })?;
        constructor.call(element.clone(), ctx.clone()).await
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the registry. Each test exercises only the registration
    //! side of the API: handlers are dummy closures that panic if invoked,
    //! since invocation requires a real `Context` (covered by integration
    //! tests in `mod.rs`).
    use super::*;

    fn dummy_backend_handler() -> Arc<dyn Handler<Backend>> {
        Arc::new(|_el: Element, _ctx: Context| async move { unreachable!("not invoked") })
    }

    fn dummy_farm_handler() -> Arc<dyn Handler<Farm>> {
        Arc::new(|_el: Element, _ctx: Context| async move { unreachable!("not invoked") })
    }

    fn dummy_source_handler() -> Arc<dyn Handler<Source>> {
        Arc::new(|_el: Element, _ctx: Context| async move { unreachable!("not invoked") })
    }

    fn dummy_transform_handler() -> Arc<dyn Handler<Transform>> {
        Arc::new(|_el: Element, _ctx: Context| async move { unreachable!("not invoked") })
    }

    fn dummy_vendor_handler() -> Arc<dyn Handler<Vendor>> {
        Arc::new(|_el: Element, _ctx: Context| async move { unreachable!("not invoked") })
    }

    #[test]
    fn default_registry_has_empty_maps() {
        let r = Registry::default();
        assert!(r.backends.is_empty());
        assert!(r.farms.is_empty());
        assert!(r.sources.is_empty());
        assert!(r.transforms.is_empty());
        assert!(r.vendors.is_empty());
    }

    #[test]
    fn register_backend_inserts_into_map() {
        let r = Registry::default();
        r.register_backend("kind-backend", dummy_backend_handler());
        assert_eq!(r.backends.len(), 1);
        assert!(r.backends.contains_key("kind-backend"));
    }

    #[test]
    fn register_farm_inserts_into_map() {
        let r = Registry::default();
        r.register_farm("kind-farm", dummy_farm_handler());
        assert_eq!(r.farms.len(), 1);
        assert!(r.farms.contains_key("kind-farm"));
    }

    #[test]
    fn register_source_inserts_into_map() {
        let r = Registry::default();
        r.register_source("kind-source", dummy_source_handler());
        assert_eq!(r.sources.len(), 1);
        assert!(r.sources.contains_key("kind-source"));
    }

    #[test]
    fn register_transform_inserts_into_map() {
        let r = Registry::default();
        r.register_transform("kind-transform", dummy_transform_handler());
        assert_eq!(r.transforms.len(), 1);
        assert!(r.transforms.contains_key("kind-transform"));
    }

    #[test]
    fn register_vendor_inserts_into_map() {
        let r = Registry::default();
        r.register_vendor("kind-vendor", dummy_vendor_handler());
        assert_eq!(r.vendors.len(), 1);
        assert!(r.vendors.contains_key("kind-vendor"));
    }
}
