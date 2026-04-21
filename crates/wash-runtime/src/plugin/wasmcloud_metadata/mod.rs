use std::collections::HashSet;

use crate::{
    engine::{
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::WorkloadItem,
    },
    plugin::HostPlugin,
    wit::{WitInterface, WitWorld},
};

mod bindings {
    wasmtime::component::bindgen!({
        world: "wasmcloud-metadata",
        imports: {
            default: async | trappable | tracing
        },
    });
}

use bindings::wasmcloud::metadata::host::add_to_linker;

const PLUGIN_ID: &str = "wasmcloud-metadata";

#[derive(Clone, Default)]
pub struct WasmcloudMetadata;

impl<'a> bindings::wasmcloud::metadata::host::Host for ActiveCtx<'a> {
    async fn get_ctx_id(&mut self) -> wasmtime::Result<String> {
        Ok(self.id.clone())
    }

    async fn get_component_id(&mut self) -> wasmtime::Result<String> {
        Ok(self.component_id.to_string())
    }

    async fn get_workload_id(&mut self) -> wasmtime::Result<String> {
        Ok(self.workload_id.to_string())
    }
}

#[async_trait::async_trait]
impl HostPlugin for WasmcloudMetadata {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from("wasmcloud:metadata/host@0.1.0")]),
            exports: HashSet::new(),
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: HashSet<WitInterface>,
    ) -> anyhow::Result<()> {
        let Some(_) = interfaces.iter().find(|i| {
            i.namespace == "wasmcloud" && i.package == "metadata" && i.interfaces.contains("host")
        }) else {
            tracing::warn!(
                "WasmcloudMetadata plugin requested for unexpected interface(s): {:?}",
                interfaces
            );
            return Ok(());
        };

        add_to_linker::<_, SharedCtx>(item.linker(), extract_active_ctx)?;

        Ok(())
    }
}
