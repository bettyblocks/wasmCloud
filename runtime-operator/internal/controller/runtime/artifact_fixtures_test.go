package runtime

// Shared fixtures for the Artifact, precompile and artifact GC tests, factored
// out so repeated literals stay consistent across the suite.
const (
	testArtifactName    = "comp"
	testImageV1         = "ghcr.io/x/y:v1"
	testImageV2         = "ghcr.io/x/y:v2"
	testTarget          = "x86_64-unknown-linux-gnu"
	testWasmtimeVersion = "27.0.0"
	testEchoName        = "echo"
	testLiveName        = "live"
	gcTestKey           = "a/img/x86_64-27.0.0.cwasm"
)
