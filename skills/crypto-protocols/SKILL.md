---
name: crypto-protocols
description: "Cryptographic and protocol verification pitfalls when reviewing crypto libraries, token formats (JWS/JWT/COSE/SD-JWT/OID4V*, ASN.1/X.509) and hand-rolled protocols. Load when the target implements or verifies signatures, encryption, attestation, or secure protocols."
---

# Cryptography and protocol verification

When the target implements or verifies cryptographic protocols, check for:

## Signature and MAC verification
- Algorithm confusion (RS256 vs HS256, `none` algorithm accepted, alg taken from the token header)
- Signature computed but not enforced (digest mismatch ignored, verify return value unchecked)
- Keys taken from the token/attestation itself instead of a trusted store (self-attestation)
- Missing domain separation, wrong padding mode, no constant-time comparison (timing side channels)

## Nonce / replay
- Nonce or challenge verified but never consumed (replay of the same request/credential)
- Nonce generated with insufficient entropy or a predictable source
- Missing expiry (`exp`/`iat`/`nbf`) checks, or clock skew windows that are too generous

## Token formats
- Duplicate claims with disagreeing values accepted (JSON member dedup)
- Unreferenced disclosures accepted (SD-JWT), missing KB-JWT `iat`/`typ` checks
- Undisclosed/optional fields that verifiers silently default to safe values
- Key attestation verified against a key from the attestation's own header
- Security level of a certificate chain not asserted (KeyMaster/KeyMint downgrade)

## Parsing robustness (DoS)
- Unbounded recursion on deeply nested structures (ASN.1, JSON, XML) — stack overflow before validation
- Quadratic time/memory in parsing, conversion or cache pruning on attacker-controlled sizes
- Hash-colliding inputs causing CPU DoS before validation

## When reviewing
- Trace every verify path end to end: where the key comes from, what is signed, what is checked, what is ignored.
- Prefer a concrete byte-level PoC (crafted token/frame) over "the code looks wrong".
