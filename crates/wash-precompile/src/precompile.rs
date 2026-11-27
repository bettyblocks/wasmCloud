use anyhow::Result;
use wasmtime::{Config, Engine};

/// Builds the wasmtime engine. A batch builds it once and shares it for every
/// item (an `Engine` is a cheap `Arc` clone).
pub fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Keep in lockstep with the host's engine builder
    // On musl the system unwinder is left out:
    // registering per-function FDEs makes workload teardown quadratic there.
    #[cfg(target_env = "musl")]
    config.native_unwind_info(false);
    #[cfg(feature = "epoch-interruption")]
    config.epoch_interruption(true);

    Engine::new(&config).map_err(|e| anyhow::anyhow!("Error setting up wasmtime engine: {e}"))
}

/// Precompiles one component with an existing engine.
pub fn compile_with(engine: &Engine, wasm_bytes: &[u8]) -> Result<Vec<u8>> {
    let cwasm = engine
        .precompile_component(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("Error precompiling wasm component: {e}"))?;
    Ok(cwasm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(wasm_bytes: &[u8]) -> Result<Vec<u8>> {
        compile_with(&engine()?, wasm_bytes)
    }

    // The host refuses an artifact whose `unwind_info` setting differs from its
    // own, so this asserts the section is really gone rather than trusting the
    // flag — the two crates have to stay in lockstep. Only musl disables it.
    #[cfg(target_env = "musl")]
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

    #[test]
    fn one_engine_compiles_many_components() {
        let engine = engine().unwrap();
        let wasm = wat::parse_str("(component)").unwrap();

        let first = compile_with(&engine, &wasm).unwrap();
        let second = compile_with(&engine, &wasm).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn invalid_wasm_is_an_error() {
        let err = compile(b"not wasm").unwrap_err();
        assert!(err.to_string().contains("Error precompiling"));
    }
}
