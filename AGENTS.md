# Repository guide

Use the `ligmaSec/victory-royale` repository for issues and changes.

Follow the [code standards](docs/code-standards.md) and [asset policy](docs/asset-policy.md).
The [development guide](CONTRIBUTING.md) covers setup and checks. See the [architecture guide](docs/architecture.md)
for crate responsibilities, terminology, and the import-resolution order.

For title-specific compatibility work, check the affected behavior against the
user-supplied game build and the relevant sections of its platform specification
when available. Record the build, the observation, and what still needs a runtime
or hardware check. Run gameplay and hardware comparisons from separate copies
of the standard `7952987A` save. The ignored root `saves/` directory accepts a
local copy. Treat saves as game files and keep them out of commits, artifacts
and reports. Keep other game material and derived files outside the checkout.
Use synthetic inputs for committed regression tests.

When documenting a command, check its flags, defaults and prerequisites against
the entry point readers will use. A source checkout, an extracted build and a
hardware test kit have different entry points. For CI and release instructions,
check the workflow and the contents of the package it produces.
