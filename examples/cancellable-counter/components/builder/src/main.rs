//! Componentize the P3 guest core modules: cm-async guests cannot be
//! linked into components by the wasm32-wasip2 toolchain, so (like the
//! wash-runtime test fixtures) they are built as wasm32-wasip1 core
//! modules and wrapped here with the WASI preview1 reactor adapter.

use std::fs;
use std::path::Path;

use wasi_preview1_component_adapter_provider::WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER;

fn main() -> anyhow::Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest_dir.parent().unwrap().join("target");
    let core_dir = target.join("wasm32-wasip1/release");
    let out_dir = target.join("demo");
    fs::create_dir_all(&out_dir)?;

    for name in ["frontend", "counter"] {
        let core = fs::read(core_dir.join(format!("{name}.wasm").replace('-', "_")))?;
        let component = wit_component::ComponentEncoder::default()
            .validate(true)
            .module(&core)?
            .adapter("wasi_snapshot_preview1", WASI_SNAPSHOT_PREVIEW1_REACTOR_ADAPTER)?
            .encode()?;
        fs::write(out_dir.join(format!("{name}.wasm")), component)?;
        println!("componentized {name}");
    }

    Ok(())
}
