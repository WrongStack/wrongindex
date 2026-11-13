# SignPath Foundation application draft

Preparation only; not submitted, approved or activated. Do not infer a signing entitlement from the checked-in workflow/configuration.

| Field | Prepared value / evidence |
|---|---|
| Project | WrongIndex |
| Repository / homepage | https://github.com/WrongStack/wrongindex |
| License | MIT; `LICENSE`, Cargo metadata and the generated third-party inventory accompany builds |
| Downloads | https://github.com/WrongStack/wrongindex/releases |
| Existing release | v0.1.9 Windows installer; downloaded checksums and GitHub provenance verified in the local release-continuation evidence |
| Application purpose | Local codebase indexing service for WrongStack: one per-user Rust daemon, a command-line client and separate desktop clients. Direct adapter and existing WrongStack MCP surfaces use local authenticated IPC |
| Supported preparation platform | Windows x64; additional-platform certification is not claimed |
| Signing scope | Four own program images, NSIS-generated uninstaller and outer NSIS installer; no upstream executables signed as this project |
| Maintainer / proposed approver | GitHub @ersinkoc, authenticated repository administrator |
| Code signing policy | `docs/code-signing-policy.md`; public main-page link becomes usable after this preparation is reviewed and published |
| Build provenance | GitHub-hosted Windows workflow, exact action/CLI pins, artifact-origin verification, source/hash/Authenticode validation |
| Contact | Use the owner's approved contact during the external application; no contact email is published in this source draft |
| MFA | GitHub MFA confirmed by the owner on 2026-11-12; SignPath account/MFA setup remains pending |
| Adoption / user count | Not measured; do not invent install counts, endorsements or project reputation |

Suggested project description:

> WrongIndex maintains local WrongStack-compatible SQLite code indexes across multiple checkouts. Its native daemon owns and watches each admitted index, provides bounded local IPC, and survives client/GUI exits. The project ships a Windows per-user installer and CLI with a separate desktop interface. Indexing does not execute the indexed project's scripts or build hooks. The current GPUI expansion is an unreleased preview, with explicit compatibility limits and release checks documented in the repository.

Application entry point: [SignPath Foundation](https://signpath.org/apply). Its current form is embedded by HubSpot; the read-only HTML is retained in the local preparation evidence. No form request has been submitted and no subscription/GitHub App installed.

Before submission: review the public policy/role claims and contact details, complete local validation, and ensure the intended policy/workflow links are publicly reachable. GitHub MFA is already confirmed by the owner; provider account/MFA is a separate onboarding step. A provider decision is external; the project must continue labelling unsigned and unqualified builds accurately while it is pending.

## Exact published application form

The published form behind [the official application page](https://signpath.org/apply) was read without submitting it on 2026-11-12. Its public definition is retained at `target/signpath-20261112/application-public-definition.json`. The description field asks for a stable project summary rather than version-specific features. The form creates a SignPath account and requires an account email plus two owner consents; GitHub MFA confirmation does not provide those consents.

| Form field | Concrete draft / remaining boundary |
|---|---|
| Project Name | WrongIndex |
| Repository URL / Homepage URL | https://github.com/WrongStack/wrongindex |
| Tagline | A local codebase indexing service for consistent search across development projects. |
| Category | Developer Tools (`developer-tools`) |
| Description | WrongIndex is a local codebase indexing service for developer tools, with a persistent daemon, command-line client and desktop interface. It maintains searchable indexes across multiple project checkouts and connects WrongStack clients through authenticated local IPC. Indexing reads source files without executing each project's scripts or build hooks. |
| Reputation | Public MIT repository and versioned Windows releases: https://github.com/WrongStack/wrongindex/releases. GitHub API metadata at 2026-11-12 17:33 UTC reports 10 public releases, 118 total release-asset downloads, zero stars and zero forks. Downloads include maintainer/CI verification and do not establish unique users or independent adoption. No media coverage, endorsements or broad adoption are claimed. The project is part of the public WrongStack ecosystem; provider acceptance remains discretionary. |
| Build System | GitHub Actions (`GitHub`) |
| First Name / Last Name | Owner account name; confirm for the SignPath account before submission |
| Email | Owner reply pending; this is the SignPath account and notification address, not a public source-file value |
| Primary Discovery Channel | AI / LLM tools; this application route was introduced during assistant research |
| Exact discovery source (optional) | ChatGPT / Codex research |
| Maintainer Type / Company (optional) | Leave blank unless the owner chooses a representation; the GitHub organization name does not establish a legal entity |
| Download URL (optional) | Leave blank at this pending stage: its help text requires Foundation signing attribution. The current unsigned release page is linked as factual release history above, without claiming signing is provided |
| Privacy Policy URL (optional) | Do not supply an unpublished link or imply review of data practices is complete. The local service and optional WrongStack/provider boundaries are described in the prepared signing policy |
| Wikipedia URL (optional) | Blank; no article asserted |
| Personal-data storage/processing consent (required) | Owner confirmation pending; [provider privacy policy](https://signpath.io/privacy-policy) |
| Foundation Code of Conduct / certificate identity acknowledgement (required) | Owner confirmation pending; [Foundation terms](https://signpath.org/terms). The form requires acknowledgement of Foundation-issued certificate identity and possible revocation for violations |
| Other marketing communications (optional) | Unchecked |

Reputation evidence: `reputation-repository.json`, `reputation-releases.json` and `reputation-summary.json` under `target/signpath-20261112/`. The counters are dated observations, not a promise of eligibility or acceptance. No checkbox was accepted and no account was created by inspecting this public form.

## CI provisioning after acceptance

Import `.signpath/payloads-v1.xml`, `uninstaller-v1.xml` and `installer-v1.xml` as versioned artifact configurations. Set public repository variables `SIGNPATH_ORGANIZATION_ID`, `SIGNPATH_PROJECT_SLUG`, `SIGNPATH_POLICY_SLUG`, and the approved certificate's SHA-256 DER fingerprint `SIGNPATH_CERTIFICATE_SHA256`. Put the submitter API credential in the protected repository secret `SIGNPATH_API_TOKEN`, never in chat, scripts or command arguments. Enable the provider's required trusted GitHub build/origin configuration and approval roles.

Run `signpath-preview.yml` manually from canonical `main`. Program images are signed first; a bounded compiler courier exposes the generated uninstaller and final installer as separate workflow artifacts for provider approval. Already-signed program images are not patched again. Final checksums, manifests and attestations describe the signed bytes.

The five upstream NSIS plugins are not submitted. Their exact hashes come from `.signpath/nsis-vendor.json`, generated from the two official release assets by `export-nsis-vendor.ps1`. The pinned CLI/bundler versions and allowed plugin paths must be reviewed together when upgrading the toolchain. Unknown or changed plugins fail the compiler hook instead of being signed under the project's identity.

After the exact downloaded artifact passes remaining release gates, set `WRONGINDEX_QUALIFIED_SIGNING_RUN_ID` to that run ID and invoke `publish-signed.yml` with the same ID. Publication verifies source workflow/commit, artifact hashes, signatures and attestations and creates the version tag at that source commit without rebuilding or signing again. No variable, secret, workflow dispatch or release has been created by this draft.
