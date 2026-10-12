# Offline CI prerequisites

Jobs are manual and require dedicated pre-provisioned runners labelled
`shoes-offline-linux` or `shoes-offline-macos`. No runner was deployed by this change.
GitHub runner control traffic is distinct from build/test traffic.
Administrators must deny job network egress (including all VPS access) while allowing
loopback for local protocol tests. Do not place SSH keys or production credentials
on these runners. Use isolated disposable runners, not the production server.

Before a run, provision the exact requested commit into `GITHUB_WORKSPACE` outside
the offline job. Jobs verify `HEAD == GITHUB_SHA` and reject staged or unstaged
changes to tracked files; they never fetch or check out code. This guard does not
inspect ignored or untracked files. Provision a clean disposable workspace and
trusted external caches; a clean Git status alone is not a supply-chain sandbox.
Provision Rust, rustfmt, Clippy, locked Cargo dependencies, target libraries,
cross and its container images, a verified sing-box test peer, and platform tools.
Export `SHOES_TEST_SING_BOX_BIN` to the preinstalled peer path.
Missing tools or caches must fail the job, not trigger downloads.

For mobile jobs, preinstall Xcode/cbindgen or Android SDK/NDK, Java, cargo-ndk,
Gradle and cached Gradle dependencies. Run builds with an egress-denied sandbox.
`--offline` is a dependency policy, not a complete network sandbox.

Artifacts remain on the runner. Automatic artifact upload and release publication
are intentionally absent; transfer/publishing requires a separate approved process.
No workflow deploys to, tunes, updates, reboots or contacts the VPS.

## Administrator verification checklist

Before approving a runner for use:

- Record the effective egress-denial policy for the actual job sandbox, including
  host networking, proxies, IPv4/IPv6 and DNS. Separately isolate runner control traffic.
- Use an administrator-owned non-VPS test endpoint to prove connection attempts
  fail from the job sandbox. Never probe the VPS as an isolation test.
- Verify container and Docker-daemon egress as well. Preloaded images and
  `--network=none` do not by themselves prevent daemon-side image downloads.
- Verify that loopback TCP and UDP protocol tests still work.
- Record the sandbox identity, policy version, timestamp and aggregate result,
  without credentials, raw configurations or packet contents.

This checklist is not run by the workflows. Runner provisioning and live isolation
checks require a separate scoped approval; they were not performed by this change.
