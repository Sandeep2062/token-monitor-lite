# Updater signing design (not implemented)

Status: **design only**. The updater currently trusts the release asset's
SHA-256 digest as delivered by the GitHub Releases API (VERIFIED, see
`src/update.rs`). That binds the download to the release record, not to a
project-held signing key. This document specifies an ed25519 signature over
`SHA256SUMS` so a compromised release record, token, or mirror cannot produce a
runnable update.

## Threat model

Protected against:

- A tampered or forged release asset hosted from a GitHub-controlled host.
- A compromised GitHub token or release record that can replace `digest` and
  `browser_download_url` values.
- A redirect to a non-GitHub host (already blocked by the transport allowlist).
- A swap of the downloaded file between verification and execution (already
  mitigated by the install-time re-hash).

Not protected against:

- Compromise of the offline signing key.
- A malicious build produced from the tagged source by CI (signing happens
  after the build, so the signature attests to bytes, not provenance).
- Local malware running as the same user, which can read or replace private
  update files.

## Artifacts

Each release publishes, alongside the platform artifacts:

| Asset | Meaning |
| --- | --- |
| `SHA256SUMS` | UTF-8/ASCII lines: `<64 lowercase hex><two spaces><name>\n` |
| `SHA256SUMS.ed25519` | Detached signature over the **exact bytes** of `SHA256SUMS` |

The signature file layout:

```
offset 0   : 4 bytes  key id, big-endian u32
offset 4   : 64 bytes ed25519 signature
offset 68  : end of file
```

- The signature is ed25519 (RFC 8032) over the raw byte string of
  `SHA256SUMS` — no re-encoding, newline normalization, or JSON wrapping.
- The key id identifies which embedded public key must verify. It is not
  secret; it makes rotation unambiguous.
- `SHA256SUMS.ed25519` is size-bounded to 68 bytes; `SHA256SUMS` is already
  bounded to 64 KiB by `MAX_SUMS_BYTES` (VERIFIED, `src/update.rs`).

## Embedded public keys

A compile-time table in `src/update.rs`:

```rust
/// (key_id, 32-byte ed25519 public key)
const UPDATE_TRUSTED_KEYS: &[(u32, [u8; 32])] = &[
    (1, [/* current release key */]),
];
```

Rules:

- The private key is generated offline (hardware token or air-gapped host) and
  never enters CI, the repository, or the build cache.
- Adding a new key is a normal release that embeds both the new and the
  previous key. Removing a key requires a release that no longer accepts it and
  a documented cut-off version.
- Key ids are monotonically increasing and never reused.
- The embedded key table is covered by the existing catalog/integrity tests in
  addition to signature-vector tests.
- Rotation procedure: generate key pair offline → embed new public key in an
  unsigned-but-digest-verified release → sign subsequent releases with the new
  key → retire the old key after the published minimum supported version.

## Verification flow

Order matters; any failure aborts the update and fails closed.

1. `check_for_update` fetches the release metadata from `api.github.com` through
   the redirect-restricted transport (VERIFIED: only `github.com`, its
   GitHub-owned subdomains, and the two GitHub asset hosts
   `objects.githubusercontent.com` / `release-assets.githubusercontent.com`
   are accepted, over HTTPS on port 443).
2. The updater resolves the `SHA256SUMS` and `SHA256SUMS.ed25519` assets, but
   **before trusting either**, downloads them with size bounds.
3. Verify the signature file: length exactly 68 bytes, key id present in
   `UPDATE_TRUSTED_KEYS`, ed25519 `verify_strict(public_key, SHA256SUMS_bytes,
   signature)` succeeds.
4. Parse `SHA256SUMS` strictly:
   - Only lines matching `^[0-9a-f]{64}  [A-Za-z0-9._-]+$`.
   - Reject duplicate names (ambiguous first-match is not acceptable).
   - Reject a file with zero matching rows, invalid UTF-8, or oversized input.
5. Select the one row for the platform asset. Absent multiple times → abort.
6. Download the artifact, bound to `MAX_UPDATE_BYTES`, and compare its SHA-256
   with the signed row. The API-supplied `digest` field remains a second,
   independent check.
7. Record the verified digest next to the artifact (existing behavior,
   VERIFIED: `<asset>.sha256`) so installation can re-hash immediately before
   execution.
8. Installation re-hashes (Windows/macOS) or copies into a fresh 0700
   directory and hashes the copy (Linux) before launching. Any mismatch aborts
   without execution (VERIFIED behavior added alongside this design).

The API `digest` field is never sufficient on its own once signing is enabled;
a release without both signature assets is treated as **not installable**, not
as unsigned-but-acceptable.

## Compatibility and rollout

- Signing lands behind no feature flag. Releases that predate the rollout are
  naturally excluded because the profile only offers updates for newer
  versions, and the first signed release becomes the minimum installable
  version.
- Rollback protection: the existing `version <= current` check already refuses
  downgrades (VERIFIED, `src/update.rs`); the signed manifest does not weaken
  it.
- Offline/slow networks: signature verification is local and adds no request.
- Failure messaging should distinguish "signature missing/invalid" from
  "network error"; both keep the current version running.

## Implementation plan (future change, not in this commit)

1. Add a pinned `ed25519-dalek` dependency (exact version, `verify_strict`,
   no default features beyond `std`). Verify the license and checksum on
   `pixvault-vm` before pinning.
2. Extend `UpdateCheck` with `signature: Option<UpdateAsset>` and
   `UpdateError` with `SignatureMissing` and `SignatureInvalid`.
3. Add `verify_manifest_signature(manifest: &[u8], signature: &[u8]) -> Result<(), UpdateError>`
   plus a strict `parse_sums` that returns a single digest per name.
4. Require the signature pair in `download_update`; remove the
   `checksum_from_sums` fallback that accepts an unsigned `SHA256SUMS`.
5. Sign in the release workflow: after artifacts and `SHA256SUMS` are produced,
   run the offline-key signing step on a protected environment, upload
   `SHA256SUMS.ed25519`, then publish.
6. Tests:
   - RFC 8032 test vector verifies with the embedded-format parser.
   - Tampered manifest byte, truncated 67-byte signature, unknown key id,
     duplicate sums rows, and a valid signature for a different manifest all
     fail closed.
   - A correctly signed manifest selects the expected digest.
   - Existing checksum/digest and install re-hash tests stay green.
7. Migration note for users: the first signed release must be built only after
   the public key is embedded in the *previous* installed version; otherwise
   that previous version will reject the first signed release. Ship the key
   table one release before enforcing signatures.

## Verified inputs used by this design

- VERIFIED: ureq 3.5.2 defaults are `max_redirects = 10`,
  `https_only = false`, `http_status_as_error = true` (read from
  `~/.cargo/registry/src/.../ureq-3.5.2/src/config.rs` on `pixvault-vm`).
- VERIFIED: `max_redirects = 0` returns the 3xx response as-is
  (`src/run.rs`, `max_redirects_do_error()` requires `max_redirects > 0`).
- VERIFIED: the updater now restricts every request and redirect hop to
  `github.com` (and its subdomains), `objects.githubusercontent.com`, or
  `release-assets.githubusercontent.com` over HTTPS on port 443.
- VERIFIED: as of 2026-11-13, a live release-asset request on `pixvault-vm`
  returned `302` with `Location:
  https://release-assets.githubusercontent.com/github-production-release-asset/...`;
  the historical `objects.githubusercontent.com` host is retained for
  compatibility. A future host change requires updating the allowlist and this
  document (ASSUMED residual risk).
