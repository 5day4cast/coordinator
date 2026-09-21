// Generates escrow.json: entry escrow vectors built with @arkade-os/sdk.
//
// The Rust crate must reproduce these bytes exactly. To regenerate:
//
//   npm install --no-save @arkade-os/sdk@0.4.74
//   node generate-escrow-vectors.mjs > escrow.json
//
// Each leaf uses the SDK's own closure encoders, so the vectors show what arkd and SDK clients expect.
import {
    ArkAddress,
    CLTVMultisigTapscript,
    CSVMultisigTapscript,
    decodeTapscript,
    MultisigTapscript,
    VtxoScript,
} from "@arkade-os/sdk";
import { hex } from "@scure/base";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

// The SDK does not export its package.json, so read it next to the resolved entry point.
const require = createRequire(import.meta.url);
const sdkDir = dirname(dirname(require.resolve("@arkade-os/sdk")));
const sdkVersion = JSON.parse(readFileSync(join(sdkDir, "package.json"), "utf8")).version;

// Keys from the SDK's tapscript and VHTLC tests, and Mutinynet's signerPubkey on 2026-09-21.
const keys = {
    ex1: "f8352deebdf5658d95875d89656112b1dd150f176c702eea4f91a91527e48e26",
    ex2: "fc68d5ea9279cc9d2c57e6885e21bbaee9c3aec85089f1d6c705c017d321ea84",
    vhtlcSender: "0192e796452d6df9697c280542e1560557bcf79a347d925895043136225c7cb4",
    vhtlcReceiver: "1e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53b",
    vhtlcServer: "aad52d58162e9eefeafc7ad8a1cdca8060b5f01df1e7583362d052e266208f88",
    mutinynetSigner: "301078808e4f7bc0dadfe29e34b1df8eaf0108ef06b1722274075ebc107a127a",
};

const cases = [
    {
        description: "Mutinynet signer, height locktime, 2048 second exit delay",
        player: keys.ex1,
        coordinator: keys.ex2,
        server: keys.mutinynetSigner,
        refundLocktime: 3444600,
        exitDelay: { type: "seconds", value: 2048 },
        unilateralRefundDelay: { type: "seconds", value: 1209856 },
    },
    {
        description: "timestamp locktime, 144 block exit delay",
        player: keys.vhtlcSender,
        coordinator: keys.vhtlcReceiver,
        server: keys.vhtlcServer,
        refundLocktime: 1790000000,
        exitDelay: { type: "blocks", value: 144 },
        unilateralRefundDelay: { type: "blocks", value: 1008 },
    },
    {
        description: "16 block exit delay encodes as OP_16, 17 block refund delay does not",
        player: keys.ex2,
        coordinator: keys.ex1,
        server: keys.mutinynetSigner,
        refundLocktime: 265,
        exitDelay: { type: "blocks", value: 16 },
        unilateralRefundDelay: { type: "blocks", value: 17 },
    },
    {
        description: "small height locktime, maximum block refund delay",
        player: keys.vhtlcReceiver,
        coordinator: keys.vhtlcSender,
        server: keys.mutinynetSigner,
        refundLocktime: 17,
        exitDelay: { type: "blocks", value: 17 },
        unilateralRefundDelay: { type: "blocks", value: 65535 },
    },
    {
        description: "minimum 512 second exit delay",
        player: keys.ex1,
        coordinator: keys.vhtlcSender,
        server: keys.vhtlcServer,
        refundLocktime: 500000000,
        exitDelay: { type: "seconds", value: 512 },
        unilateralRefundDelay: { type: "seconds", value: 1024 },
    },
    {
        description: "Mutinynet signer, timestamp locktime, maximum second refund delay",
        player: keys.vhtlcSender,
        coordinator: keys.ex1,
        server: keys.mutinynetSigner,
        refundLocktime: 1790000000,
        exitDelay: { type: "seconds", value: 2048 },
        unilateralRefundDelay: { type: "seconds", value: 33553920 },
    },
];

const relative = ({ type, value }) => ({ type, value: BigInt(value) });

function escrowLeaves({ player, coordinator, server, refundLocktime, exitDelay, unilateralRefundDelay }) {
    const [p, c, s] = [player, coordinator, server].map((key) => hex.decode(key));
    return [
        MultisigTapscript.encode({ pubkeys: [p, c, s] }),
        CLTVMultisigTapscript.encode({
            absoluteTimelock: BigInt(refundLocktime),
            pubkeys: [p, s],
        }),
        CSVMultisigTapscript.encode({ timelock: relative(exitDelay), pubkeys: [p, c] }),
        CSVMultisigTapscript.encode({ timelock: relative(unilateralRefundDelay), pubkeys: [p] }),
    ].map((tapscript) => tapscript.script);
}

const vectors = cases.map((input) => {
    const leaves = escrowLeaves(input);
    const vtxoScript = new VtxoScript(leaves);
    const server = hex.decode(input.server);
    return {
        ...input,
        expected: {
            leaves: leaves.map((leaf) => hex.encode(leaf)),
            leafTypes: leaves.map((leaf) => decodeTapscript(leaf).type),
            tweakedPublicKey: hex.encode(vtxoScript.tweakedPublicKey),
            tapTree: hex.encode(vtxoScript.encode()),
            address: new ArkAddress(server, vtxoScript.tweakedPublicKey, "tark").encode(),
        },
    };
});

process.stdout.write(
    JSON.stringify({ generator: `@arkade-os/sdk@${sdkVersion}`, vectors }, null, 2) + "\n",
);
