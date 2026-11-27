<!--
Run details for one repository on the Findings page.

1. Copy this file to runs/<repository>.md. Use the repository name without the owner: a-sit-plus/vck becomes vck.md.
2. Fill in what you know and delete the lines you do not.
3. Write plain Markdown. Avoid headings; use bold labels.

Without a file, or with only comments in it, the page shows no run details.
Only add information cleared for publication. Never add credentials or private reports.
-->

- **Model:** z.ai/glm5.3-flash
- **Cost:** about 12 USD
- **Agent:** `security-review`


**Notes**

The following prompt lead to finding about a dozen flaws in a development snapshot AFTER already fixing critical vulnerabilities found by Flawrence. Everything was fixed prior to publishing a release.  
Flawrence runs are now part of the release process to catch any bugs prior to actually preparing the final releases.

**Prompt**

> # Security Audit: VC-K 8.0.0 → develop
>
> ## Objective
>
> Perform a comprehensive, standards-aware security audit of the VC-K library, focusing primarily on **security vulnerabilities introduced between version 8.0.0 and the current `develop` branch**.
>
> - **Source code:** `vck/` (already checked out)
> - **Baseline:** Git tag `8.0.0`
> - **Target:** Current `develop` HEAD
> - **Standards and specifications:** `wallet-standards/`
>
> The highest priority is the **new validation and reporting architecture**, including trust establishment, cryptographic validation, validation policies, and acceptance decisions.
>
> Also review the library's core credential functionality and the relevant OpenID protocol implementations for security regressions and vulnerabilities.
>
> ## 1. Repository and architecture
>
> Start by reading `vck/AGENTS.md`, `ARCHITECTURE.md`, `README.md`, and `CHANGELOG.md`.
>
> Use Git history and `git diff 8.0.0..develop` to identify new and significantly modified functionality. Review the complete current implementation where necessary, including unchanged code involved in security-sensitive paths.
>
> Focus on:
>
> - `vck` — credential issuance, storage, presentation, verification, validation, and trust.
> - `vck-openid` — OpenID4VP, OpenID4VCI, OAuth2, OIDC, DCAPI, and protocol security.
> - `vck-openid-ktor` — HTTP integration and remote resource handling.
> - `openid-data-classes`, `etsi-data-classes`, `sd-jwt-type-metadata` — security-sensitive data models, serialization, and metadata processing.
> - Other modules where relevant to security-critical operations.
>
> Prioritize functionality introduced since 8.0.0 without excluding pre-existing vulnerabilities in the core functionality.
>
> ## 2. Primary focus: Validation and Reports
>
> Thoroughly review:
>
> `vck/src/commonMain/kotlin/at/asitplus/wallet/lib/validation/`
>
> Particularly:
>
> - `ValidationReport`, `ValidationRules`, `ValidationPolicy`
> - `CredentialValidator`, `CredentialEvaluation`
> - `PresentationValidation`, `PresentationEvaluation`
> - `RelyingPartyValidator`, `IssuerMetadataValidator`
> - `StatusValidator`, `TrustCheck`, `SignatureCheck`
> - `ClientAttestationValidator`, `KeyAttestationValidator`
> - Format-specific validation, required claims, and disclosures.
>
> Include legacy validators, compatibility APIs, and all consumers of validation results.
>
> ### 2.1 Validation decisions and report integrity
>
> Investigate whether invalid, untrusted, or malicious artifacts can be incorrectly accepted.
>
> Pay particular attention to:
>
> - Incorrect handling of `Passed`, `Failed`, `Blocked`, and `NotApplicable`
> - Required checks being skipped, bypassed, or accidentally treated as optional
> - Incorrect aggregation of nested reports or dependent checks
> - Inconsistent decisions between individual checks and the overall report
> - Missing prerequisites or unexpected exceptions causing fail-open behavior
> - Differences between the checks actually performed and those reported
> - Invalid artifacts being exposed through APIs as validated objects
> - Security properties being lost through compatibility wrappers or result conversions
>
> Examine `isIntactCredential()`, `unacceptedCredential`, `PreparedCredentialStorage`, `storeAccepted()`, and `storeAfterUserConsent()` for possible bypasses of mandatory validation.
>
> ### 2.2 ValidationPolicy
>
> Review the behavior and interaction of:
>
> - `TrustPolicy`
> - `StatusPolicy`
> - `HolderBindingPolicy`
> - `RequiredClaimsPolicy`
> - `RegistrationPolicy`
> - Timeliness enforcement and time leeway
>
> Verify that policy relaxations cannot inadvertently weaken cryptographic integrity, mandatory holder binding, proof validation, challenge/audience checks, or replay protection.
>
> Distinguish intentionally relaxed policies from actual vulnerabilities.
>
> ### 2.3 Trust and certificate validation
>
> Examine trust establishment for credential issuers, status list signers, wallet providers, relying parties, credential issuer metadata, and attestations.
>
> Include:
>
> - X.509 certificate validation, chain building, and trust-anchor selection
> - Certificate extensions, constraints, identifiers, and validity
> - WRPAC and WRPRC validation and linkage
> - LoTE processing and trust-anchor extraction
> - Certificate and signing-key identity matching
> - Trust decisions based on externally supplied versus authenticated identifiers
> - Attestation verification and proof-of-possession checks
>
> Consider whether an attacker can substitute identities, certificates, trust anchors, or credential types to obtain an incorrect trust decision.
>
> ### 2.4 Credential and presentation validation
>
> Review the security of:
>
> - SD-JWT VC, disclosures, digests, and key-binding JWTs
> - ISO mDoc/mDL, COSE signatures, issuer-signed items, device signatures, device MACs, and session transcripts
> - JWT-based verifiable credentials
> - Holder binding, nonce, audience, and challenge verification
> - Required claims, credential metadata, and credential type matching
> - Presentation binding and submission requirements
>
> Examine parsing ambiguities, duplicated or conflicting claims, malformed structures, and opportunities to bypass signature or integrity checks.
>
> ### 2.5 Status and timeliness
>
> Review Token Status List handling, including:
>
> - Signature and signer trust validation
> - Revocation/suspension interpretation
> - Expiration, freshness, and TTL
> - Status list retrieval and caching
> - Multiple status mechanisms and agreement
> - Missing status information and unavailable resolvers
> - Index calculations, decompression, and resource exhaustion
>
> ## 3. Core library and protocol security
>
> Review the complete credential lifecycle:
>
> **Issuance → Decoding → Validation → Storage → Selection → Presentation → Verification**
>
> Look for security regressions and weaknesses introduced by the changes since 8.0.0.
>
> ### OpenID4VP and DCAPI
>
> Examine:
>
> - Authorization request validation and verifier authentication
> - Request objects, signatures, encryption, and client identifier schemes
> - Nonce, state, audience, and replay protection
> - DCAPI origin binding and session transcripts
> - `direct_post`, `direct_post.jwt`, `dc_api`, and `dc_api.jwt`
> - Presentation validation and DCQL submission requirements
> - Credential disclosure to unauthorized relying parties
> - One-time request consumption and concurrent protocol processing
>
> ### OpenID4VCI and OAuth2
>
> Examine:
>
> - Authorization code and pre-authorized code flows
> - Credential request proofs and nonce handling
> - DPoP, client attestations, and proof of possession
> - Credential issuer metadata validation
> - Encrypted credential requests and responses
> - Credential type and holder-key binding
> - Security checks before storing issued credentials
> - State-machine transitions, retries, and replay protection
>
> ### Additional security boundaries
>
> Review security-sensitive serialization and parsing of JSON, JWT, JOSE, COSE, CBOR, URIs, and certificates.
>
> Inspect external resource retrieval, redirects, caching, concurrency, and exception handling, particularly where untrusted input crosses a trust boundary.
>
> ## 4. Standards verification
>
> Use the locally available specifications in `wallet-standards/`.
>
> Identify the applicable versions and compare the implementation against their normative security requirements.
>
> Relevant specifications include, where available:
>
> - OpenID4VP and OpenID4VCI
> - SD-JWT and SD-JWT VC
> - ISO/IEC 18013-5 and ISO/IEC 18013-7
> - OAuth2, DPoP, JOSE, COSE, and Token Status List
> - ETSI TS 119 475, TS 119 602, and relevant certificate/trust standards
> - EUDI ARF and applicable Implementing Acts
>
> Pay particular attention to security-relevant deviations that could allow unauthorized issuance, presentation, verification, or trust establishment.
>
> Reference exact specification versions and sections when identifying deviations. Consider applicable EUDI profiles and conflicts between standards.
>
> Do not assume that a standards violation necessarily constitutes an exploitable vulnerability.
>
> ## 5. Review priorities
>
> Prioritize findings in this order:
>
> 1. **Validation bypasses:** Invalid credentials, presentations, certificates, or protocol messages incorrectly accepted.
> 2. **Trust and identity confusion:** Incorrect issuer/verifier authentication, trust-anchor selection, certificate validation, or identity binding.
> 3. **Cryptographic and protocol weaknesses:** Signature verification, key binding, nonce/state handling, replay protection, and encryption.
> 4. **Reporting inconsistencies:** Incorrect outcomes, misleading reports, or invalid objects exposed as trusted.
> 5. **Security regressions:** Behavior introduced or weakened since 8.0.0.
> 6. **Other vulnerabilities:** Parser ambiguities, resource exhaustion, unsafe retrieval, and concurrency issues.
>
> Use existing tests to understand intended behavior, but independently examine whether security invariants are enforced.
>
> For suspected vulnerabilities, trace the relevant production code and verify exploitability wherever feasible. Include precise code references and relevant standards requirements.
>
> **Focus on concrete security weaknesses rather than general code quality, stylistic issues, or speculative hardening recommendations.**



