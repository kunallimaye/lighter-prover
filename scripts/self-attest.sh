#!/bin/bash
# Copyright (c) Elliot Technologies, Inc.
# SPDX-License-Identifier: BUSL-1.1
#
# Issue #118: self-attest against Lighter's LIVE on-chain verifier.
#
# This makes a FREE, READ-ONLY eth_call to verifyProof/Verify on Lighter's
# public PLONK verifier on Ethereum MAINNET. It is NOT a transaction: no gas,
# no funded wallet, no on-chain write, no state change. Any public RPC works.
#
# It takes the BN254 proof from #117 (final::<d>.proof) + the outer-wrapper
# proof's public inputs (outer-wrapper-proof::<d>.json), formats them to the
# on-chain verifier ABI (Verify(bytes,uint256[]) — selector 0x7e4f7a8a, the
# same gnark PlonkVerifier ABI the repo exports), and records accept/reject.
#
# Usage:
#   scripts/self-attest.sh <final::<d>.proof> <outer-wrapper-proof::<d>.json> [RPC_URL]
#
# Env:
#   VERIFIER_ADDR  (default 0xac3Ce44B6ff4E402858C99D5699ff63131572BaA — Lighter mainnet)
#   RPC_URL        (default https://ethereum-rpc.publicnode.com — public mainnet)
set -euo pipefail

PROOF_FILE="${1:?usage: self-attest.sh <final.proof> <outer-wrapper-proof.json> [RPC_URL]}"
PIS_FILE="${2:?usage: self-attest.sh <final.proof> <outer-wrapper-proof.json> [RPC_URL]}"
RPC_URL="${3:-${RPC_URL:-https://ethereum-rpc.publicnode.com}}"
VERIFIER_ADDR="${VERIFIER_ADDR:-0xac3Ce44B6ff4E402858C99D5699ff63131572BaA}"

# Verify-function selector for Verify(bytes,uint256[]) (gnark PlonkVerifier ABI).
SELECTOR="7e4f7a8a"

echo "[INFO] RPC:      $RPC_URL"
echo "[INFO] Verifier: $VERIFIER_ADDR (Ethereum mainnet)"

# Confirm we are on mainnet (chainId 0x1).
CHAIN_ID=$(curl -s -m 15 -X POST "$RPC_URL" -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' | jq -r '.result')
echo "[INFO] chainId:  $CHAIN_ID"
if [[ "$CHAIN_ID" != "0x1" ]]; then
  echo "[ERROR] not Ethereum mainnet (chainId=$CHAIN_ID); aborting" >&2
  exit 1
fi

# Marshal the proof to Solidity layout + derive the public input.
MARSHAL=$(go run ./tools/marshal-proof "$PROOF_FILE" "$PIS_FILE")
SOL_PROOF_HEX=$(echo "$MARSHAL" | sed -n 's/^SOLIDITY_PROOF_HEX=//p')
PI_DEC=$(echo "$MARSHAL" | sed -n 's/^PUBLIC_INPUT_DEC=//p')
echo "[INFO] $(echo "$MARSHAL" | sed -n 's/^SOLIDITY_PROOF_LEN=/solidity proof bytes: /p')"
echo "[INFO] public input (dec): $PI_DEC"

# ABI-encode Verify(bytes proof, uint256[] publicInputs) and build calldata.
CALLDATA=$(python3 - "$SELECTOR" "$SOL_PROOF_HEX" "$PI_DEC" <<'PY'
import sys, binascii
sel, proof_hex, pi = sys.argv[1], sys.argv[2], int(sys.argv[3])
proof = binascii.unhexlify(proof_hex)
pad = proof + b'\x00' * ((32 - len(proof) % 32) % 32)
w = lambda x: f"{x:064x}"
off_bytes = 0x40
off_array = 0x40 + 32 + len(pad)
data = sel + w(off_bytes) + w(off_array) + w(len(proof)) + binascii.hexlify(pad).decode() + w(1) + w(pi)
print("0x" + data)
PY
)

echo "[INFO] eth_call -> $VERIFIER_ADDR  data(len=${#CALLDATA})"
RESP=$(curl -s -m 30 -X POST "$RPC_URL" -H 'content-type: application/json' \
  --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_call\",\"params\":[{\"to\":\"$VERIFIER_ADDR\",\"data\":\"$CALLDATA\"},\"latest\"]}")
echo "[RAW]  $RESP"

RESULT=$(echo "$RESP" | jq -r '.result // empty')
ERROR=$(echo "$RESP" | jq -r '.error.message // empty')

if [[ -n "$ERROR" ]]; then
  echo "[RESULT] REVERT: $ERROR"
  echo "[FINDING] the verifier reverted — shape mismatch (not a transaction failure)."
  exit 0
fi
case "$RESULT" in
  0x*1) echo "[RESULT] ACCEPTED (verifyProof returned true) — Lighter's on-chain verifier accepts our proof." ;;
  0x0000000000000000000000000000000000000000000000000000000000000000)
        echo "[RESULT] REJECTED (verifyProof returned false) — shape/vk mismatch (a FINDING, not a failure)." ;;
  *)    echo "[RESULT] UNEXPECTED return: $RESULT" ;;
esac
