# Security Policy

## Supported versions

`neotop` ships from `main`. Only the latest tagged release receives
fixes; older tags are kept for historical reference and will not
receive backports unless the issue affects current `main`.

| Version               | Status                      |
| --------------------- | --------------------------- |
| `main`                | active                      |
| latest tag (`v0.x.y`) | active — fixes shipped here |
| older tags            | unsupported                 |

## Reporting a vulnerability

**Please do not open a public GitHub issue for security reports.**
Public disclosure before a fix is available puts every running
instance at risk.

Use one of the following private channels instead:

1. **GitHub Security Advisories** *(preferred)* — go to the
   [Security tab](https://github.com/nt2311-vn/neotop/security/advisories/new)
   and click **Report a vulnerability**. This routes the report
   to the maintainer privately and lets us collaborate on a fix
   in a draft advisory.
2. **Email** — send to the address in the maintainer's GitHub
   profile. PGP key on request. Subject prefix `[neotop-security]`
   so the message bypasses normal triage.

Please include:

- A short description of the issue and its impact.
- A minimal reproduction (process tree, sysfs nodes, or
  permission setup that triggers the bug).
- Affected version (`neotop --version` if installed; commit SHA
  if running from source).
- Linux distribution + kernel version (`uname -a`).

## Disclosure timeline

- **48 hours** — initial acknowledgement that the report was
  received.
- **7 days** — confirmation of severity assessment and
  reproduction status.
- **30 days** — target window for a fix landing in `main` and a
  patched release on the same minor line.
- **Coordinated disclosure** — once a fix ships, the advisory
  goes public on GitHub, the [RustSec database](https://rustsec.org),
  and the project changelog. Reporter is credited unless they
  prefer otherwise.

## Threat model

`neotop` reads `/proc`, `/sys`, and `/sys/kernel/debug/kvm`
(when available) and renders the data to a TUI. It does **not**:

- open network sockets,
- write to anywhere outside its own controlling terminal,
- exec child processes (other than the operator-initiated
  signal sends — `K` for `SIGTERM`, `Shift+K` for `SIGKILL`,
  always confirmed),
- load arbitrary code at runtime (NVML is the one dlopen and is
  scoped to NVIDIA management calls).

In-scope security concerns we'll fix:

- TOCTOU races between reading `/proc/<pid>/*` files that allow
  an attacker to race a wrong-process signal,
- buffer or numeric handling in our parsers that can panic /
  abort on adversarial sysfs / proc content,
- privilege escalation through any Linux capability requested
  by the binary (we currently request none),
- supply-chain attacks via compromised crates (see CI's
  `cargo audit`, `cargo deny`, and OpenSSF Scorecard runs).

Out-of-scope (kernel-side issues, please report upstream):

- vulnerabilities in `i915`, `amdgpu`, `kvm`, or other Linux
  kernel modules whose `/proc` / `/sys` surfaces we read.
