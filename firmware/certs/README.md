The firmware trusts these public Let's Encrypt roots for VictoriaLogs HTTPS:

- [ISRG Root X1](https://letsencrypt.org/certs/isrgrootx1.pem), SHA-256 `96BCEC06264976F37460779ACF28C5A7CFE8A3C0AAE11A8FFCEE05C0BDDF08C6`.
- [ISRG Root X2](https://letsencrypt.org/certs/isrg-root-x2.pem), SHA-256 `69729B8E15A86EFC177A57AFB7171DFC64ADD28C2FCA8CF1507E34453CCB1470`.

Downloaded from the [CA's published certificate list](https://letsencrypt.org/certificates/) on 2026-10-27. These are public trust anchors, not client credentials or pinned leaf certificates. Servers must provide their intermediate certificates. Review and update this bundle when Let's Encrypt changes its trusted roots; server certificate renewals under these roots need no firmware change.
