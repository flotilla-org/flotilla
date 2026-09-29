# Sandbox and isolation technologies as Flotilla fulfilment kinds

Survey date: 2026-09-29. Scope: candidate **fulfilment kinds** for ADR 0046 (least-privilege fulfilment), with emphasis on native macOS and Windows environments that are not Linux containers.

Grant vocabulary used below, from ADR 0046 §1 and §4: `platform`, `gui_session`, `gpu`, `host_devices`, `network:<scope>`, `host_account_reach`, `container_runtime`, plus `toolchain` and `harness` as host-observed facts. I also use two informal descriptors that are not in the ADR vocabulary, so the partial order can be discussed: **isolation** (process < shared-kernel container < gVisor < microVM ≈ VM) and **persistence** (ephemeral / suspendable / snapshot).

Confidence markers: **[verified]** means checked against a current primary or near-primary source in this session. **[uncertain]** means from memory or secondary sources and should be re-checked before anyone builds on it.

---

## 0. At a glance

| Option | Host | Guest | Isolation | Real GUI desktop | Snapshot / suspend | Licence |
|---|---|---|---|---|---|---|
| Apple Virtualization.framework (raw) | macOS, Apple silicon | macOS, Linux | full VM | yes (macOS guest) | save/restore of a running VM (macOS 14+) | Apple SDK |
| tart | macOS, Apple silicon | macOS, Linux | full VM (VZ) | yes (VNC or window) | `tart suspend` | Fair Source, being relicensed to OSS (2026) |
| Lima v2.1 (vz) | macOS, Linux | Linux, macOS (experimental) | full VM | macOS guest: yes | limited | Apache-2.0 (CNCF incubating) |
| Lume (cua) | macOS, Apple silicon | macOS, Linux | full VM (VZ) | yes, aimed at computer use | [uncertain] | MIT [uncertain] |
| UTM / VirtualBuddy | macOS | macOS, Linux, Windows ARM (UTM via QEMU) | full VM | yes | UTM: QEMU backend only [uncertain] | Apache-2.0 / BSD [uncertain] |
| Anka / Orka | macOS, Apple silicon | macOS | full VM (VZ) | yes | Anka "Instant Start" suspend | commercial |
| Parallels Desktop | macOS, Apple silicon | macOS, Linux, **Windows 11 ARM (MS-authorised)** | full VM | yes | yes (`prlctl snapshot`) | commercial |
| Seatbelt (`sandbox-exec`, srt) | macOS | host macOS | process sandbox (MAC) | host desktop, shared | none | built-in; srt Apache-2.0 |
| Separate macOS user account | macOS | host macOS | UID boundary | yes, if that user is logged in | none | built-in |
| Hyper-V VM | Windows Pro/Ent/Server | Windows, Linux | full VM | yes (Enhanced Session / RDP) | checkpoints (incl. memory) | built-in; guest needs a licence |
| Windows Sandbox (`wsb`) | Windows 11 Pro/Ent | Windows (same build as host) | lightweight VM | yes | **none**; one instance at a time | built-in |
| Windows containers (process / Hyper-V) | Windows, Windows Server | Windows Server Core / Nano | shared kernel / utility VM | **no** | image layers only | built-in |
| AppContainer / restricted token / sandbox user | Windows | host Windows | process sandbox | host desktop, shared | none | built-in |
| Microsoft MXC | Windows, Linux, macOS | host OS | policy layer over several backends | not initially | lifecycle only | MIT, early preview |
| dockur/windows | Linux + KVM (in Docker) | Windows desktop / Server | full VM (QEMU/KVM) in a container | yes (web VNC / RDP) | disk persistence; QEMU snapshots possible | MIT; Windows licence is on you |
| microsandbox | macOS (AS), Linux (KVM), Windows (WHP) | Linux (OCI images) | microVM (libkrun) | no (community demos only) | fork/snapshot | Apache-2.0, beta |
| Apple `container` 1.0 | macOS 26, Apple silicon | Linux | VM per container | no | none documented | Apache-2.0 |
| Docker Sandboxes (`sbx`) | macOS, Windows, Linux | Linux + private dockerd | microVM | no | [uncertain] | free, proprietary |
| libkrun / krunkit / krunvm | Linux (KVM), macOS (HVF) | Linux | microVM | GPU via Venus, no desktop | no | Apache-2.0 |
| SmolVM | macOS, Linux | Linux | microVM | no | no | [uncertain] |
| Firecracker | Linux + KVM | Linux | microVM | no | yes, mature | Apache-2.0 |
| Cloud Hypervisor | Linux (KVM/MSHV) | Linux, **Windows** | VM | via VFIO GPU / VNC-less | yes | Apache-2.0 / BSD |
| Kata Containers | Linux | Linux | VM per pod | no | limited | Apache-2.0 |
| gVisor | Linux | Linux | user-space kernel | no | `runsc checkpoint` | Apache-2.0 |
| bubblewrap / nsjail / Landlock | Linux | host Linux | process sandbox | host X/Wayland if passed | none | OSS |
| OrbStack | macOS | Linux | shared-kernel machines in one VM | no | no | commercial for work use |
| e2b | cloud, or self-host | Linux | Firecracker | Ubuntu desktop over VNC | pause/resume incl. memory | Apache-2.0 infra + SaaS |
| Daytona | cloud | Linux, **Windows**, macOS | VM | yes (computer use) | snapshots | AGPL core + SaaS [uncertain on licence] |
| Modal Sandboxes | cloud | Linux | gVisor, or VM (beta) | no | filesystem + memory (alpha) | SaaS |
| Fly Machines / Sprites | cloud | Linux | Firecracker | no | Sprites: filesystem checkpoints in ~1 s | SaaS |
| Cloudflare Sandboxes | cloud | Linux | VM per sandbox | no | snapshot-based session recovery | SaaS |
| Windows 365 for Agents | cloud | Windows | Cloud PC (VM) | yes | reset on check-in | SaaS, preview, US only |
| EC2 Mac / use.computer | cloud | macOS | bare metal, or VM on it | yes | AMI / VM images | SaaS, 24-hour minimum |

---

## 1. Native macOS

