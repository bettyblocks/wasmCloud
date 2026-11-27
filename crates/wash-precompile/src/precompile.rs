use anyhow::Result;
use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine};

/// The engine settings that change the `.cwasm` and that this worker
/// controls. [`EngineSettings::config`] builds the engine from them and
/// [`EngineSettings::profile`] names them, so the two cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSettings {
    /// `<arch>-<os>-<env>` of this build, for example `x86_64-linux-gnu`.
    pub target: String,
    pub epoch_interruption: bool,
    /// `None` keeps wasmtime's default.
    pub native_unwind_info: Option<bool>,
}

impl EngineSettings {
    /// The settings of this build of the worker.
    pub fn current() -> Self {
        let env = if cfg!(target_env = "musl") {
            "musl"
        } else if cfg!(target_env = "gnu") {
            "gnu"
        } else {
            "other"
        };
        Self {
            target: format!("{}-{}-{env}", std::env::consts::ARCH, std::env::consts::OS),
            epoch_interruption: cfg!(feature = "epoch-interruption"),
            // Keep in lockstep with the host's engine builder.
            // On musl the system unwinder is left out:
            // registering per-function FDEs makes workload teardown quadratic there.
            native_unwind_info: if cfg!(target_env = "musl") {
                Some(false)
            } else {
                None
            },
        }
    }

    pub fn config(&self) -> Config {
        let mut config = Config::new();
        config.wasm_component_model(true);
        if let Some(unwind) = self.native_unwind_info {
            config.native_unwind_info(unwind);
        }
        config.epoch_interruption(self.epoch_interruption);
        config
    }

    pub fn engine(&self) -> Result<Engine> {
        Engine::new(&self.config())
            .map_err(|e| anyhow::anyhow!("Error setting up wasmtime engine: {e}"))
    }

    /// A short name (16 lower-case hex characters) for these settings. The
    /// operator puts it in content-addressed keys, so workers that build
    /// different `.cwasm` bytes for the same `.wasm` never share an object.
    ///
    /// It deliberately does not use wasmtime's
    /// `Engine::precompile_compatibility_hash`: that hash includes the CPU
    /// features cranelift detects on the node the Job happens to run on, so
    /// it would change from node to node. The wasmtime version is a separate
    /// part of the key.
    pub fn profile(&self) -> String {
        let unwind = match self.native_unwind_info {
            None => "default",
            Some(true) => "on",
            Some(false) => "off",
        };
        let canonical = format!(
            "v1;target={};component-model=on;epoch-interruption={};native-unwind-info={unwind}",
            self.target,
            if self.epoch_interruption { "on" } else { "off" },
        );
        Sha256::digest(canonical.as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

pub fn compile(engine: &Engine, wasm_bytes: &[u8]) -> Result<Vec<u8>> {
    let cwasm = engine
        .precompile_component(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("Error precompiling wasm component: {e}"))?;
    Ok(cwasm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        EngineSettings::current().engine().unwrap()
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

        let cwasm = compile(&engine(), &wasm).unwrap();

        assert!(
            !cwasm.windows(b".eh_frame".len()).any(|w| w == b".eh_frame"),
            "precompiled artifact still carries an .eh_frame section"
        );
    }

    #[test]
    fn precompiles_a_minimal_component() {
        let wasm = wat::parse_str("(component)").unwrap();

        let cwasm = compile(&engine(), &wasm).unwrap();
        assert!(!cwasm.is_empty());
    }

    #[test]
    fn profile_is_16_lower_hex_and_stable() {
        let a = EngineSettings::current().profile();
        assert_eq!(a, EngineSettings::current().profile());
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
    }

    #[test]
    fn profile_changes_with_every_setting() {
        let base = EngineSettings::current();
        let mut epoch = base.clone();
        epoch.epoch_interruption = !epoch.epoch_interruption;
        let mut unwind = base.clone();
        unwind.native_unwind_info = Some(!base.native_unwind_info.unwrap_or(true));
        let mut target = base.clone();
        target.target = "aarch64-linux-gnu".into();

        let profiles: std::collections::HashSet<String> = [&base, &epoch, &unwind, &target]
            .iter()
            .map(|s| s.profile())
            .collect();
        assert_eq!(profiles.len(), 4, "every setting must change the profile");
    }

    // The profile is only useful if the settings it names really change the
    // compiled bytes. Epoch interruption adds checks to loop back-edges.
    #[test]
    fn epoch_setting_changes_the_cwasm() {
        let wasm = wat::parse_str(
            r#"(component
                 (core module $m (func (export "f") (loop $l br $l)))
                 (core instance (instantiate $m))
               )"#,
        )
        .unwrap();
        let on = EngineSettings {
            epoch_interruption: true,
            ..EngineSettings::current()
        };
        let off = EngineSettings {
            epoch_interruption: false,
            ..EngineSettings::current()
        };
        let a = compile(&on.engine().unwrap(), &wasm).unwrap();
        let b = compile(&off.engine().unwrap(), &wasm).unwrap();
        assert_ne!(a, b);
        assert_ne!(on.profile(), off.profile());
    }
}
