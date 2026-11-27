use clap::Parser;
use url::Url;

mod digest_key;
mod output;
mod precompile;
mod pull;

#[derive(Parser, Debug)]
#[command(name = "wash-precompile")]
struct Args {
    /// OCI reference to the source component
    #[arg(long, required_unless_present = "print_engine_profile")]
    image: Option<String>,

    /// URL where the precompiled .cwasm bytes will be written
    #[arg(long, required_unless_present = "print_engine_profile")]
    output: Option<String>,

    /// Print the engine profile of this worker and exit. The operator puts
    /// it in content-addressed output keys (-precompile-engine-profile).
    #[arg(long, conflicts_with_all = ["image", "output"])]
    print_engine_profile: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let settings = precompile::EngineSettings::current();
    let profile = settings.profile();
    if args.print_engine_profile {
        println!("{profile}");
        return Ok(());
    }
    let (Some(image), Some(output)) = (args.image, args.output) else {
        unreachable!("clap requires --image and --output");
    };
    let output_url = Url::parse(&output)?;
    // Fail before the pull when the key is malformed or names another engine.
    digest_key::parse(&output_url)?;

    let wasm = pull::fetch(&image).await?;
    // A content-addressed key is shared by every Artifact with the same
    // bytes, so it must name the bytes and the engine of this run.
    digest_key::check(&output_url, &wasm, &profile)?;
    let cwasm = precompile::compile(&settings.engine()?, &wasm)?;
    output::write(&output_url, &cwasm).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_and_output_flags() {
        let args = Args::parse_from([
            "wash-precompile",
            "--image",
            "ghcr.io/example/comp:v1",
            "--output",
            "file:///tmp/out.cwasm",
        ]);
        assert_eq!(args.image.as_deref(), Some("ghcr.io/example/comp:v1"));
        assert_eq!(args.output.as_deref(), Some("file:///tmp/out.cwasm"));
        assert!(!args.print_engine_profile);
    }

    #[test]
    fn image_and_output_are_required_without_print_engine_profile() {
        assert!(Args::try_parse_from(["wash-precompile", "--image", "x"]).is_err());
        assert!(Args::try_parse_from(["wash-precompile"]).is_err());
    }

    #[test]
    fn print_engine_profile_needs_no_other_flags() {
        let args = Args::parse_from(["wash-precompile", "--print-engine-profile"]);
        assert!(args.print_engine_profile);
        assert!(
            Args::try_parse_from(["wash-precompile", "--print-engine-profile", "--image", "x"])
                .is_err()
        );
    }
}
