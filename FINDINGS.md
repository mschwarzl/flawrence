# Findings

This page lists vulnerabilities found with Flawrence.
We only list a finding once the maintainers have fixed it or published an advisory.
When maintainers do not respond within 90 days, we publish the report under [disclosures](disclosures).

If Flawrence found a bug in your project, please open a pull request and add it here.

## a-sit-plus/vck

- [GHSA-xhc7-2hfg-fx98](https://github.com/a-sit-plus/vck/security/advisories/GHSA-xhc7-2hfg-fx98). ISO mdoc issuer data element tampering is accepted. The digest mismatch is computed but not enforced.
- [GHSA-fvf5-9qvq-h7m6](https://github.com/a-sit-plus/vck/security/advisories/GHSA-fvf5-9qvq-h7m6). JWS and COSE signature verification trusts keys taken from the token itself.
- [GHSA-pjhf-m9gg-49m4](https://github.com/a-sit-plus/vck/security/advisories/GHSA-pjhf-m9gg-49m4). OID4VCI key attestation is verified against a key from the attestation's own header.
- [GHSA-8m3x-xjm3-w8xv](https://github.com/a-sit-plus/vck/security/advisories/GHSA-8m3x-xjm3-w8xv). OID4VCI credential request replay. The proof of possession nonce is verified but not consumed.
- [GHSA-85fc-jv65-v886](https://github.com/a-sit-plus/vck/security/advisories/GHSA-85fc-jv65-v886). OpenID4VP presentation response replay. The authentication request and its nonce are not consumed.
- [GHSA-62h6-459f-h72j](https://github.com/a-sit-plus/vck/security/advisories/GHSA-62h6-459f-h72j). SD-JWT strict verification gaps. Duplicate claims, unreferenced disclosures and missing KB-JWT `iat` and `typ` checks are accepted.

## a-sit-plus/warden-supreme

- [GHSA-frpv-cj76-xm4r](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-frpv-cj76-xm4r). The KeyMaster and KeyMint security level from the certificate chain is not asserted.
- [GHSA-4r28-jj7j-g7v8](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-4r28-jj7j-g7v8). Hash-colliding attestation digests cause CPU denial of service before validation.
- [GHSA-3chr-q239-rmpp](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-3chr-q239-rmpp). Challenge cache pruning takes quadratic CPU time.
- [GHSA-qv7m-wr3r-hfg3](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-qv7m-wr3r-hfg3). Nested hybrid JSON bypasses the depth guard and overflows the stack before attestation.
- [GHSA-3qj3-8wvm-fmcp](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-3qj3-8wvm-fmcp). Unbounded recursion in `AttestationChallenge.additionalPayload` crashes any client that fetches a challenge.
- [GHSA-744h-w68v-qfpc](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-744h-w68v-qfpc). Enabling software attestation next to hardware attestation downgrades verification to the weaker engine.
- [GHSA-2f9g-97q5-f2wr](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-2f9g-97q5-f2wr). iOS App Attest environment confusion applies the wrong security policy.
- [GHSA-rxrw-2p38-wfmr](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-rxrw-2p38-wfmr). The parser accepts duplicate and disagreeing `AuthorizationList` elements.
- [GHSA-6jpm-5g92-cpcg](https://github.com/a-sit-plus/warden-supreme/security/advisories/GHSA-6jpm-5g92-cpcg). Duplicate CSR attributes are not rejected.

## a-sit-plus/signum

- [GHSA-hch5-9pjg-jqjh](https://github.com/a-sit-plus/signum/security/advisories/GHSA-hch5-9pjg-jqjh). Unbounded recursion when parsing, encoding, comparing and hashing deeply nested `Asn1Element` structures.

## a-sit-plus/awesn1

- [GHSA-48c5-x28r-fff9](https://github.com/a-sit-plus/awesn1/security/advisories/GHSA-48c5-x28r-fff9). Quadratic time when converting large ASN.1 INTEGER and OID values to decimal.
- [GHSA-q34j-33q7-fw9h](https://github.com/a-sit-plus/awesn1/security/advisories/GHSA-q34j-33q7-fw9h). Quadratic time and memory when parsing nested encapsulating OCTET STRINGs.
- Multiple other findings assessed as not being security-critical being addressed through regular pull requests.

## vercel-labs/just-bash

- [GHSA-g5pf-mh4h-h655](https://github.com/vercel-labs/just-bash/security/advisories/GHSA-g5pf-mh4h-h655). Sandbox escape from `ReadWriteFs` through pre-existing hard links.

## vllm-project/vllm

- [GHSA-vfp2-c8pq-v6h6](https://github.com/vllm-project/vllm/security/advisories/GHSA-vfp2-c8pq-v6h6). SSRF through the `X-Request-Id` header drives the P2pNcclConnector and leaks KV cache tensors.
- [GHSA-mm38-5g96-7j6x](https://github.com/vllm-project/vllm/security/advisories/GHSA-mm38-5g96-7j6x). The Nemotron-VL processors turn off the Pillow decompression bomb guard for the whole process. This allows unauthenticated denial of service.
- [GHSA-w9r6-29q4-pvpr](https://github.com/vllm-project/vllm/security/advisories/GHSA-w9r6-29q4-pvpr). Unvalidated `stop_token_ids` crash the engine and cause an out-of-bounds GPU write.

## vbpf/prevail

- [GHSA-2qc8-qh94-66rc](https://github.com/vbpf/prevail/security/advisories/GHSA-2qc8-qh94-66rc) (CVE-2026-53670). A typeset with more than one type in `add()` skips the offset update. Out-of-bounds accesses pass eBPF verification.
- [GHSA-65rv-h458-cq99](https://github.com/vbpf/prevail/security/advisories/GHSA-65rv-h458-cq99) (CVE-2026-53671). A context write in `do_mem_store` is a no-op. Unsafe eBPF programs pass verification.
- [GHSA-65fp-5qmq-gg2m](https://github.com/vbpf/prevail/security/advisories/GHSA-65fp-5qmq-gg2m) (CVE-2026-53706). ALU32 pointer arithmetic is accepted without a 64-bit check. The verifier passes programs that corrupt pointers.

## android/keyattestation

- [GHSA-fwcv-9wq2-cg54](https://github.com/android/keyattestation/security/advisories/GHSA-fwcv-9wq2-cg54). The StrongBox security level from the certificate chain is never evaluated.
- [GHSA-3hrv-cc7c-cmjx](https://github.com/android/keyattestation/security/advisories/GHSA-3hrv-cc7c-cmjx). Unbounded hash set construction in `AttestationApplicationId` causes CPU denial of service before validation.

## Bouncy Castle

- [Contributors list](https://www.bouncycastle.org/engage/contributors/). Lenient ASN.1 UTCTime and GeneralizedTime parsing accepted structurally malformed content. The report included test cases from fuzzing, and further fuzzing covered date validity. Initial discovery was made manually, follow-up findings on a believed-to-be fixed state were discovered using Flawrence.

## apache/superset

- 13 reports. The maintainers did not respond properly within 90 days. Eight are fixed on master. See [disclosures/superset](disclosures/superset/README.md).

## trinodb/trino

- 15 advisories. The maintainers did not respond within 90 days. None is fixed. See [disclosures/trino](disclosures/trino/README.md).

## locationtech/jts

- XXE in `KMLReader`. The maintainers did not respond within 90 days. Fixed on master, not released. See [disclosures/jts](disclosures/jts/README.md).

## cloudflare/workerd

Flawrence discovered several memory corruption vulnerabilities such use after free bugs in the I/O and file system code.
It discovered logic bugs and regressions introduced an patches and new features.

## quickjs-ng/quickjs

- [Issue 1660](https://github.com/quickjs-ng/quickjs/issues/1660). Use after free in `Promise.withResolvers()` cleanup after an out of memory error.
- [Issue 1661](https://github.com/quickjs-ng/quickjs/issues/1661). Use after free in `AsyncDisposableStack` with a reject handler that takes no arguments.
- Stale typed array length after a resizable ArrayBuffer shrinks. Not fixed. See [disclosures/quickjs-ng](disclosures/quickjs-ng/README.md).

## saghul/txiki.js

- Out-of-bounds write through `FileHandle.read()`. Not fixed. See [disclosures/txiki.js](disclosures/txiki.js/README.md).

## vercel-labs/scriptc

- Three reports through HackerOne. See [disclosures/scriptc](disclosures/scriptc/README.md).
