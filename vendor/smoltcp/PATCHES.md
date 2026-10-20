# Local smoltcp patch

This is the source, build manifest, examples, and tests from the crates.io
`smoltcp` 0.13.1 release, under its included 0BSD license. `.cargo_vcs_info.json`
identifies the upstream source revision.

The only source change is in `src/iface/interface/ipv6.rs`:
`Interface::sync_slaac_state` treats non-IPv6 routes as non-conflicting when
installing a newly advertised IPv6 default route (`_ => true`).
Otherwise an existing IPv4 default route silently prevents installation of
the SLAAC gateway, breaking off-link IPv6 connections on dual-stack networks.

Upstream references:

- [Issue #1191](https://github.com/smoltcp-rs/smoltcp/issues/1191)
- [Proposed fix #1206](https://github.com/smoltcp-rs/smoltcp/pull/1206)

The regression test is `crates/han-config/tests/ipv6_stack.rs`, run with
`cargo test --locked -p han-config --test ipv6_stack` from the workspace root.
It injects an actual router advertisement after IPv4 is configured, verifies
all three interface addresses coexist, and checks both default gateways.
It fails against the unmodified 0.13.1 release.

Remove this patch and the root `[patch.crates-io]` entry when the Embassy
dependency can use a released smoltcp version containing the fix.
