use std::cell::RefCell;
use std::collections::BTreeMap;

use super::{Addr, Node};
use semver::VersionReq;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::Value;

#[derive(ProvidesStaticType, Default)]
pub struct Store {
    nodes: RefCell<BTreeMap<Addr, Node>>,
}

impl Store {
    pub fn get_node(&self, name: &Addr) -> Option<Node> {
        self.nodes.borrow().get(name).cloned()
    }

    pub fn nodes(&self) -> BTreeMap<Addr, Node> {
        self.nodes.borrow().clone()
    }

    pub fn add_node(&self, name: Addr, node: Node) {
        self.nodes.borrow_mut().insert(name, node);
    }
}

#[starlark_module]
pub fn starlark_bindings(builder: &mut GlobalsBuilder) {
    fn source_cache(
        name: String,
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            table.insert(key.clone(), node);
        }
        let addr = Addr::parse(name.as_str()).unwrap();
        let node = Node::new_definition("source_cache", kind.as_str(), name.as_str(), table);
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn build_cache(
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            table.insert(key.clone(), node);
        }

        let node = Node::new_definition("build_cache", kind.as_str(), "edo-build-cache", table);
        let addr = Addr::parse("edo-build-cache").unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn output_cache(
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            table.insert(key.clone(), node);
        }

        let node = Node::new_definition("output_cache", kind.as_str(), "edo-output-cache", table);
        let addr = Addr::parse("edo-output-cache").unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn vendor(
        name: String,
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            if key == "source" {
                let mut sources = Vec::new();
                for entry in node.as_list().unwrap_or(vec![node]) {
                    let name = entry.as_string().unwrap();
                    let cnode = store
                        .get_node(&Addr::parse(name.as_str()).unwrap())
                        .unwrap();
                    sources.push(cnode);
                }
                table.insert(key.clone(), Node::new_list(sources));
            } else {
                table.insert(key.clone(), Node::try_from(value).unwrap());
            }
        }

        let node = Node::new_definition("vendor", kind.as_str(), name.as_str(), table);
        let addr = Addr::parse(name.as_str()).unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn source(
        name: String,
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        for (key, value) in args.entries.iter() {
            table.insert(key.clone(), Node::try_from(value).unwrap());
        }
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        let node = Node::new_definition("source", kind.as_str(), name.as_str(), table);
        let addr = Addr::parse(name.as_str()).unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn wants(
        name: String,
        kind: String,
        at: String,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let node = Node::new_definition(
            "wants",
            kind.as_str(),
            name.as_str(),
            BTreeMap::from([(
                "at".into(),
                Node::new_require(VersionReq::parse(at.as_str()).unwrap()),
            )]),
        );
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        let addr = Addr::parse(name.as_str()).unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn plugin(
        name: String,
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            if key == "source" {
                let mut sources = Vec::new();
                for entry in node.as_list().unwrap_or(vec![node]) {
                    let name = entry.as_string().unwrap();
                    let cnode = store
                        .get_node(&Addr::parse(name.as_str()).unwrap())
                        .unwrap();
                    sources.push(cnode);
                }
                table.insert(key.clone(), Node::new_list(sources));
            } else {
                table.insert(key.clone(), Node::try_from(value).unwrap());
            }
        }

        let node = Node::new_definition("plugin", kind.as_str(), name.as_str(), table);
        let addr = Addr::parse(name.as_str()).unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn environment(
        name: String,
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            if key == "source" {
                let mut sources = Vec::new();
                for entry in node.as_list().unwrap_or(vec![node]) {
                    let name = entry.as_string().unwrap();
                    let cnode = store
                        .get_node(&Addr::parse(name.as_str()).unwrap())
                        .unwrap();
                    sources.push(cnode);
                }
                table.insert(key.clone(), Node::new_list(sources));
            } else {
                table.insert(key.clone(), Node::try_from(value).unwrap());
            }
        }

        let node = Node::new_definition("environment", kind.as_str(), name.as_str(), table);
        let addr = Addr::parse(name.as_str()).unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }

    fn transform(
        name: String,
        kind: String,
        #[starlark(kwargs)] args: UnpackDictEntries<String, Value<'_>>,
        eval: &mut Evaluator,
    ) -> starlark::Result<String> {
        let mut table = BTreeMap::new();
        let store = eval.extra.unwrap().downcast_ref::<Store>().unwrap();
        for (key, value) in args.entries.iter() {
            let node = Node::try_from(value).unwrap();
            if key == "source" {
                let mut sources = Vec::new();
                for entry in node.as_list().unwrap_or(vec![node]) {
                    let name = entry.as_string().unwrap();
                    let cnode = store
                        .get_node(&Addr::parse(name.as_str()).unwrap())
                        .unwrap();
                    sources.push(cnode);
                }
                table.insert(key.clone(), Node::new_list(sources));
            } else {
                table.insert(key.clone(), Node::try_from(value).unwrap());
            }
        }

        let node = Node::new_definition("transform", kind.as_str(), name.as_str(), table);
        let addr = Addr::parse(name.as_str()).unwrap();
        store.add_node(addr.clone(), node.clone());
        Ok(addr.to_string())
    }
}
