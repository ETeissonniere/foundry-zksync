//! ZKSolc module.

mod compile;
mod config;
mod factory_deps;
mod manager;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
};

pub use compile::*;
pub use config::*;
pub use factory_deps::*;
use foundry_compilers::{
    zksync::{
        artifact_output::Artifact as ZkArtifact,
        compile::output::ProjectCompileOutput as ZkProjectCompileOutput,
    },
    Artifact, ProjectCompileOutput,
};
pub use manager::*;

use alloy_primitives::{keccak256, B256};
use tracing::debug;
use zksync_types::H256;

/// Defines a contract that has been dual compiled with both zksolc and solc
#[derive(Debug, Default, Clone)]
pub struct DualCompiledContract {
    /// Contract name
    pub name: String,
    /// Deployed bytecode with zksolc
    pub zk_bytecode_hash: H256,
    /// Deployed bytecode hash with zksolc
    pub zk_deployed_bytecode: Vec<u8>,
    /// Deployed bytecode factory deps
    pub zk_factory_deps: Vec<Vec<u8>>,
    /// Deployed bytecode hash with solc
    pub evm_bytecode_hash: B256,
    /// Deployed bytecode with solc
    pub evm_deployed_bytecode: Vec<u8>,
    /// Bytecode with solc
    pub evm_bytecode: Vec<u8>,
}

/// A collection of `[DualCompiledContract]`s
#[derive(Debug, Default, Clone)]
pub struct DualCompiledContracts {
    contracts: Vec<DualCompiledContract>,
}

impl DualCompiledContracts {
    /// Creates a collection of `[DualCompiledContract]`s from the provided solc and zksolc output.
    pub fn new(output: &ProjectCompileOutput, zk_output: &ZkProjectCompileOutput) -> Self {
        let mut dual_compiled_contracts = vec![];
        let mut solc_bytecodes = HashMap::new();
        for (contract_name, artifact) in output.artifacts() {
            let contract_name =
                contract_name.split('.').next().expect("name cannot be empty").to_string();
            let deployed_bytecode = artifact.get_deployed_bytecode();
            let deployed_bytecode = deployed_bytecode
                .as_ref()
                .and_then(|d| d.bytecode.as_ref().and_then(|b| b.object.as_bytes()));
            let bytecode = artifact.get_bytecode().and_then(|b| b.object.as_bytes().cloned());
            if let Some(bytecode) = bytecode {
                if let Some(deployed_bytecode) = deployed_bytecode {
                    solc_bytecodes
                        .insert(contract_name.clone(), (bytecode, deployed_bytecode.clone()));
                }
            }
        }

        // DualCompiledContracts uses a vec of bytecodes as factory deps field vs
        // the <hash, name> map zksolc outputs, hence we need all bytecodes upfront to
        // then do the conversion
        let mut zksolc_all_bytecodes: HashMap<String, Vec<u8>> = Default::default();
        for (_, zk_artifact) in zk_output.artifacts() {
            if let (Some(hash), Some(bytecode)) = (&zk_artifact.hash, &zk_artifact.bytecode) {
                // TODO: we can do this because no bytecode object could be unlinked
                // at this stage for zksolc, and BytecodeObject as ref will get the bytecode bytes.
                // We should be careful however we should check and handle errors in
                // case an Unlinked BytecodeObject gets here somehow
                let bytes = bytecode.object.clone().into_bytes().unwrap();
                zksolc_all_bytecodes.insert(hash.clone(), bytes.to_vec());
            }
        }

        for (contract_name, artifact) in zk_output.artifacts() {
            let maybe_bytecode = &artifact.bytecode;
            let maybe_hash = &artifact.hash;
            let maybe_factory_deps = &artifact.factory_dependencies;
            if let (Some(bytecode), Some(hash), Some(factory_deps_map)) =
                (maybe_bytecode, maybe_hash, maybe_factory_deps)
            {
                if let Some((solc_bytecode, solc_deployed_bytecode)) =
                    solc_bytecodes.get(&contract_name)
                {
                    // TODO: we can do this because no bytecode object could be unlinked
                    // at this stage for zksolc, and BytecodeObject as ref will get the bytecode
                    // bytes. We should be careful however we should check and
                    // handle errors in case an Unlinked BytecodeObject gets
                    // here somehow
                    let bytecode_vec = bytecode.object.clone().into_bytes().unwrap().to_vec();
                    let mut factory_deps_vec: Vec<Vec<u8>> = factory_deps_map
                        .keys()
                        .map(|factory_hash| zksolc_all_bytecodes.get(factory_hash).unwrap())
                        .cloned()
                        .collect();

                    factory_deps_vec.push(bytecode_vec.clone());

                    dual_compiled_contracts.push(DualCompiledContract {
                        name: contract_name,
                        zk_bytecode_hash: H256::from_str(hash).unwrap(),
                        zk_deployed_bytecode: bytecode_vec,
                        zk_factory_deps: factory_deps_vec,
                        evm_bytecode_hash: keccak256(solc_deployed_bytecode),
                        evm_bytecode: solc_bytecode.to_vec(),
                        evm_deployed_bytecode: solc_deployed_bytecode.to_vec(),
                    });
                }
            }
        }

        Self { contracts: dual_compiled_contracts }
    }

    /// Finds a contract matching the ZK deployed bytecode
    pub fn find_by_zk_deployed_bytecode(&self, bytecode: &[u8]) -> Option<&DualCompiledContract> {
        self.contracts.iter().find(|contract| bytecode.starts_with(&contract.zk_deployed_bytecode))
    }

    /// Finds a contract matching the EVM bytecode
    pub fn find_by_evm_bytecode(&self, bytecode: &[u8]) -> Option<&DualCompiledContract> {
        self.contracts.iter().find(|contract| bytecode.starts_with(&contract.evm_bytecode))
    }

    /// Finds a contract matching the ZK bytecode hash
    pub fn find_by_zk_bytecode_hash(&self, code_hash: H256) -> Option<&DualCompiledContract> {
        self.contracts.iter().find(|contract| code_hash == contract.zk_bytecode_hash)
    }

    /// Finds a contract own and nested factory deps
    pub fn fetch_all_factory_deps(&self, root: &DualCompiledContract) -> Vec<Vec<u8>> {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        for dep in &root.zk_factory_deps {
            queue.push_back(dep);
        }

        while let Some(dep) = queue.pop_front() {
            // try to insert in the list of visited, if it's already present, skip
            if visited.insert(dep) {
                if let Some(contract) = self.find_by_zk_deployed_bytecode(dep) {
                    debug!(
                        name = contract.name,
                        deps = contract.zk_factory_deps.len(),
                        "new factory depdendency"
                    );

                    for nested_dep in &contract.zk_factory_deps {
                        // check that the nested dependency is inserted
                        if !visited.contains(nested_dep) {
                            // if not, add it to queue for processing
                            queue.push_back(nested_dep);
                        }
                    }
                }
            }
        }

        visited.into_iter().cloned().collect()
    }

    /// Returns an iterator over all `[DualCompiledContract]`s in the collection
    pub fn iter(&self) -> impl Iterator<Item = &DualCompiledContract> {
        self.contracts.iter()
    }

    /// Adds a new `[DualCompiledContract]` to the collection
    pub fn push(&mut self, contract: DualCompiledContract) {
        self.contracts.push(contract);
    }
}
