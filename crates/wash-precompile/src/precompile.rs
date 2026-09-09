use anyhow::Result;
use wasmtime::{Config, Engine};

pub fn compile(wasm_bytes: &[u8]) -> Result<Vec<u8>> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Keep in lockstep with the host's engine builder
    // (`wash_runtime::engine::Engine::builder`), which explains why the system
    // unwinder is left out of this: registering per-function FDEs makes
    // workload teardown quadratic. This maps to Cranelift's `unwind_info`
    // setting, which is recorded in the `.cwasm` and checked when the host
    // loads it, so the two have to agree or every artifact this produces is
    // refused as "incompatible with native host".
    config.native_unwind_info(false);
    #[cfg(feature = "epoch-interruption")]
    config.epoch_interruption(true);

    let engine = Engine::new(&config)
        .map_err(|e| anyhow::anyhow!("Error setting up wasmtime engine: {e}"))?;
    let cwasm = engine
        .precompile_component(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("Error precompiling wasm component: {e}"))?;
    Ok(cwasm)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The host refuses an artifact whose `unwind_info` setting differs from its
    // own, so this asserts the section is really gone rather than trusting the
    // flag — the two crates have to stay in lockstep.
    #[test]
    fn precompiled_artifacts_carry_no_native_unwind_info() {
        let wasm = wat::parse_str(
            r#"(component
                 (core module $m (func (export "f") (result i32) i32.const 1))
                 (core instance (instantiate $m))
               )"#,
        )
        .unwrap();

        let cwasm = compile(&wasm).unwrap();

        assert!(
            !cwasm.windows(b".eh_frame".len()).any(|w| w == b".eh_frame"),
            "precompiled artifact still carries an .eh_frame section"
        );
    }

    #[test]
    fn precompiles_a_minimal_component() {
        let wasm = wat::parse_str("(component)").unwrap();

        let cwasm = compile(&wasm).unwrap();
        assert!(!cwasm.is_empty());
    }
}
