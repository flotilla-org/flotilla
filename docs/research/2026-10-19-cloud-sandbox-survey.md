# Cloud sandbox providers for vessels (summary)

Survey date: 2026-10-19. The full survey lives in the private repository [flotilla-org/sandbox-survey](https://github.com/flotilla-org/sandbox-survey). It covers 33 providers against 52 criteria, with 1,716 sourced findings, per-provider profiles, free-tier arithmetic, Jev scores and the tools that rebuild the tables. This page keeps the conclusions that bear on flotilla's environment providers ([ADR 0058](../adr/0058-environment-providers-own-lifecycle-and-runtime-capabilities.md), #3468, #3469, #3494).

## How providers were judged

Hosted providers only, on two questions:

1. **Does it give a vessel what it needs?** An image by digest (or another route to it), root with nftables TCP redirect, a resident in-vessel agent, egress enforced to our proxy, storage that outlives the lifetime cap, a documented API and a usage API.
2. **Is there cheap or free burst capacity?** A user may run three or four provider accounts and live on their free tiers.

Self-run options (Coder, GKE Sandbox, kubernetes-sigs/agent-sandbox, microsandbox) are not ranked. They reduce to renting KVM-capable compute, which is a host for our own Firecracker kind, not a sandbox provider.

## Best fit

Three late entrants, all trialled the same day, lead: **Runtime (0.79)**, **Tensorlake (0.77)** and **Islo (0.76)**. Each enforces egress against root from outside the guest, and the three differ on protocol:

- Runtime's guest has no network card, so its host proxy's allow list is the only way out.
- Tensorlake's allow list passes any protocol to an allowed host.
- Islo's gateway is HTTP(S)-only and re-signs TLS.

All three are young, so treat their lead as provisional.

Behind them the established field sits within noise (fit 0.69–0.75): Namespace, Modal, Daytona, E2B, Vercel Sandbox, Northflank, Sailboxes, AWS AgentCore. Choose between them on their specific gaps:

| Provider | Gap that decides it |
|---|---|
| Namespace | Lifetime cap per plan (3/5/24 h); a 2024 terms clause on processes running while the user is logged out. |
| E2B | 24 h runs need Pro ($150/mo); the account is blocked when credit runs out. |
| Vercel Sandbox | Images only from Vercel's registry, so ours must be mirrored. |
| Daytona | Sysbox container on a shared kernel; stop drops processes. Docs gate per-sandbox allowlists at Tier 3, but the trial account had one enforced on the free credit. |
| Modal | SDK-only API; 24 h cap; no pause. The VM runtime gives nftables and enforced allowlists, but Modal's resolver stays open. |
| Northflank | Egress allowlist on managed cloud is undocumented. |

Docker Cloud Sandboxes scored 0.63 after a hands-on probe; see below.

## Cheapest credible

Monthly cost for one 4 vCPU / 8 GB sandbox, run for 160 h (steady) and for 200 h of burst: Sailboxes $14/$11, Runtime $15/$17 (both active-CPU), Northflank $25/$27, DigitalOcean MicroVMs $26/$31, AWS AgentCore Instances $27/$32, Cloudflare $31/$37, Fly Machines $33/$37, Fly Sprites $36/$44, Namespace $42/$48. Active-CPU billing is taken at 30% busy.

## Free capacity per account

- **Recurring:** Modal 63 h a month, Sailboxes ~61 h ($5 credit with a card), Freestyle 50 h a month, Runtime 25 h at 2x4, all with enforced egress. Codespaces gives 30 h, but egress cannot be controlled.
- **One-off:** Docker 893 h ($250 promotion until 2026-10-31), AWS 618–1,236 h, Daytona 603 h (with an enforced `/32` allowlist in the trial), E2B 302 h, Superserve ~750 h at its 2x2 free maximum.
- **At the limit**, providers behave differently: hard stop (Freestyle, Vercel Hobby), account blocked (E2B), or overage to the card (Modal, AWS, Docker). Few expose remaining credit through an API, so placement must meter each account's allowance itself.

## Findings that shape the provider design

- **Usage APIs are weak across the field.** Only Runtime, Superserve, Morph, Koyeb and Railway expose usage, credit and quota well. Cost-based admission needs our own metering from create and stop events. The working assumption is our own accounting first, reconciled against provider usage APIs where they exist and against scraped billing pages where they don't.
- **Many providers are SDK-only** (Python or TypeScript, no documented REST or gRPC API). A TypeScript connector would cover them (#3494). Because the in-vessel agent is the runner (#3281), such a connector only needs control-plane operations: lifecycle, image preparation, network policy, volumes, loss causes, and usage or quota.
- **gVisor has no working nftables.** Where a provider defaults to gVisor (Modal, GKE's managed Agent Sandbox), vessels need its VM runtime.

## Docker Cloud Sandboxes, probed 2026-10-19

Checked with `sbx --cloud` on two scratch sandboxes:

- **Image:** a public image runs by digest (`--image-ref`). PID 1 is Docker's own vsock `/init`; a process started through exec stays resident.
- **Guest control:** root is available, and nftables nat REDIRECT works in the guest.
- **Egress:** enforced outside the guest, so root cannot bypass it. Allow rules are exact to host:port and IP:port. Ordinary TLS is not intercepted.
- **Stop/start** keeps memory and disk, and resumes in about 2 s.
- **Caution:** the account's deny-all default did not apply to sandboxes without their own rules. Fresh sandboxes reached everything until a sandbox-scoped rule existed. A Docker provider must always write explicit rules per sandbox and check the decision log.
- **Open:** private-registry pulls; whether `--on-timeout restart` passes the 24 h-from-creation ceiling; no usage API.

## Daytona, probed 2026-10-19

Checked with the daytona CLI on a new account, using the free credit:

- **Image:** a digest-pinned snapshot built in 17 s, and a sandbox from it was running in about 1 s, as root.
- **Isolation:** a Sysbox user-namespaced container on a shared host kernel. The VM class was not offered to this account.
- **Guest control:** sudo and nftables work, and nat REDIRECT carried traffic.
- **Egress:** `--network-allow-list <ip>/32` was enforced, with DNS blocked, despite the docs' Tier 3 gate.
- **Stop/start** keeps the disk but not processes, and `pause` is refused.

## Modal, probed 2026-10-19

Checked with the Python SDK (modal 1.6.2):

- **Runtimes:** gVisor has no nftables. `runtime='vm'` needed no request and boots Modal's own kernel, with root, every capability, nftables and a working REDIRECT.
- **Egress:** CIDR (`/32`) and domain allowlists are enforced outside the guest; flushing nftables as root does not bypass them. IPv6 egress is blocked.
- **Gap:** Modal's resolver keeps answering any name, so DNS stays a channel out.

## E2B, probed 2026-10-19

Checked with the Python SDK on a Hobby account:

- **Image:** a digest-pinned template built in 46 s, and sandboxes started in about 0.5 s.
- **Guest:** a Firecracker VM with root, nftables and a working REDIRECT.
- **Egress:** `deny 0.0.0.0/0 + allow <ip>` blocked everything else, DNS and IPv6 included. That is the cleanest egress result of the four providers tested. A domain allowlist leaves DNS open.
- **Pause and resume** keep memory and take under a second each.

## Namespace, probed 2026-10-19

- **Instance:** a plain 2x4 instance is a microVM with its own kernel, root, and `/dev/kvm`. That makes it a fit for crews that need nested VMs, such as Firecracker work.
- **Guest control:** a privileged container pulled our image by digest, and nftables plus REDIRECT work inside it.
- **Egress:** a `BLOCK` policy is enforced through DNS. Only addresses that Namespace's resolver returned for allowed domains are reachable; raw IPs, pinned IPs and unlisted subdomains are blocked. So the proxy must be reached by name.
- **Attaching a policy:** it applies only through the Compute API's `networkPolicy.egressPolicyTag`. `nsc create --experimental` silently ignored it.

## Vercel Sandbox, probed 2026-10-19

- **Guest:** a Firecracker VM, and `sudo` gives full root.
- **nftables:** works, but `redirect` fails because NAT redirect isn't supported. Use `dnat` to the sandbox's own IP instead.
- **Egress:** both allowlists are enforced outside the guest. A CIDR allowlist leaves Vercel's resolver answering any name; a domain allowlist filters DNS as well.

## Freestyle, Blaxel and Northflank, probed 2026-10-19

- **Freestyle:**
  - A VM with root, `/dev/kvm`, nftables and REDIRECT. It's the only provider with IPv6 egress by default.
  - `--no-internet` plus one firewall allow rule is exact to IP and port, and blocks DNS and IPv6.
  - Pause keeps memory.
- **Blaxel:**
  - Default egress is IPv6-only, through DNS64/NAT64. nftables NAT works on the default kernel.
  - The `proxy` firewall with `allowedDomains` is enforced, but through Blaxel's in-guest root proxy, so the crew must not be root.
- **Northflank:**
  - Image by digest works, but the container lacks `CAP_NET_ADMIN`, so there is no in-guest nftables.
  - A card is needed before any compute.

## Fly Sprites, probed 2026-10-19

- **Guest:** root with `NET_ADMIN`, and nftables plus REDIRECT work.
- **Egress:** the allowlist is set from outside and enforced through DNS. Raw IPs, pinned IPs and unlisted names are blocked, but ports are not scoped, so an allowed name is reachable on any port.

## Fly Machines, probed 2026-10-19

- **Image:** a public image runs straight from Docker Hub, pinned by digest.
- **Guest:** root with every capability; nftables, iptables, REDIRECT and TUN all work.
- **Egress:** IPv4 and IPv6 are open, and Fly has no provider-side allowlist.

## GitHub Codespaces, probed 2026-10-19

- **Guest:** passwordless sudo to root with every capability; nftables, REDIRECT and TUN work.
- **Nested VMs:** `/dev/kvm` is passed through and opens on the default image, so a Firecracker crew could run there.
- **Egress:** IPv4 is open with no provider allowlist; IPv6 is unreachable.
- **Billing:** a repo in an org with no Codespaces budget is refused (HTTP 402); personal free quota only covers repos billed to the user.

## AWS AgentCore Runtime, probed 2026-10-19

- **Image:** our arm64 image from ECR ran once it served `/ping` and `/invocations`.
- **Guest:** a 2 vCPU / 8 GiB microVM; root, but no `NET_ADMIN`, so nftables is refused. No `/dev/kvm` or TUN.
- **Egress:** PUBLIC mode is open over IPv4 (IPv6 unreachable). Control comes only from VPC mode, and a VPC runtime with no NAT or ECR/S3 endpoints never started, so the image pull needs a path from our VPC.
- **Sessions:** files persist between invocations of one session. Runtime creation is a one-off (about 5 minutes in VPC mode).

## DigitalOcean MicroVMs, probed 2026-10-19

- **Status:** public preview, Kansas City (MKC1) only; the trial account was capped at 1 running vCPU.
- **Guest:** an OCI image by digest runs in its own microVM. Root, but no `NET_ADMIN`, so nftables is refused. No KVM or TUN.
- **Lifecycle:** pause keeps memory and processes, and an exec resumed it in about 0.55 s. A clone from a checkpoint came up already running with its processes and installed packages.
- **Egress:** public mode is open. A VPC MicroVM has no internet without a NAT gateway, so a proxy Droplet in the VPC could be the only way out (not yet tested). Image pulls also go through the VPC, so a VPC MicroVM from a public image failed; cloning a public-mode checkpoint into the VPC worked.

## Runtime, probed 2026-10-19

- **Egress:** the sandbox has no network card. Everything leaves through Runtime's host proxy, whose allow list (domains, wildcards, IPs, CIDRs) held against root and can change while the sandbox runs. `--no-internet` overrides the allow list, and **a fork does not inherit the rules**.
- **Image and lifecycle:** our Dockerfile (Ubuntu by digest) built in about 60 s; a sandbox was ready in under 2 s; pause and fork kept running processes.
- **Billing:** active CPU plus memory, a usage API and spend caps, and 25 free hours a month at 2 vCPU / 4 GiB. US-east only.

## Islo, probed 2026-10-19

- **Guest:** a public Ubuntu image by digest, root with every capability (when `--run-as-user root` is passed on each call), nftables REDIRECT, and a working `/dev/kvm`.
- **Egress:** a deny-by-default gateway outside the guest held against root, allowing only github.com. It is HTTP(S)-only and transparent: raw TCP is refused even to allowed hosts, and TLS is re-signed by an Islo Gateway CA.
- **Lifecycle:** pause keeps memory and processes; snapshots keep the filesystem only.
- **Caveats:** $0.07 per vCPU-hour, Canada only, and the CLI copies your local git name and email into each sandbox without asking.

## Sailboxes, probed 2026-10-19

- **Guest:** root with every capability and nftables REDIRECT. A public Ubuntu image by digest came up in 13 s through the Python SDK (the CLI can't pick an image). No KVM.
- **Egress:** an allow-only policy, set on the running box, held against root: github.com got through and everything else was cut, including non-HTTP traffic to github.com.
- **Lifecycle and cost:** sleep released compute and kept a running process; the lowest rates in the survey, billed on CPU and memory actually used.

## Tensorlake, probed 2026-10-19

- **Guest:** an image imported from Ubuntu by digest in about 10 s; root with every capability and nftables REDIRECT. No KVM.
- **Egress:** an allow list changed on the running sandbox held against root. Unlike Islo and Sailboxes it passes any protocol to allowed hosts (SSH to github.com worked).
- **Limits:** the free tier is one 1 vCPU / 1 GB sandbox in 2 h sessions; sessions cap at 24 h unless on Pro ($250/month); suspend needs a sandbox named at creation.

## Superserve, probed 2026-10-19

- **Guest:** a template built from Ubuntu by digest in 37 s; sandboxes active about 0.2 s after create. Root with all capabilities, but the guest kernel is 4.14: no nftables, and iptables-legacy REDIRECT works.
- **Egress:** an allow list PATCHed on the running sandbox held against root. Domain rules pass only HTTP(S) matching Host/SNI; CIDR rules pass any protocol; there is a per-sandbox egress log.
- **Lifecycle:** pause/resume and snapshots keep memory; a fork from a snapshot came up with its process running and the source's network rules.
- **Usage:** a billing API reports usage and credit remaining. Free is capped at 2 vCPU / 2 GiB; Pro is $1,000/month.

## Not testable on 2026-10-19

Koyeb accepts signups but redirects to a Mistral "coming soon" page; Morph redirects new users to Blaxel sandboxes. Northflank needs a card before any compute.

## Follow-up

Where to rent hosts for the Firecracker kind: EC2 nested virtualisation (C8i/M8i/R8i) versus bare metal such as Hetzner AX, at our sizes.
