# Store

A native software store for Omarchy, part of Nyx. Browse desktop apps, discover
curated Arch favorites, search AUR, install and remove apps, and review coordinated
system updates. Uses the shared Nyx theme, settings card and keyboard conventions.

```sh
curl -fsSL https://raw.githubusercontent.com/design-nexus/nyx-store/main/install.sh | bash
nyx-store
nyx-store --remove firefox    # the installed app a launcher belongs to, to remove it
```

The installer installs the UI to `~/.local` and asks through sudo to install the
privileged helper and Polkit policy to root-owned locations. Package actions then
use Polkit. Building alone does not install the helper or change any packages.

## Sources

Omarchy always wins for new packages with the same name. Updates keep their recorded
source, even when another source has a newer version. Every dependency in a prepared
transaction is checked too. Store blocks implicit removals, source substitutions,
and removal of protected system components.

Pacman does not reliably preserve original repository names. **Repositories →
Review sources** groups existing installations by candidate sources. Confirm the
suggestions you recognize to establish the baseline. They are not proven history.
AUR and locally built packages with no binary-repository candidate can be assigned
explicitly; unresolved packages cannot be updated by Store. Reinstallation or
external package changes invalidate stale records when the installed package stamp
changes. Identical external reinstalls within the same second cannot be distinguished
by pacman's metadata. Other package tools do not enforce Store's policies.

Added repositories are stored in `/var/lib/nyx-store/repositories.json`, separately
from `/etc/pacman.conf`. Signed packages and trusted keys are required. Repository
names identify sources; changing mirrors retains that identity. Disable a repository
to remove it from discovery; its installed packages retain their source records and
updates will stop rather than migrate them.

BlackArch has one entry in Repositories. Click **Setup** and authenticate once;
Store retrieves the keyring, verifies its detached signature against BlackArch’s
published primary signing-key fingerprints, installs only the keyring, populates
its trust and enables the Store source. **Disable** turns the source off again.
Setup does not run `strap.sh`, change pacman.conf, install tool groups, replace
unrelated packages or change Omarchy’s priority.

App icons come from Arch’s bundled artwork, including JPEG XL decoded and cached
as PNG. Screenshot downloads are decoded before caching; blocked upstream URLs
can fall back to Flathub’s mirrored artwork. This uses display metadata only and
does not add Flatpak support or alter any package’s source.

## Updates and builds

Repository installs include required coordinated system upgrades. Browsing and catalog refresh use a separate read-only view of installed packages.
Authorized package operations refresh and commit through pacman's real database and native lock.
Review the complete transaction before applying. Source conflicts stop the operation;
Store never forces dependency resolution or changes signature policy to get past them.

AUR recipes are downloaded from aur.archlinux.org, shown at a pinned Git commit, and
built as the current user with `makepkg`. Repository build dependencies are reviewed
first. Missing AUR dependencies must be installed through their own recipe review
before retrying the parent build. Split-package artifacts are reviewed together.
AUR recipes execute user-controlled code and can access your user's files. Build logs
are in `~/.cache/nyx-store/aur/<base>/nyx-build.log`. AUR updates are checked and
reviewed separately from binary repository updates.

Root-owned provenance and the durable pending transaction journal live in
`/var/lib/nyx-store/provenance.sqlite`. Failed commits are reconciled against the
actual installed database. Store uses libalpm’s native global pacman lock. Never delete a package lock while another package manager is running.
Cancellation is available before applying a reviewed transaction and during AUR
builds; commits are allowed to finish. Trusting a signing key modifies pacman's system trust database explicitly.

Home uses an editorial selection, not live usage rankings. Arch AppStream metadata
supplies additional descriptions and screenshots; refresh downloads its data package
without installing it. Libraries and system packages appear only with **Include
packages** in search or as dependencies in transaction reviews.

## Keyboard

| Key | Action |
| --- | --- |
| Ctrl+F | Search |
| Ctrl+B | Collapse sidebar |
| Alt+Left | Home |
| Ctrl+R | Refresh catalog |
| Ctrl+, | Settings |
| Ctrl+Shift+P | Command palette |
| Esc | Close settings or clear search |
| F1 / ? | Shortcuts |
| Ctrl+Q / Ctrl+W | Close, after any active transaction finishes |

## Development

```sh
cargo build -p nyx-store
cargo test -p nyx-store --lib
cargo run -p nyx-store --bin nyx-store -- --catalog-check
```

The catalog check only reads pacman's installed packages and writes Store's user
cache. Policy tests use fixtures and never install packages or modify repositories.
This app is GPL-3.0-or-later because it links the GPL libalpm Rust bindings.
