# Third-party notices

Canary is GPL-3.0-or-later (see `LICENSE`). It bundles or builds on the projects below.

## Bundled plugins

Unmodified official macOS builds, fetched by `scripts/fetch_plugins.sh` (pinned versions and SHA-256 sums) and copied into `Canary.app/Contents/PlugIns` by `scripts/bundle.sh`. Source code is at each project's repository.

| Plugin | Version | License | Copyright | Source |
| --- | --- | --- | --- | --- |
| Surge XT, Surge XT Effects | 1.3.4 | GPL-3.0 | Surge Synth Team and contributors | https://github.com/surge-synthesizer/surge |
| Dexed | 1.1.0 | GPL-3.0 | Pascal Gauthier (asb2m10) and contributors | https://github.com/asb2m10/dexed |
| OB-Xf | 1.0.3 | GPL-3.0 | Surge Synth Team, based on OB-Xd by Vadim Filatov (2DaT) | https://github.com/surge-synthesizer/OB-Xf |

Their factory data (Surge XT patches and wavetables, OB-Xf patches and themes) is installed by the fetch script into `~/Library/Application Support/Surge XT` and `~/Library/Application Support/Surge Synth Team/OB-Xf`.

## Vendored source

| Project | License | Where |
| --- | --- | --- |
| Celemony ARA_API 2.3 | Apache-2.0 | `third_party/ARA_API` |
