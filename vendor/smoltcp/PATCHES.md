# Local smoltcp patch

This is the source, build manifest, examples, and tests from the crates.io
`smoltcp` 0.13.1 release, under its included 0BSD license. `.cargo_vcs_info.json`
identifies the upstream source revision.

The gateway installation fix is in `src/iface/interface/ipv6.rs`:
`Interface::sync_slaac_state` treats non-IPv6 routes as non-conflicting when
installing a newly advertised IPv6 default route (`_ => true`).
Otherwise an existing IPv4 default route silently prevents installation of
the SLAAC gateway, breaking off-link IPv6 connections on dual-stack networks.

Router discovery is also patched in `src/iface/slaac.rs` and the interface
polling/IPv6 code. Unanswered solicitations now continue with the exponential
backoff and jitter specified by [RFC 7559](https://datatracker.ietf.org/doc/html/rfc7559)
(initial interval about 4 seconds, maximum about 1 hour). Previously discovery
stopped after three requests, leaving the device dependent on unsolicited
advertisements. A zero-lifetime router advertisement no longer stops retries.

The interface combines socket and SLAAC deadlines by taking the earliest
present deadline. `Option::min` previously treated `None` as earlier than a
real deadline, losing the SLAAC timer on an idle network or a socket timer
when SLAAC had none. This fix adds no heap allocations or extra sockets.

Upstream references:

- [Issue #1191](https://github.com/smoltcp-rs/smoltcp/issues/1191)
- [Proposed fix #1206](https://github.com/smoltcp-rs/smoltcp/pull/1206)

The regression test is `crates/han-config/tests/ipv6_stack.rs`, run with
`cargo test --locked -p han-config --test ipv6_stack` from the workspace root.
It injects an actual router advertisement after IPv4 is configured, verifies
all three interface addresses coexist, and checks both default gateways.
It fails against the unmodified 0.13.1 release.

`crates/han-config/tests/ipv6_discovery.rs` drops the first three router
solicitations, then verifies successful discovery after a fourth request
without restarting. It also covers idle scheduling, bounded backoff,
zero-lifetime advertisements, and socket deadlines without SLAAC timers.
Run both packet-level suites with
`cargo test --locked -p han-config --test ipv6_stack --test ipv6_discovery`.

Remove this patch and the root `[patch.crates-io]` entry when the Embassy
dependency can use a released smoltcp version containing these fixes.
