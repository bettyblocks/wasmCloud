package runtime_operator

import (
	"strings"
	"testing"
)

func TestValidateContentAddressed(t *testing.T) {
	ok := EmbeddedOperatorConfig{
		PrecompileContentAddressed: true,
		PrecompileEngineProfile:    "0123456789abcdef",
		PrecompileArtifactBaseURL:  "nats://precompiled-artifacts",
	}
	if err := validateContentAddressed(ok); err != nil {
		t.Fatalf("valid config: %v", err)
	}

	noProfile := ok
	noProfile.PrecompileEngineProfile = ""
	if err := validateContentAddressed(noProfile); err == nil || !strings.Contains(err.Error(), "PrecompileEngineProfile") {
		t.Fatalf("missing profile: %v", err)
	}

	notNats := ok
	notNats.PrecompileArtifactBaseURL = "file:///tmp/cwasm"
	if err := validateContentAddressed(notNats); err == nil || !strings.Contains(err.Error(), "nats://") {
		t.Fatalf("non-nats base URL: %v", err)
	}
}
