use crate::{adapter::ModuleAdapter, Module, Profile};
use anyhow::{anyhow, bail, Result};
use petgraph::{algo::toposort, graph::NodeIndex, Graph};
use std::collections::{hash_map::Entry, HashMap, HashSet};
use wasmparser::{ExternalKind, FuncType, ImportSectionEntryType, Type, TypeDef};

pub fn to_val_type(ty: &Type) -> wasm_encoder::ValType {
    match ty {
        Type::I32 => wasm_encoder::ValType::I32,
        Type::I64 => wasm_encoder::ValType::I64,
        Type::F32 => wasm_encoder::ValType::F32,
        Type::F64 => wasm_encoder::ValType::F64,
        Type::V128 => wasm_encoder::ValType::V128,
        Type::FuncRef => wasm_encoder::ValType::FuncRef,
        Type::ExternRef => wasm_encoder::ValType::ExternRef,
        Type::ExnRef | Type::Func | Type::EmptyBlockType => {
            unimplemented!("unsupported value type")
        }
    }
}

struct LinkState<'a> {
    types: HashSet<&'a FuncType>,
    type_map: HashMap<&'a FuncType, u32>,
    imports: HashMap<&'a str, HashMap<Option<&'a str>, &'a FuncType>>,
    num_imported_funcs: u32,
    implicit_instances: HashMap<&'a str, u32>,
    modules: Vec<wasm_encoder::Module>,
    module_map: HashMap<&'a ModuleAdapter<'a>, (u32, Option<u32>)>,
    instances: Vec<(u32, Vec<(&'a str, u32)>)>,
    func_aliases: Vec<(u32, &'a str)>,
    table_aliases: Vec<(u32, &'a str)>,
    segments: Vec<(u32, Vec<wasm_encoder::Element>)>,
    exports: Vec<(&'a str, wasm_encoder::Export)>,
    module: wasm_encoder::Module,
}

/// Implements a WebAssembly module linker.
#[derive(Debug)]
pub struct Linker {
    profile: Profile,
}

impl Linker {
    /// Constructs a new WebAssembly module linker with the given profile.
    pub fn new(profile: Profile) -> Self {
        Self { profile }
    }

    /// Links the given module with the given set of imported modules.
    ///
    /// On success, returns a vector of bytes representing the linked module
    pub fn link(&self, main: &Module, imports: &HashMap<&str, Module>) -> Result<Vec<u8>> {
        if !main.exports.iter().any(|e| {
            if e.field == "_start" {
                return match e.kind {
                    ExternalKind::Function => {
                        let ty = main.func_type(e.index).unwrap();
                        ty.params.is_empty() && ty.returns.is_empty()
                    }
                    _ => false,
                };
            }
            false
        }) {
            bail!("main module `{}` must export a start function");
        }

        let graph = self.build_graph(main, imports)?;

        let mut state = self.link_state(&graph)?;

        self.write_type_section(&mut state);
        self.write_import_section(&mut state);
        self.write_module_section(&mut state);
        self.write_instance_section(&mut state);
        self.write_alias_section(&mut state);
        self.write_export_section(&mut state);
        self.write_element_section(&mut state);

        Ok(state.module.finish())
    }

    fn build_graph<'a>(
        &self,
        main: &'a Module,
        imports: &'a HashMap<&str, Module>,
    ) -> Result<Graph<ModuleAdapter<'a>, ()>> {
        let mut queue: Vec<(Option<petgraph::graph::NodeIndex>, &Module)> = Vec::new();
        let mut seen = HashMap::new();
        let mut graph: Graph<ModuleAdapter, ()> = Graph::new();

        queue.push((None, main));

        loop {
            match queue.pop() {
                Some((predecessor, module)) => {
                    let index = match seen.entry(module as *const _) {
                        Entry::Occupied(e) => *e.get(),
                        Entry::Vacant(e) => {
                            let index = graph.add_node(ModuleAdapter::new(module));

                            for import in &module.imports {
                                let imported_module = imports.get(import.module);

                                // Check for profile provided function imports before resolving exports on the imported module
                                if let ImportSectionEntryType::Function(i) = &import.ty {
                                    match module
                                        .types
                                        .get(*i as usize)
                                        .expect("function index must be in range")
                                    {
                                        TypeDef::Func(ft) => {
                                            if self.profile.provides(
                                                import.module,
                                                import.field,
                                                ft,
                                            ) {
                                                continue;
                                            }
                                        }
                                        _ => unreachable!("import must be a function"),
                                    }
                                }

                                let imported_module = imported_module.ok_or_else(|| {
                                    anyhow!(
                                        "module `{}` imports from unknown module `{}`",
                                        module.name,
                                        import.module
                                    )
                                })?;

                                imported_module.resolve_import(import, module)?;

                                queue.push((Some(index), imported_module));
                            }

                            *e.insert(index)
                        }
                    };

                    if let Some(predecessor) = predecessor {
                        graph.add_edge(predecessor, index, ());
                    };
                }
                None => break,
            }
        }

        // Ensure the graph is acyclic by performing a topographical sort.
        // This algorithm requires more space than `is_cyclic_directed`, but
        // performs the check iteratively rather than recursively.
        toposort(&graph, None).map_err(|e| {
            anyhow!(
                "module `{}` and its dependencies form a cycle in the import graph",
                graph[e.node_id()].module.name
            )
        })?;

        Ok(graph)
    }

    fn write_type_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::TypeSection::new();

        for (index, ty) in state.types.iter().enumerate() {
            section.function(
                ty.params.iter().map(to_val_type),
                ty.returns.iter().map(to_val_type),
            );
            state.type_map.insert(ty, index as u32);
        }

        state.module.section(&section);
    }

    fn write_import_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::ImportSection::new();

        for (module, imports) in &state.imports {
            for (field, ty) in imports {
                section.import(
                    module,
                    *field,
                    wasm_encoder::EntityType::Function(state.type_map[ty]),
                );
            }
        }

        state.module.section(&section);
    }

    fn write_module_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::ModuleSection::new();

        for module in &state.modules {
            section.module(module);
        }

        state.module.section(&section);
    }

    fn write_instance_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::InstanceSection::new();

        for (module, args) in &state.instances {
            section.instantiate(
                *module,
                args.iter()
                    .map(|(name, index)| (*name, wasm_encoder::Export::Instance(*index))),
            );
        }

        state.module.section(&section);
    }

    fn write_alias_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::AliasSection::new();

        for (index, name) in &state.func_aliases {
            section.instance_export(*index, wasm_encoder::ItemKind::Function, name);
        }

        for (index, name) in &state.table_aliases {
            section.instance_export(*index, wasm_encoder::ItemKind::Table, name);
        }

        state.module.section(&section);
    }

    fn write_export_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::ExportSection::new();

        for (name, export) in &state.exports {
            section.export(name, *export);
        }
        state.module.section(&section);
    }

    fn write_element_section(&self, state: &mut LinkState) {
        let mut section = wasm_encoder::ElementSection::new();

        for (table_index, elements) in &state.segments {
            section.active(
                Some(*table_index),
                wasm_encoder::Instruction::I32Const(0),
                wasm_encoder::ValType::FuncRef,
                wasm_encoder::Elements::Expressions(&elements),
            );
        }

        state.module.section(&section);
    }

    fn link_state<'a>(&self, graph: &'a Graph<ModuleAdapter<'a>, ()>) -> Result<LinkState<'a>> {
        let mut state = LinkState {
            types: HashSet::new(),
            type_map: HashMap::new(),
            imports: HashMap::new(),
            num_imported_funcs: 0,
            implicit_instances: HashMap::new(),
            modules: Vec::new(),
            module_map: HashMap::new(),
            instances: Vec::new(),
            func_aliases: Vec::new(),
            table_aliases: Vec::new(),
            segments: Vec::new(),
            exports: Vec::new(),
            module: wasm_encoder::Module::new(),
        };

        let mut num_imported_funcs = 0;
        for f in graph.node_indices() {
            let adapter = &graph[f];
            let module = adapter.module;

            // Add all profile imports to the base set of types and imports
            for import in &module.imports {
                let ty = module
                    .import_func_type(import)
                    .expect("expected import to be a function");

                if !self.profile.provides(import.module, import.field, ty) {
                    continue;
                }

                let imports = state.imports.entry(import.module).or_default();

                let existing = imports.entry(import.field).or_insert_with(|| {
                    num_imported_funcs += 1;
                    ty
                });

                if existing != &ty {
                    bail!(
                        "import `{}` from profile module `{}` has a conflicting type between different importing modules",
                        import.field.unwrap_or(""),
                        import.module
                    );
                }

                let len = state.implicit_instances.len();
                state
                    .implicit_instances
                    .entry(import.module)
                    .or_insert(len as u32);

                state.types.insert(ty);
            }

            let module_index = state.modules.len() as u32;
            state.modules.push(adapter.adapt()?);

            let shim_index = adapter.shim().map(|m| {
                let index = state.modules.len() as u32;
                state.modules.push(m);
                index
            });

            state.module_map.insert(adapter, (module_index, shim_index));
        }

        state.num_imported_funcs = num_imported_funcs;

        // Instantiate the main module
        let (main_index, _) = self.instantiate(&mut state, &graph, NodeIndex::new(0), None);

        // Re-export the start function
        let start_index = state.func_aliases.len() as u32;
        state.func_aliases.push((main_index, "_start"));
        state.exports.push((
            "_start",
            wasm_encoder::Export::Function(state.num_imported_funcs + start_index),
        ));

        Ok(state)
    }

    fn instantiate<'a>(
        &self,
        state: &mut LinkState<'a>,
        graph: &'a Graph<ModuleAdapter<'a>, ()>,
        current: NodeIndex,
        parent: Option<u32>,
    ) -> (u32, bool) {
        // TODO: make this iterative instead of recursive?

        // If a parent module was specified and this is a shim module, just instantiate it
        let (module_index, shim_index) = state.module_map[&graph[current]];
        if parent.is_none() {
            // Instantiate shims for adapted modules
            if let Some(shim_index) = shim_index {
                let index = (state.instances.len() + state.implicit_instances.len()) as u32;
                state.instances.push((shim_index, Vec::new()));
                return (index, true);
            }
        }

        // Add the implicit instances to the instantiation args
        let mut args = Vec::new();
        for (name, index) in &state.implicit_instances {
            args.push((*name, *index));
        }

        // Add the parent instance
        if let Some(parent) = parent {
            args.push((crate::adapter::PARENT_MODULE_NAME, parent));
        }

        // Recurse on each direct dependency in the graph
        let mut shims = Vec::new();
        let mut neighbors = graph.neighbors(current).detach();
        while let Some(neighbor) = neighbors.next_node(graph) {
            let (index, is_shim) = self.instantiate(state, graph, neighbor, None);
            args.push((graph[neighbor].module.name, index));
            if is_shim {
                shims.push((neighbor, index));
            }
        }

        // Instantiate the current module
        let parent_index = (state.instances.len() + state.implicit_instances.len()) as u32;
        state.instances.push((module_index, args));

        // For each shim that was instantiated, instantiate the real module passing in the parent
        for (shim, shim_index) in shims {
            let (child_index, _) = self.instantiate(state, graph, shim, Some(parent_index));

            // Emit the shim function table
            let adapter = &graph[shim];
            let table_index = state.table_aliases.len() as u32;
            state
                .table_aliases
                .push((shim_index, crate::adapter::FUNCTION_TABLE_NAME));

            // Emit the segments populating the function table
            let mut segments = Vec::new();
            for func in adapter.adapted_funcs() {
                let func_index = state.num_imported_funcs + state.func_aliases.len() as u32;
                state.func_aliases.push((child_index, func));

                segments.push(wasm_encoder::Element::Func(func_index));
            }

            state.segments.push((table_index, segments));
        }

        (parent_index, false)
    }
}