### 1.1 Apple Virtualization.framework (VZ): the substrate
- **What:** Apple's high-level VM API on top of Hypervisor.framework. tart, Lume, Lima (vz), UTM (Apple backend), VirtualBuddy, Anka 3, Orka on Apple silicon, Docker `sbx`, and Apple `container` all sit on it.
- **Platform / guest:** macOS host. On Apple silicon it runs macOS and Linux guests. **No Windows guests**: Windows on ARM needs QEMU (UTM) or Parallels.
- **Isolation:** full hardware VM.
- **GUI:** real macOS desktop in the guest (`VZVirtualMachineView`, or VNC through wrappers). A GPU is paravirtualised for macOS guests, which is enough for Metal apps. No GPU passthrough.
- **Startup / density:** macOS guest cold boot is tens of seconds; restore from saved state is seconds. Density is capped at **2 concurrent macOS guests per host**. The cap is enforced in the XNU kernel through `hv_apple_isa_vm_quota` and matches the macOS SLA clause allowing "up to two (2) additional copies or instances". Bypassing it needs SIP disabled and a development kernel; see khronokernel below. That is not an option for a production fleet. **Linux guests are not subject to the cap** (eclecticlight, stated as the author's suspicion, and widely relied on). [verified]
- **Snapshot:** `saveMachineStateTo` and `restoreMachineStateFrom` (macOS 14+, Apple silicon only). The save file is consumed on restore because disk and memory must match. Disk snapshotting is left to the wrapper, for example APFS clones. [verified]
- **Nested virtualisation:** macOS 15+ on M3 or later, for Linux guests (KVM inside the guest). [verified]
- **Network:** NAT, bridged, or file-handle attachment, so userspace filters such as tart's Softnet can sit in the path.
- **Licence / cost:** the framework is free. The macOS SLA permits VMs only on Apple-branded hardware and only for development, testing, macOS Server, or personal non-commercial use. Flotilla's agent development work plausibly fits "software development"; the lawyerly reading is yours to make [uncertain].
- **Grant mapping:** `platform: macos`, `gui_session: yes`, `gpu: paravirt (Metal)`, `host_account_reach: none`, `container_runtime: no` (Linux containers only with nested virtualisation on M3+ inside a Linux guest), `network: nat | softnet | none`.
- Sources: https://developer.apple.com/documentation/virtualization/vzvirtualmachine/restoremachinestatefrom(url:completionhandler:) · https://khronokernel.com/macos/2023/08/08/AS-VM.html · https://eclecticlight.co/2022/08/04/virtualisation-on-apple-silicon-macs-8-how-apple-limits-vms/ · https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationenabled

### 1.2 tart (Cirrus Labs, now under OpenAI)
- **What:** CLI for macOS and Linux VMs on VZ. VM images are pushed to and pulled from OCI registries. Orchard is its multi-host orchestrator. Heavily used for macOS CI.
- **Isolation / GUI:** full VM. `tart run` gives a window or `--no-graphics` + VNC, which is a real Aqua desktop.
- **Startup / density:** local clone (APFS copy-on-write) is near-instant; boot takes tens of seconds; the 2-macOS-VM cap applies.
- **Snapshot:** `tart suspend` saves RAM, CPU and device state, provided the VM was started with `--suspendable`. This is the closest existing fit for dehydrate/rehydrate on macOS. [verified]
- **Network:** default NAT; `--net-softnet` (a userspace packet filter that blocks host access and ARP spoofing, needs passwordless sudo); `--no-network`; bridged. [verified]
- **Licence:** Fair Source: free on personal workstations, paid beyond 100 server CPU cores. Cirrus Labs joined OpenAI on 2026-04-07. Tart, Orchard and Vetu are to be "re-released under a more permissive open-source license" and fees dropped. Maintenance moves to the community, and MacStadium flags that as a risk. The tart.run licensing page still showed Fair Source tiers at survey time, so **the relicensing has been announced but I could not verify it has landed** [uncertain]. The repo now lives at github.com/openai/tart.
- **Maturity:** high, years of production CI use. Watch the maintainer transition.
- **Grant mapping:** the `tart-vm` example in ADR 0046 itself: `platform: macos`, `gui_session`, `host_account_reach: none`, `network: softnet-scoped`, persistence `suspendable`.
- Sources: https://github.com/cirruslabs/tart · https://tart.run/licensing/ · https://tart.run/faq/ · https://github.com/openai/softnet · https://macstadium.com/blog/cirrus-labs-is-joining-openai

### 1.3 Lima v2.x (vz driver, macOS guests)
- **What:** CNCF (incubating) VM launcher. v2.0 (Nov 2025) added plugins, a krunkit GPU driver, and an MCP server that exposes VM file and shell tools to agents. **v2.1 (Mar 2026) added experimental macOS guests** (`limactl start template:macos`) on vz, plus agent-safety hardening.
- **Platform / guest:** macOS host (vz, qemu, krunkit), Linux host (qemu). Linux guests; macOS guests experimental.
- **GUI:** macOS guests boot a desktop (random password written to a VM file). Linux guests are headless by default.
- **Snapshot:** QEMU driver has snapshots; vz snapshot support is not documented [uncertain].
- **Network:** user-mode networking (gvproxy), socket_vmnet, port-forward rules.
- **Licence:** Apache-2.0.
- **Grant mapping:** same as tart for macOS guests. For Linux guests it is a sturdier `docker-per-vessel` analogue on Mac hosts (`platform: linux`, `container_runtime` inside).
- **Caveat:** macOS guests are experimental, and the 2-VM cap applies. Lima's value here is governance: it is a CNCF project with an active vendor-neutral community, which is a hedge if tart's community handover stalls.
- Sources: https://www.cncf.io/blog/2026/03/25/lima-v2-1-macos-guests-and-enhanced-ai-agent-safety/ · https://www.cncf.io/blog/2025/12/11/lima-v2-0-new-features-for-secure-ai-workflows/ · https://lima-vm.io/docs/config/vmtype/krunkit/

### 1.4 Lume / Cua
- **What:** Cua's VZ-based CLI and daemon for macOS and Linux VMs, built for **computer-use agents**. It includes a macOS driver (`cua-driver-rs`, v0.12.x mid-2026) for background UI control, and `cua-fleet` for parallel sandboxes. lume reached v0.4.0. Cua also sells cloud sandboxes (Linux, Windows, macOS).
- **Fit:** the closest off-the-shelf match to a "mac verifier with `gui_session`" role that drives GUI apps. Setup Assistant automation is brittle across macOS releases (Cua's own issue #1215).
- **Licence:** open source, MIT per my memory [uncertain]. Maturity: young, moving fast.
- Sources: https://github.com/trycua/cua · https://cua.ai/docs · https://github.com/trycua/cua/issues/1215

### 1.5 UTM, VirtualBuddy
- **UTM:** QEMU plus an Apple VZ backend. It is the only free way to run **Windows 11 ARM** or x86 (emulated, slow) guests on a Mac. Scriptable via `utmctl` and AppleScript. Snapshots are on the QEMU backend only; macOS-guest snapshots were a long-open request (issue #5376). Apache-2.0. [partly uncertain]
- **VirtualBuddy:** a GUI app for macOS guests on VZ, oriented at trying macOS betas. It has little automation surface, so it is not a fulfilment candidate beyond manual use.
- Sources: https://github.com/utmapp/UTM/issues/5376 · https://en.wikipedia.org/wiki/UTM_(software) · https://github.com/insidegui/VirtualBuddy

### 1.6 Anka (Veertu) and Orka (MacStadium)
- **What:** commercial macOS VM platforms for iOS/macOS CI. Anka has a registry, "Instant Start" (suspended VMs resume on demand) and a controller. Orka 3 is Kubernetes-native, runs on-prem, on EC2 Mac, or in MacStadium's cloud, and adds RBAC, SSO and audit logs.
- **Isolation / GUI:** full VM with a desktop. Same 2-VM cap per host.
- **Cost:** quote-based, not public [verified absence]. MacStadium is openly positioning Orka as the supported replacement for tart after the OpenAI move.
- **Fit:** only if the fleet grows past what self-run tart or Lima can manage. For a small lab it is overkill.
- Sources: https://docs.veertu.com/anka/what-is-anka/ · https://macstadium.com/orka · https://docs.macstadium.com/orka/orka-overview/orka-overview

### 1.7 Parallels Desktop (Pro / Business)
- **What:** commercial hypervisor. It is the **only Microsoft-authorised way to run Windows 11 Pro/Enterprise ARM on Apple silicon** (M1–M5). It also runs macOS guests (VZ-based, with limits; see KB 128867) and Linux.
- **Automation:** `prlctl` (Pro edition) can create, start, stop, suspend, snapshot and delete VMs. Windows-guest snapshots are mature.
- **Fit:** gives a Mac host a `platform: windows` + `gui_session` fulfilment. That matters because Windows-on-ARM covers native ARM64 Windows builds, and x64 apps run under Prism emulation. For RAD Debugger-style x64 debugging, native x64 Windows is still preferable.
- Sources: https://www.parallels.com/products/desktop/microsoft-authorized-solution-windows-11-arm/ · https://kb.parallels.com/en/128867 · https://support.microsoft.com/en-us/windows/options-for-using-windows-11-with-mac-computers-with-apple-m1-m2-and-m3-chips-cd15fd62-9b34-4b78-b0bc-121baa3c568c

### 1.8 Seatbelt (`sandbox-exec`, Anthropic sandbox-runtime, Codex seatbelt)
- **What:** macOS kernel MAC framework (TrustedBSD) applied to an arbitrary process tree via an SBPL profile. `sandbox-exec` has been **deprecated for years but still works on macOS 26.x**. Apple uses the same machinery internally, and there is no supported replacement for sandboxing non-App-Store CLI processes (apple/containerization issue #737 asks for exactly that). [verified]
- **Users:** Claude Code's `/sandbox` and `@anthropic-ai/sandbox-runtime` (srt, beta research preview, Apache-2.0), Codex CLI, and MXC's Seatbelt backend.
- **Isolation:** process-level, shares the host kernel and user. Filesystem read/write allow and deny lists; network deny, or allow via a proxy (srt routes through a local proxy to get domain allowlists).
- **GUI:** a sandboxed process can still draw on the host user's desktop and talk to WindowServer unless the profile denies those Mach services. So it offers a GUI session, but not an isolated one.
- **Startup / density:** effectively zero cost; unlimited density.
- **Snapshot:** none.
- **Grant mapping:** `platform: macos`, `gui_session: shared-host` (if permitted), `host_account_reach: same-user but fs-scoped`, `network: allowlist-via-proxy`, isolation `process`. It sits just above host-direct in the partial order.
- **Caveats:** the deprecation risk is real but slow-moving. It is a weak boundary against kernel exploits. Profiles must also deny persistence paths (shell rc files, `~/.claude`, `.git/hooks`); srt ships sensible mandatory denies.
- Sources: https://code.claude.com/docs/en/sandbox-environments · https://github.com/anthropic-experimental/sandbox-runtime · https://github.com/apple/containerization/issues/737 · https://github.com/openai/codex/issues/215

### 1.9 App Sandbox
- Entitlement-based sandbox for **signed apps** (App Store, or Developer ID with `com.apple.security.app-sandbox`). Child processes inherit it. Arbitrary CLI agents and toolchains (git, cargo, compilers) are not practical to run under it. **Not a fulfilment candidate.** The one conceivable use is a signed Flotilla helper that launches agents.
- Source: https://developer.apple.com/documentation/security/app-sandbox

### 1.10 Separate macOS user account per vessel (or per pool)
- **What:** run the agent as a dedicated local user: `dscl`/`sysadminctl`-created, with an unreadable home for others, launched via `launchctl asuser` or `sudo -u`, and optionally wrapped in Seatbelt.
- **Isolation:** Unix UID and ACL boundary plus TCC. The kernel, network namespace and `/tmp` are shared.
- **GUI:** macOS supports several concurrent logged-in GUI sessions through fast user switching. A second user's session can be viewed over Screen Sharing, but only one session drives the physical console at a time. Headless GUI for a non-console user works via Screen Sharing, not reliably via plain SSH. [uncertain on current macOS 26 behaviour; test first]
- **Density:** high (bounded by RAM), and **not subject to the 2-VM cap**. That makes it the density answer for macOS GUI work.
- **Grant mapping:** `platform: macos`, `gui_session: dedicated-session` (if logged in; a host-observed fact per ADR 0046 §4), `host_account_reach: none (distinct user)`, `network: host` (use a pf anchor or a Seatbelt proxy to scope it), isolation `uid`.
- **Caveats:** TCC prompts (screen recording, accessibility) need MDM/PPPC profiles to pre-grant them. Keychain per user. Account setup needs admin. Codex's Windows "elevated sandbox" (§2.6) is the same pattern on Windows.

---

## 2. Native Windows

### 2.1 Hyper-V VMs
- **What:** Windows' type-1 hypervisor (Windows 11 Pro/Enterprise/Education, Windows Server). Full VMs, scriptable via the `Hyper-V` PowerShell module.
- **Guest:** any Windows (x64 on x64 hosts; ARM64 on ARM64 hosts) and Linux.
- **GUI:** real desktop via VMConnect **Enhanced Session Mode** (RDP over VMBus) or plain RDP into the guest.
- **GPU:** software (WARP / Basic Display) by default, which is enough for D3D11 apps under test, slowly. **GPU-P (partitioning) is supported only with a Windows Server 2025+ host**; Windows 11 hosts are unsupported, though community scripts exist. DDA passthrough is Server-only. [verified]
- **Startup / density:** boot takes tens of seconds. Differencing disks off a golden VHDX make clones cheap. Density is RAM-bound; dynamic memory helps.
- **Snapshot:** "standard" checkpoints capture memory and device state, "production" checkpoints are VSS-consistent disk only. Save-state is also available. **This is the most mature Windows dehydrate/rehydrate story.**
- **Network:** vSwitch (internal, external, private, Default Switch NAT), port ACLs (`Add-VMNetworkAdapterAcl`), or no adapter. Egress scoping is done with a guest firewall or host NAT rules.
- **Licence / cost:** the hypervisor is free with Pro. **Each Windows guest needs a licence.** Microsoft's free Windows 11 development-environment VMs have been unavailable since Oct 2024. Windows Enterprise evaluation ISOs (90 days) remain the unlicensed path. Enterprise E3/E5 per-user subscriptions include virtualisation rights [uncertain on the exact current terms].
- **Grant mapping:** `platform: windows`, `gui_session: yes`, `gpu: none | gpu-p (Server host)`, `host_account_reach: none`, `network: vswitch-scoped`, `container_runtime: optional` (Windows containers, or Docker with nested virtualisation), persistence `snapshot`.
- Sources: https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/gpu-partitioning · https://techcommunity.microsoft.com/blog/itopstalkblog/gpu-partitioning-in-windows-server-2025-hyper-v/4429593 · https://www.neowin.net/news/microsofts-official-windows-11-virtual-machines-are-no-longer-available/

### 2.2 Windows Sandbox (`wsb`)
- **What:** a disposable, lightweight Hyper-V-based Windows desktop that shares the host's read-only OS files. Windows 11 Pro/Enterprise.
- **Guest:** always the **same build as the host**.
- **Automation:** since 24H2 there is a `wsb` CLI: `start --config <xml>`, `list`, `exec`, `share`, `connect`, `ip`, `stop`, with JSON output via `--raw`. **Limits:** `exec` returns only the exit code, with **no stdout/stderr**. `exec` in the user context needs an active RDP session opened by `wsb connect`; otherwise it runs as `System`. [verified]
- **GUI:** yes, a real desktop. A vGPU option is available in config.
- **Density:** **one instance at a time per host**, per the Microsoft FAQ. [verified]
- **Snapshot:** none. Closing it discards everything.
- **Network:** config can disable networking entirely, and Group Policy can enforce that. There is no domain-level scoping.
- **Grant mapping:** `platform: windows`, `gui_session: yes`, `gpu: vgpu (opt)`, `host_account_reach: mapped-folders only`, `network: on | off`, persistence `ephemeral`, capacity `1/host`.
- **Caveats:** the single instance and missing I/O make it a poor general agent host. It fits an "install and smoke-test the built artefact" verifier step, with results written to a mapped folder. MXC wraps it as an experimental backend.
- Sources: https://learn.microsoft.com/en-us/windows/security/application-security/application-isolation/windows-sandbox/windows-sandbox-cli · https://learn.microsoft.com/en-us/windows/security/application-security/application-isolation/windows-sandbox/windows-sandbox-faq · https://learn.microsoft.com/en-us/windows/security/application-security/application-isolation/windows-sandbox/windows-sandbox-configure-using-wsb-file

### 2.3 Windows containers (process isolation and Hyper-V isolation)
- **What:** Docker/containerd containers with Windows base images (Nano Server, Server Core, "Windows"). Process isolation shares the host kernel. Hyper-V isolation gives each container a utility VM.
- **Host:** Windows Server; Windows 10/11 (Hyper-V isolation is the default on client). Process isolation requires the host and image versions to match.
- **GUI:** **not supported.** Containers get a service console session only; GUI and RDP apps are out (microsoft/Windows-Containers issue #611). [verified]
- **Fit:** good for **headless Windows build and test** (MSVC, cargo on msvc) at density, with Hyper-V isolation for a real boundary. No good for RAD Debugger-style GUI work.
- **Grant mapping:** `platform: windows`, `gui_session: no`, isolation `container | utility-vm`, `network: container-net`, `container_runtime: windows`.
- **Caveats:** big images (Server Core is GBs), servicing churn, and MSVC Build Tools licensing inside images.
- Sources: https://learn.microsoft.com/en-us/virtualization/windowscontainers/deploy-containers/version-compatibility · https://github.com/microsoft/Windows-Containers/issues/611

### 2.4 AppContainer / LPAC
- **What:** Windows' capability-based process sandbox (used by UWP, Edge and Chromium renderers). The token carries an AppContainer SID; the object namespace is isolated; file and registry access needs explicit ACEs or capabilities; network needs capabilities (`internetClient`, `privateNetworkClientServer`).
- **Fit for agents:** awkward. Toolchains expect broad filesystem and registry access, and every path needs an ACL grant. OpenAI evaluated it and built Codex's Windows sandbox on restricted tokens plus sandbox users instead (§2.6). MXC's ProcessContainer backend is the maintained way to reach this class of primitive.
- **GUI:** AppContainer windows share the user's desktop, with UIPI limiting their interaction [uncertain on the detail].
- **Grant mapping:** isolation `process`, `host_account_reach: acl-scoped`, `network: on|off by capability`.
- Sources: https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation · https://codex.danielvaughan.com/2026/07/18/codex-cli-windows-sandbox-architecture-powershell-ast-safety-elevated-unelevated-appcontainer-restricted-tokens/

### 2.5 Microsoft Execution Containers (MXC), new in 2026
- **What:** Microsoft's open-source (MIT, Rust with a TypeScript SDK) **policy layer for agent containment**, announced at Build 2026, v0.6–0.8 in early preview. One JSON policy (filesystem, network, UI and clipboard) maps onto backends. **Stable:** ProcessContainer (Windows), IsolationSession (Windows, separate user or session), WSLC, Bubblewrap, LXC, Seatbelt. **Experimental:** Windows Sandbox, Hyperlight microVM, NanVix microVM. GitHub Copilot's CLI uses it.
- **Status caveat:** the README says **"no MXC profiles should be treated as security boundaries currently"**. Session isolation initially supports **non-interactive sessions only**, so no GUI yet. [verified]
- **Planned:** microVMs, WSL-based Linux containers, and Windows 365 for Agents integration.
- **Fit:** strategically interesting as a cross-platform abstraction whose shape resembles fulfilment kinds (one policy, many backends). Not a security boundary today. Worth tracking; for Windows non-GUI process sandboxing it may be the least-effort route once it matures.
- Sources: https://github.com/microsoft/mxc · https://blogs.windows.com/windowsdeveloper/2026/06/02/windows-platform-security-for-ai-agents/ · https://www.infoworld.com/article/4215416/running-ai-agents-in-sandboxes-with-microsoft-execution-containers.html

### 2.6 Codex-style Windows sandbox (restricted tokens, sandbox users, ACLs, firewall)
- **What:** OpenAI's native Windows sandbox for Codex CLI (shipped 2026). The **elevated** mode uses dedicated low-privilege sandbox users, write-allow ACEs stamped on the workspace, deny-write on `.git` and `.codex`, and Windows Firewall rules per sandbox user for network control. A helper (`codex-command-runner.exe`) calls `CreateProcessAsUserW`. The **unelevated** fallback uses write-restricted tokens plus a synthetic `sandbox-write` SID.
- **Fit:** this is the proven design for a Windows `process` or `uid`-isolation fulfilment kind with scoped network. It is the Windows twin of §1.10. It is only available inside Codex; a Flotilla kind would reimplement the pattern or use MXC IsolationSession.
- Sources: https://learn.chatgpt.com/docs/windows/windows-sandbox · https://www.infoq.com/news/2026/06/codex-windows-sandbox-design/ · https://codex.danielvaughan.com/2026/05/14/codex-cli-windows-sandbox-engineering-restricted-tokens-acls-elevated-architecture/

### 2.7 dockur/windows: a Windows desktop inside a Linux Docker container
- **What:** an OCI image that runs QEMU/KVM, downloads Windows media, installs unattended, and exposes a **browser VNC viewer (port 8006) and RDP**. Supports Windows 11/10/Server editions.
- **Host:** **Linux with `/dev/kvm`**, which is today's feta/udder docker fleet, or Windows 11 with nested virtualisation.
- **Isolation:** full VM, wrapped in a container.
- **Snapshot:** persistent `/storage` volume. QEMU `savevm` and qcow2 snapshots are possible but not first-class in the image [uncertain].
- **Licence:** MIT for the tooling. **Windows licensing is the operator's problem**: evaluation or unactivated media is fine for testing, and production use needs licences.
- **Fit:** **the cheapest path to `platform: windows` + `gui_session` on the existing Linux fleet**, reusing docker-per-vessel machinery (image, volume, network, reaping). Density is RAM-bound (about 4–8 GB per Windows guest).
- Sources: https://github.com/dockur/windows · https://hub.docker.com/r/dockurr/windows

### 2.8 Hyperlight (Microsoft, CNCF sandbox)
- An embeddable micro-VMM (KVM, MSHV, WHP) running **function-sized guests with no OS**, in 1–2 ms. Not an agent host. Relevant only as an MXC backend for running tool snippets.
- Sources: https://github.com/hyperlight-dev/hyperlight · https://opensource.microsoft.com/blog/2025/02/11/hyperlight-creating-a-0-0009-second-micro-vm-execution-time/

### 2.9 WSL containers (WSLC), WSL-adjacent
- `wslc.exe` / `container.exe`: Linux containers in WSL without Docker Desktop. Public preview (WSL 2.9.3, Build 2026), GA targeted for fall 2026, GPU on day one. **Linux guest**, so it is out of the "native Windows" scope. It gives Windows hosts a `docker-per-vessel` analogue for Linux roles.
- Sources: https://learn.microsoft.com/en-us/windows/wsl/wsl-container · https://devblogs.microsoft.com/commandline/wsl-container-is-now-available-for-public-preview/

### 2.10 Microsoft Dev Box, Windows 365, Windows 365 for Agents (cloud)
- **Dev Box is in maintenance mode, closing down from 2026-09-14 and retiring 2028-09-18.** Microsoft points customers at Windows 365. Do not build on it. [verified]
- **Windows 365 for Agents:** Cloud PC pools that agents check out and in (reset before reuse), with Computer-Do/See/TakeControl APIs. **Public preview, US only.** $0.40/hour pay-as-you-go, or $5 per Cloud PC per month always-available plus usage. Real Windows desktop, strong isolation, metered.
- **Fit:** a metered-tier `platform: windows` + `gui_session` kind for burst capacity, with ADR 0046's Quartermaster placing it last among ties (owned, then included, then metered).
- Sources: https://learn.microsoft.com/en-us/azure/dev-box/dev-box-roadmap · https://learn.microsoft.com/en-us/azure/dev-box/dev-box-retirement-guide · https://learn.microsoft.com/en-us/windows-365/agents/pricing-paygo-always-available · https://www.microsoft.com/en-us/windows-365/agents

---

## 3. Cross-platform and Linux-guest field

### 3.1 microsandbox (superradcompany)
- **What:** self-hosted microVM runtime and SDK (Rust, TypeScript, Python, Go) on **libkrun**. Runs OCI images as microVMs with their own kernel. **Hosts: macOS Apple silicon (HVF), Linux (KVM), Windows (WHP).** Linux guests only.
- **Startup:** under 100 ms average boot claimed on M1. High density.
- **Snapshot:** fork, snapshot and restore; the hosted docs mention disk snapshots of stopped sandboxes. The repo claims snapshots of running state too [uncertain which is current].
- **Network:** policy allowlists. By default the public internet is reachable, and private, link-local and metadata ranges are blocked. The hosted product adds **destination-bound credential substitution** (the host swaps in real secrets only for allowlisted destinations), which parallels ADR 0044's delivery concerns.
- **Licence / maturity:** Apache-2.0, about 8.5k stars, README says "still beta… expect breaking changes". The microsandbox.dev cloud is in private beta.
- **GUI:** none (community graphics demos only).
- **Grant mapping:** `platform: linux`, isolation `microVM`, `gui_session: no`, `network: allowlist`, `host_account_reach: none`, persistence `snapshot`. It is a stronger-isolation alternative to docker-per-vessel on all three host OSes, which makes it a way to run **Linux coder roles on Mac and Windows hosts** at microVM strength.
- Sources: https://microsandbox.dev/ · https://github.com/superradcompany/microsandbox

### 3.2 libkrun / krunvm / krunkit / muvm
- Red Hat's library VMM (KVM on Linux, HVF on macOS ARM64). Networking uses **TSI** (vsock socket impersonation, no virtual NIC) or virtio-net with passt/gvproxy. krunkit adds **Venus/virtio-gpu Vulkan** on macOS and backs Podman machine and Lima's krunkit driver. The maintainer noted in July 2026 that GPU command protocols are unsuitable for untrusted workloads except the DRM native-context path. That matters if `gpu` is ever granted to a microVM kind. Apache-2.0.
- Sources: https://github.com/containers/libkrun · https://lima-vm.io/docs/config/vmtype/krunkit/

### 3.3 Apple `container` 1.0 (and the Containerization package)
- **What:** Apple's open-source (Apache-2.0) CLI. **One lightweight VM per Linux container** on VZ. 1.0 shipped 2026-06-09 and added `container machine`, a persistent Linux environment with the macOS user and home mapped in. Requires **macOS 26 on Apple silicon**.
- Linux guests only, no GUI, no documented snapshots. Cold start is reportedly slower than Docker Desktop.
- **Fit:** a first-party `docker-per-vessel` equivalent on Mac hosts with per-vessel VM isolation. Note that `container machine` maps the home directory by default, which grants `host_account_reach` unless that is turned off.
- Sources: https://github.com/apple/container · https://cloudnativenow.com/features/apple-ships-stable-1-0-of-its-native-container-tool-for-macos/

### 3.4 Docker Sandboxes (`sbx`)
- Standalone and free (GA 2026-01-30). A microVM per sandbox with its **own Docker daemon**. Runs on macOS (VZ), Windows and Linux (KVM). Ships agent presets (Claude Code, Codex, OpenCode, and others). Linux guest, no GUI.
- **Fit:** `platform: linux`, `container_runtime: yes (private)`, isolation `microVM`. Useful where a role needs Docker without host socket exposure. Its microVM API is undocumented; people have reverse-engineered it.
- Sources: https://www.docker.com/products/docker-sandboxes/ · https://www.docker.com/blog/why-microvms-the-architecture-behind-docker-sandboxes/

### 3.5 SmolVM (smol-machines, YC, launched 2026-04-17)
- A single static binary microVM runtime. **macOS (Hypervisor.framework) and Linux (libkrun).** 90–140 ms cold boot on an M3, 40–80 MB per instance, **no snapshot restore yet**. Young and unaudited. Linux guests. Licence not verified [uncertain]. It is in the same niche as microsandbox.
- Source: https://bex.co/blog/2026/09/02/smolvm-vs-firecracker-agent-sandbox-microvm

### 3.6 Firecracker
- AWS's KVM-only microVM (Linux hosts only, Linux guests, no GUI, minimal devices, the jailer for production). About 125 ms boots; **mature snapshot/restore** (tens of ms from a warm pool, but snapshots are tied to Firecracker version, CPU template and kernel). Apache-2.0. The substrate behind Lambda, Fly, e2b and Vercel Sandbox.
- **Fit:** the strongest-isolation Linux kind for feta/udder, a microVM-per-vessel upgrade over docker-per-vessel. Not relevant for macOS or Windows.
- Sources: https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md · https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md

### 3.7 Cloud Hypervisor
- A Rust VMM (KVM, MSHV). Unlike Firecracker it **runs Windows guests**, and v53 (Jul 2026) added nested Hyper-V support for WSL2 inside Windows guests. Snapshot/restore with an offloaded daemon and userfaultfd lazy restore; VFIO. Apache-2.0/BSD.
- **Fit:** a leaner Linux-host alternative to QEMU for **Windows-guest VMs on the Linux fleet**. The GUI would come over RDP from inside the guest, since there is no built-in display.
- Sources: https://www.cloudhypervisor.org/blog/cloud-hypervisor-v53.0-released/ · https://github.com/cloud-hypervisor/cloud-hypervisor

### 3.8 Kata Containers
- An OCI/CRI runtime that puts each pod or container in a lightweight VM (QEMU, Cloud Hypervisor, Firecracker, Dragonball). Linux hosts and guests. Swappable as a `runtimeClass`, and usable from Docker via containerd shims. Apache-2.0, mature (OpenInfra).
- **Fit:** the least-change route to VM isolation on the existing docker-per-vessel fleet: keep OCI images, swap the runtime.
- Source: https://katacontainers.io/ [not re-verified this session]

### 3.9 gVisor
- Google's user-space kernel (`runsc`, systrap or KVM platform). Linux only. Intercepts syscalls, so it is weaker than a VM but much stronger than runc. `runsc checkpoint/restore`. Apache-2.0. Backs Modal and GKE Sandbox.
- **Fit:** `docker-per-vessel` with isolation `gvisor`. Compatibility gaps show up with some toolchains and FUSE; Modal's VM sandboxes exist partly for that reason.
- Sources: https://gvisor.dev/ · https://modal.com/docs/guide/vm-sandboxes

### 3.10 bubblewrap, nsjail, Landlock
- **bubblewrap:** unprivileged namespace sandbox, used by Flatpak, Claude Code's Linux sandbox and srt, and Codex.
- **nsjail:** Google's namespaces, cgroups and seccomp-bpf jail with a config file.
- **Landlock:** an unprivileged LSM for filesystem and (since ABI v4) TCP bind/connect restriction, which a process applies to itself. Codex uses it on Linux.
- All three are Linux process-level, shared kernel, with zero startup cost. GUI is possible by passing through X or Wayland sockets, which weakens isolation.
- **Fit:** isolation `process` for Linux host-direct roles. The Linux twin of Seatbelt.
- Sources: https://github.com/containers/bubblewrap · https://github.com/google/nsjail · https://docs.kernel.org/userspace-api/landlock.html [not re-verified this session]

### 3.11 Lima / Colima / OrbStack (Linux on Mac)
- **Colima:** a Lima-based Docker/containerd runtime. A shared VM.
- **OrbStack:** one shared Linux VM. "Isolated machines" cut off host file and integration access but **share one kernel**. Commercial use costs $8 per user per month. macOS only.
- **Fit:** `docker-per-vessel`-like on Mac, with shared-kernel isolation. Weaker than Apple `container`, `sbx` or microsandbox for per-vessel boundaries.
- Sources: https://docs.orbstack.dev/machines/isolated · https://orbstack.dev/pricing

### 3.12 Hosted sandboxes (all Linux unless stated)
- **e2b:** Firecracker. **Pause/resume including memory** (about 4 s per GiB to pause, about 1 s to resume; paused sandboxes kept indefinitely, storage billed). A Desktop Sandbox gives Ubuntu over VNC. The infrastructure is Apache-2.0 and self-hostable. About $0.05/vCPU-hr and $0.016/GiB-hr.
  Sources: https://e2b.dev/docs/sandbox/persistence · https://blog.logrocket.com/comparing-ai-agent-sandbox-platforms-e2b-modal-daytona-and-more/
- **Daytona:** VM sandboxes with a dedicated kernel. **Windows sandboxes (July 2026, "provision in seconds", small/medium/large presets)** and macOS on dedicated Apple silicon (computer use via use.computer), plus GPU sandboxes. Computer-use API (mouse, keyboard, screenshots, recording).
  Sources: https://www.daytona.io/dotfiles/windows-sandboxes · https://www.daytona.io/docs/en/computer-use/
- **Modal Sandboxes:** gVisor by default; **VM sandboxes in beta** (real kernel, Docker-in-sandbox, systemd). Filesystem, directory and memory snapshots, with memory snapshots in alpha.
  Source: https://modal.com/docs/guide/vm-sandboxes
- **Fly Machines / Sprites:** Firecracker. **Sprites** are persistent agent computers whose disk survives sleep, with **copy-on-write filesystem checkpoints in about 1 s** and in-place restore. $0.07/CPU-hr; billed only while running.
  Source: https://fly.io/sprites
- **Cloudflare Sandboxes / Containers:** GA 2026-04-13. **A VM per sandbox**, active-CPU billing ($0.072 per active vCPU-hour), credential injection, PTY, snapshot-based session recovery.
  Sources: https://developers.cloudflare.com/changelog/post/2026-04-13-containers-sandbox-ga/ · https://blog.cloudflare.com/sandbox-ga/
- **macOS clouds:** **EC2 Mac** is bare metal on a Dedicated Host with a **24-hour minimum allocation**, a condition of Apple's SLA. **use.computer** offers VMs on M4 minis, reserved for 24 h or more, 2 VMs per Mac, warm VMs in under a second. **Cua Cloud** also sells them.
  Sources: https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-mac-instances.html · https://docs.use.computer/ · https://cua.ai/blog/introducing-cua-cloud-containers
- **Fit for all of these:** metered-tier kinds. They matter to Flotilla mainly for **Windows GUI (Daytona, Windows 365 for Agents) and macOS GUI (use.computer, Daytona, EC2 Mac) burst capacity**, where owned hardware is scarce. Each needs Flotilla to reach the agent over an API rather than over a host transport, and credential delivery goes through the provider's injection model.

---

## 4. Coverage analysis against ADR 0046 grant sets

### Well served
- **`platform: linux`, no GUI, any isolation level.** The field is crowded: process (bwrap, Landlock, nsjail), container (docker today), gVisor, microVM (Firecracker, Kata, microsandbox, `sbx`, Apple `container`), VM, and a dozen hosted services. Snapshots are mature (Firecracker, e2b, Modal, Sprites). This also works **on Mac and Windows hosts** via microsandbox, `sbx`, Apple `container` and WSLC, so Linux coder roles do not need Linux hosts.
- **`platform: linux` + `gui_session`.** Xvfb or a VNC desktop in any container or microVM; hosted by e2b Desktop, Daytona and Cua.
- **`platform: macos`, no GUI, weak isolation.** Seatbelt (srt, Codex) plus a separate user. Zero cost, unlimited density.
- **`platform: macos` + `gui_session`, strong isolation, low count.** tart, Lima, Lume, Anka or Orka on VZ. Good tooling, suspend/resume, Softnet network scoping.
- **`platform: windows`, no GUI, strong isolation.** Hyper-V-isolated Windows containers; process-level via a Codex-style sandbox user or MXC.

### Thin
- **macOS GUI at density.** Hard ceiling of **2 macOS VMs per Mac** (SLA plus kernel quota), and macOS may only run on Apple hardware. The only ways past it are more Macs, or dropping to a **per-vessel user account** on the host (weaker isolation, no VM snapshot). Cloud macOS inherits the same cap and adds a 24-hour minimum.
- **Windows GUI with strong isolation on owned hardware.** Hyper-V VMs work but need a Windows host, per-guest licences, and hand-built automation (no mature "tart for Windows" with registry images). Windows Sandbox is capped at one instance, has no stdout and no persistence. Windows containers have no GUI at all. **dockur/windows or Cloud Hypervisor on the Linux KVM fleet** is the pragmatic workaround, at the cost of licensing ambiguity.
- **Windows GUI + `gpu`.** GPU-P needs a Windows Server 2025 host. VMs on Windows 11 get WARP software rendering. Parallels on Mac gives paravirtual DirectX on ARM only.
- **Windows GUI on Mac hosts.** ARM64 Windows only (Parallels authorised; UTM unofficial). x64 GUI debugging (RAD Debugger) runs under emulation, which is poor.
- **macOS dehydrate/rehydrate.** VZ save/restore exists, but it is single-shot (the file is consumed) and needs matching disks. tart suspend and Anka Instant Start wrap it. No macOS equivalent of Firecracker-grade snapshot pools exists.
- **Cross-platform process sandbox with one policy.** MXC is the only attempt, and it is explicitly not a security boundary yet.
- **`network` scoping on macOS and Windows native.** Linux has netns, iptables and proxies. macOS relies on a Seatbelt proxy or pf anchors per user; Windows on firewall rules per sandbox user or vSwitch ACLs. Every kind will need its own network-scope implementation. Worth modelling `network:<scope>` so each kind can declare which scopes it can actually enforce.

### Modelling observations for ADR 0046
- **Isolation strength should be a grant axis.** The partial order needs something like `isolation: process < uid < container < gvisor < microvm < vm`. Otherwise "Seatbelt on kiwi" and "tart VM on kiwi" both grant `platform: macos` + `gui_session`, and the order cannot rank them.
- **`gui_session` wants sub-values:** `shared-host-desktop` (Seatbelt, AppContainer), `dedicated-session` (separate user), and `isolated-desktop` (VM). A GUI verifier's threat model differs between them.
- **Capacity is a hard per-host limit for macOS VMs (2) and Windows Sandbox (1).** Hosts should advertise these as live capacity facts (§4 of the ADR), not leave them to the Quartermaster's judgement.
- **Persistence (`ephemeral | suspendable | snapshot`) is a natural grant to add** now, ahead of dehydrate/rehydrate. Kinds differ sharply here (Windows Sandbox: none; tart: suspend; Hyper-V: checkpoints; Firecracker/e2b: memory snapshots).

---

## 5. Recommended first prototypes (macOS and Windows native)

1. **`tart-vm` on the Mac hosts (macOS + `gui_session`, VM isolation, suspendable).** It is already the ADR's own example, mature, has OCI images, `--suspendable` gives a dehydrate/rehydrate probe, and `--net-softnet` gives network scoping. Risk: maintainership moving from Cirrus Labs to OpenAI and the community, and a licence change announced but not yet visible on tart.run. Keep **Lima v2.1 macOS guests** (CNCF, Apache-2.0) as a named fallback on the same VZ substrate, and look at Lume if the verifier role needs a UI-driving layer.
2. **`mac-user-seatbelt`: a dedicated macOS user per vessel, wrapped in Seatbelt (srt-style profile).** This is the density answer the 2-VM cap forces. It covers macOS roles without GUI, and GUI roles where a dedicated logged-in session is acceptable, at near-zero cost and well above host-direct in the partial order. It is cheap to build, and it exercises `host_account_reach: none` and `network` scoping via a proxy. Verify the multi-session GUI behaviour and TCC pre-granting via PPPC first.
3. **`hyperv-vm` on a Windows x64 host (Windows + `gui_session`, VM, checkpoints).** The only owned-hardware, strongly isolated, real x64 Windows desktop with mature snapshots, and the right substrate for RAD Debugger-style verification. Build a small "tart-like" layer: a golden VHDX, differencing disks per vessel, standard checkpoints for suspend, and a vSwitch ACL for network scope. Settle guest licensing (evaluation versus Enterprise virtualisation rights) up front.
4. **`dockur-windows` on the Linux KVM fleet (Windows + `gui_session`, VM-in-container).** Reuses docker-per-vessel plumbing on feta/udder, gives browser VNC and RDP access, and needs no Windows host. It is the fastest route to a first Windows GUI fulfilment, and a useful foil for (3) on density and cost. Caveats: licensing, and heavier RAM per vessel. Cloud Hypervisor is the leaner successor if this sticks.
5. **`windows-sandbox-user` (Windows, no GUI, uid isolation, firewall-scoped network).** A Codex-style dedicated sandbox user with ACL-stamped workspace and per-user firewall rules, or MXC IsolationSession once it is trustworthy. This covers headless Windows build and test at density, so (3) and (4) are reserved for roles that actually need `gui_session`. Windows Sandbox (`wsb`) is **not** recommended as a general kind: it is single-instance, returns no stdout from `exec`, and keeps no state. It could serve a narrow "smoke-test the installer" step later.

For burst or overflow, use metered kinds: **Daytona Windows**, **Windows 365 for Agents** (preview, US only) and **use.computer / EC2 Mac** fit as the Quartermaster's last-resort ties, not as first prototypes.

---

## 6. Items flagged uncertain

- Whether the tart and Orchard relicensing has actually shipped (announced 2026-04-07; the licensing page still shows Fair Source tiers).
- Whether microsandbox snapshots cover running memory state or only stopped disk state (the repo and the hosted docs differ).
- UTM snapshot support for VZ-backed guests; the Lume and VirtualBuddy licences; SmolVM's licence.
- Concurrent GUI sessions for non-console users on macOS 26, driven headlessly.
- Current Windows virtualisation licence rights for Windows 11 Enterprise guests.
- The Kata, gVisor, bubblewrap, nsjail and Landlock descriptions come from established knowledge and were not re-fetched this session.
- The Apple SLA's "software development" clause as applied to agent crews is a legal reading, not a technical fact.
