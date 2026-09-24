// Generates arkd-rules.json: arkd's own verdict on each escrow in escrow.json.
//
// arkd runs TapscriptsVtxoScript.Validate on every VTXO spent in an intent.
// ServerRules::check in this crate must agree with it. To regenerate, from an arkd checkout:
//
//	cp escrow_rules_test.go pkg/ark-lib/script/zz_escrow_rules_test.go
//	cd pkg/ark-lib
//	ESCROW_JSON=/path/to/escrow.json ARKD_RULES_OUT=/path/to/arkd-rules.json \
//	  ARKD_COMMIT=$(git rev-parse HEAD) go test -run TestEscrowRules ./script/
//	rm script/zz_escrow_rules_test.go
package script_test

import (
	"encoding/hex"
	"encoding/json"
	"os"
	"testing"

	arklib "github.com/arkade-os/arkd/pkg/ark-lib"
	"github.com/arkade-os/arkd/pkg/ark-lib/script"
	"github.com/btcsuite/btcd/btcec/v2/schnorr"
)

// Mutinynet's signer and its deprecated signer, from /v1/info on 2026-09-21.
const (
	mutinynetSigner           = "301078808e4f7bc0dadfe29e34b1df8eaf0108ef06b1722274075ebc107a127a"
	mutinynetDeprecatedSigner = "fa73c6e4876ffb2dfc961d763cca9abc73d4b88efcb8f5e7ff92dc55e9aa553d"
)

type timelock struct {
	Type  string `json:"type"`
	Value uint32 `json:"value"`
}

type rules struct {
	Signer                string   `json:"signer"`
	MinExitDelay          timelock `json:"minExitDelay"`
	BlockTimelocksAllowed bool     `json:"blockTimelocksAllowed"`
}

type check struct {
	Rules    rules  `json:"rules"`
	Accepted bool   `json:"accepted"`
	Error    string `json:"error,omitempty"`
}

type verdict struct {
	Description      string  `json:"description"`
	TweakedPublicKey string  `json:"tweakedPublicKey"`
	Checks           []check `json:"checks"`
}

type vector struct {
	Description string   `json:"description"`
	Server      string   `json:"server"`
	ExitDelay   timelock `json:"exitDelay"`
	Expected    struct {
		Leaves           []string `json:"leaves"`
		TweakedPublicKey string   `json:"tweakedPublicKey"`
	} `json:"expected"`
}

func (v vector) ruleSets() []rules {
	// arkd counts a block as one second, so this is longer for either unit.
	longer := timelock{Type: "seconds", Value: v.ExitDelay.Value + 512}
	return []rules{
		{v.Server, v.ExitDelay, true},
		{v.Server, v.ExitDelay, false},
		{v.Server, longer, true},
		{mutinynetDeprecatedSigner, v.ExitDelay, true},
		{mutinynetSigner, timelock{"seconds", 2048}, false},
	}
}

func toArkd(t timelock) arklib.RelativeLocktime {
	if t.Type == "blocks" {
		return arklib.RelativeLocktime{Type: arklib.LocktimeTypeBlock, Value: t.Value}
	}
	return arklib.RelativeLocktime{Type: arklib.LocktimeTypeSecond, Value: t.Value}
}

func TestEscrowRules(t *testing.T) {
	input, err := os.ReadFile(os.Getenv("ESCROW_JSON"))
	if err != nil {
		t.Fatal(err)
	}
	var fixtures struct {
		Vectors []vector `json:"vectors"`
	}
	if err := json.Unmarshal(input, &fixtures); err != nil {
		t.Fatal(err)
	}

	verdicts := make([]verdict, 0, len(fixtures.Vectors))
	for _, v := range fixtures.Vectors {
		vtxo, err := script.ParseVtxoScript(v.Expected.Leaves)
		if err != nil {
			t.Fatalf("%s: parse: %v", v.Description, err)
		}
		key, _, err := vtxo.TapTree()
		if err != nil {
			t.Fatalf("%s: tap tree: %v", v.Description, err)
		}
		tweaked := hex.EncodeToString(schnorr.SerializePubKey(key))
		if tweaked != v.Expected.TweakedPublicKey {
			t.Fatalf("%s: arkd tweaked key %s, SDK %s", v.Description, tweaked, v.Expected.TweakedPublicKey)
		}

		out := verdict{Description: v.Description, TweakedPublicKey: tweaked}
		for _, r := range v.ruleSets() {
			signerBytes, err := hex.DecodeString(r.Signer)
			if err != nil {
				t.Fatal(err)
			}
			signer, err := schnorr.ParsePubKey(signerBytes)
			if err != nil {
				t.Fatal(err)
			}
			c := check{Rules: r, Accepted: true}
			if err := vtxo.Validate(signer, toArkd(r.MinExitDelay), r.BlockTimelocksAllowed); err != nil {
				c.Accepted = false
				c.Error = err.Error()
			}
			out.Checks = append(out.Checks, c)
		}
		verdicts = append(verdicts, out)
	}

	encoded, err := json.MarshalIndent(map[string]any{
		"generator": "arkd " + os.Getenv("ARKD_COMMIT") + " pkg/ark-lib/script TapscriptsVtxoScript.Validate",
		"verdicts":  verdicts,
	}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(os.Getenv("ARKD_RULES_OUT"), append(encoded, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
